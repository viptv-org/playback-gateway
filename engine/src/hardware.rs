//! Optional GPU acceleration: Intel Quick Sync, and VAAPI (Intel iHD, AMD
//! radeonsi and other Mesa drivers). A real encode probe, not encoder-name
//! presence, establishes access; unsupported inputs retry on CPU under the
//! same reservation.
use super::*;

/// A 1.8 KB HEVC Main 10 HDR10 bitstream: 128x128, two frames, BT.2020/PQ
/// with mastering-display and content-light SEI. FFmpeg 5.1's tonemap_vaapi
/// refuses frames without mastering metadata, so only a real HDR10 frame
/// proves a tone-mapping path. Generated with:
/// `ffmpeg -f lavfi -i "testsrc2=size=128x128:rate=24,format=yuv420p10le,setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc:range=tv" -frames:v 2 -c:v libx265 -preset ultrafast -x265-params "profile=main10:hdr10=1:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,50):max-cll=1000,400:keyint=24:info=0:qp=40" -f hevc hdr10-probe.hevc`
const HDR10_PROBE: &[u8] = include_bytes!("../assets/hdr10-probe.hevc");

/// Intel VPP HDR10 -> BT.709. The brightness lift before tone mapping is
/// Jellyfin's default (VppTonemappingBrightness = 16): without it the iHD
/// curve renders measurably darker than the software mobius path.
const VPP_TONEMAP: &str = "procamp_vaapi=b=16,tonemap_vaapi=format=nv12:t=bt709:m=bt709:p=bt709";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pipeline {
    Copy,
    QsvDecode,
    QsvEncode,
    /// VAAPI decode, scale (and HDR10 tone mapping) and H.264 encode: decoded
    /// frames never leave the GPU.
    VaapiDecode,
    /// Software decode and CPU filters, then GPU tone mapping (when HDR) and
    /// VAAPI H.264 encode.
    VaapiEncode,
    Software,
}
impl Pipeline {
    pub(super) fn encoder(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::QsvDecode | Self::QsvEncode => "h264_qsv",
            Self::VaapiDecode | Self::VaapiEncode => "h264_vaapi",
            Self::Software => "libx264",
        }
    }
    pub(super) fn fallback(self) -> Option<Self> {
        match self {
            Self::QsvDecode => Some(Self::QsvEncode),
            Self::VaapiDecode => Some(Self::VaapiEncode),
            Self::QsvEncode | Self::VaapiEncode => Some(Self::Software),
            _ => None,
        }
    }
    pub(super) fn accelerated(self) -> bool {
        matches!(
            self,
            Self::QsvDecode | Self::QsvEncode | Self::VaapiDecode | Self::VaapiEncode
        )
    }
    pub(super) fn vaapi(self) -> bool {
        matches!(self, Self::VaapiDecode | Self::VaapiEncode)
    }
}

/// What the VAAPI device proved at startup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Vaapi {
    /// H.264 encoding for every transcode (only on an explicit VAAPI device).
    pub(super) encode: bool,
    /// HDR10 tone mapping on the VPP (Intel iHD; Mesa drivers lack it).
    pub(super) tonemap: bool,
    /// HDR tone mapping on Vulkan through libplacebo, for drivers without VPP
    /// tone mapping such as AMD radeonsi.
    pub(super) placebo: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Accel {
    pub(super) qsv: bool,
    pub(super) vaapi: Vaapi,
}

/// How HDR pictures become SDR on a VAAPI pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ToneMap {
    Vpp,
    Placebo,
    Software,
}

/// FFmpeg 5.1's tonemap_vaapi converts HDR10 (PQ) only; HLG keeps a CPU or
/// Vulkan tone mapper.
pub(super) fn tone_map(vaapi: Vaapi, hdr: Option<&str>) -> Option<ToneMap> {
    let transfer = hdr?;
    Some(if vaapi.tonemap && transfer == "smpte2084" {
        ToneMap::Vpp
    } else if vaapi.placebo {
        ToneMap::Placebo
    } else {
        ToneMap::Software
    })
}

/// Sources a GPU decoder takes as-is: 8-bit H.264 and 8/10-bit HEVC in a
/// size every supported generation decodes, with square pixels.
fn hardware_decodable(video: &ProbeStream, interlaced: bool) -> bool {
    !interlaced
        && matches!(
            (video.codec_name.as_deref(), video.pix_fmt.as_deref()),
            (Some("h264"), Some("yuv420p")) | (Some("hevc"), Some("yuv420p" | "yuv420p10le"))
        )
        && video.width.is_some_and(|w| w > 0 && w <= 4096)
        && video.height.is_some_and(|h| h > 0 && h <= 2304)
        && video
            .sample_aspect_ratio
            .as_deref()
            .is_none_or(|sar| sar == "1:1" || sar == "N/A")
}

/// `hdr` is the source transfer (PQ or HLG) when it needs tone mapping.
pub(super) fn plan(
    copy: bool,
    accel: Accel,
    video: &ProbeStream,
    hdr: Option<&str>,
    interlaced: bool,
) -> Pipeline {
    if copy {
        return Pipeline::Copy;
    }
    let decodable = hardware_decodable(video, interlaced);
    if hdr.is_some() {
        // CPU tone mapping of 4K HDR runs below realtime on a 6-core host;
        // a GPU tone mapper is preferred over every CPU path.
        match tone_map(accel.vaapi, hdr) {
            Some(ToneMap::Vpp) if decodable => return Pipeline::VaapiDecode,
            Some(ToneMap::Vpp) => return Pipeline::VaapiEncode,
            Some(ToneMap::Placebo) if accel.vaapi.encode => return Pipeline::VaapiEncode,
            _ => {}
        }
        return if accel.qsv {
            Pipeline::QsvEncode
        } else if accel.vaapi.encode {
            Pipeline::VaapiEncode
        } else {
            Pipeline::Software
        };
    }
    if accel.qsv {
        return if decodable {
            Pipeline::QsvDecode
        } else {
            Pipeline::QsvEncode
        };
    }
    if accel.vaapi.encode {
        return if decodable {
            Pipeline::VaapiDecode
        } else {
            Pipeline::VaapiEncode
        };
    }
    Pipeline::Software
}

pub(super) fn device_args(cmd: &mut Command, device: &std::path::Path) {
    cmd.arg("-init_hw_device")
        .arg(format!("qsv=viptv,child_device={}", device.display()))
        .args(["-filter_hw_device", "viptv"]);
}

pub(super) fn vaapi_device_args(cmd: &mut Command, device: &std::path::Path, decode: bool) {
    cmd.arg("-init_hw_device")
        .arg(format!("vaapi=viptv:{}", device.display()))
        .args(["-filter_hw_device", "viptv"]);
    if decode {
        cmd.args([
            "-hwaccel",
            "vaapi",
            "-hwaccel_device",
            "viptv",
            "-hwaccel_output_format",
            "vaapi",
        ]);
    }
}

/// Fit inside the output envelope without upscaling, rounded down to even.
fn fit(video: &ProbeStream, width: u32, height: u32) -> (u32, u32) {
    let iw = video.width.unwrap_or(width).max(2);
    let ih = video.height.unwrap_or(height).max(2);
    let ratio = (width as f64 / iw as f64)
        .min(height as f64 / ih as f64)
        .min(1.0);
    (
        ((iw as f64 * ratio) as u32 / 2 * 2).max(2),
        ((ih as f64 * ratio) as u32 / 2 * 2).max(2),
    )
}

pub(super) fn scale(video: &ProbeStream, width: u32, height: u32) -> String {
    let (w, h) = fit(video, width, height);
    // scale_qsv exposes padded green rows for cropped heights (e.g. 804) on
    // the deployed Intel/FFmpeg 5.x stack. VPP preserves the visible crop.
    format!("vpp_qsv=w={w}:h={h}:format=nv12")
}

/// The -vf chain for a VAAPI pipeline. `deinterlace` is the CPU deinterlacer
/// when needed; `software` is the complete CPU chain the software pipeline
/// would run (deinterlace, then HDR conversion or scaling).
pub(super) fn vaapi_filter(
    pipeline: Pipeline,
    tone_map: Option<ToneMap>,
    video: &ProbeStream,
    width: u32,
    height: u32,
    deinterlace: Option<&str>,
    software: &str,
) -> String {
    let (w, h) = fit(video, width, height);
    let scale = format!("scale_vaapi=w={w}:h={h}:format=nv12");
    let before = deinterlace
        .map(|filter| format!("{filter},"))
        .unwrap_or_default();
    match (pipeline, tone_map) {
        // The planner only keeps HDR frames on the GPU for VPP tone mapping.
        (Pipeline::VaapiDecode, Some(_)) => format!("{VPP_TONEMAP},{scale}"),
        (Pipeline::VaapiDecode, None) => scale,
        (_, Some(ToneMap::Vpp)) => format!("{before}format=p010le,hwupload,{VPP_TONEMAP},{scale}"),
        (_, Some(ToneMap::Placebo)) => format!(
            "{before}libplacebo=w={w}:h={h}:format=nv12:tonemapping=bt.2390:range=tv:color_primaries=bt709:color_trc=bt709:colorspace=bt709,setsar=1,hwupload"
        ),
        _ => format!("{software},format=nv12,hwupload"),
    }
}

/// Operator configuration only; no arbitrary FFmpeg device-expression injection.
fn valid_device(device: &std::path::Path) -> bool {
    device.to_str().is_some_and(|p| {
        p.starts_with("/dev/dri/renderD")
            && p.len() > 16
            && p[16..].bytes().all(|b| b.is_ascii_digit())
    })
}

/// Run one bounded FFmpeg capability check, optionally fed a sample on stdin.
async fn probe(ffmpeg: &std::path::Path, args: &[&str], input: Option<&'static [u8]>) -> bool {
    use tokio::io::AsyncWriteExt;
    let mut cmd = Command::new(ffmpeg);
    cmd.kill_on_drop(true)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .args(["-hide_banner", "-loglevel", "error"])
        .args(args);
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    let stdin = child.stdin.take();
    let result = timeout(Duration::from_secs(10), async {
        if let (Some(mut stdin), Some(bytes)) = (stdin, input) {
            // A failing filter may exit before reading everything.
            let _ = stdin.write_all(bytes).await;
        }
        child.wait().await
    })
    .await;
    if matches!(result, Ok(Ok(status)) if status.success()) {
        return true;
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
    false
}

pub(super) async fn usable(ffmpeg: &std::path::Path, device: &std::path::Path) -> bool {
    if !valid_device(device) {
        return false;
    }
    let init = format!("qsv=viptv,child_device={}", device.display());
    probe(
        ffmpeg,
        &[
            "-nostdin",
            "-init_hw_device",
            &init,
            "-filter_hw_device",
            "viptv",
            "-f",
            "lavfi",
            "-i",
            "color=size=128x128:rate=30",
            "-vf",
            "format=nv12,hwupload=extra_hw_frames=32",
            "-c:v",
            "h264_qsv",
            "-frames:v",
            "2",
            "-f",
            "null",
            "-",
        ],
        None,
    )
    .await
}

/// Probe exactly the filter chains `vaapi_filter` builds, on a real HDR10
/// frame. `encode` asks for general VAAPI transcoding (explicit VAAPI device);
/// a Quick Sync device is only checked for HDR tone mapping.
pub(super) async fn vaapi(
    ffmpeg: &std::path::Path,
    device: &std::path::Path,
    encode: bool,
) -> Vaapi {
    if !valid_device(device) {
        return Vaapi::default();
    }
    let init = format!("vaapi=viptv:{}", device.display());
    let device_args = ["-init_hw_device", &init, "-filter_hw_device", "viptv"];
    let hdr10 = |filter: String| {
        let mut args: Vec<String> = device_args.iter().map(|arg| arg.to_string()).collect();
        args.extend(
            [
                "-f",
                "hevc",
                "-i",
                "pipe:0",
                "-vf",
                &filter,
                "-c:v",
                "h264_vaapi",
                "-f",
                "null",
                "-",
            ]
            .map(str::to_owned),
        );
        args
    };
    let encode = encode && {
        let mut args = vec!["-nostdin"];
        args.extend(device_args);
        args.extend([
            "-f",
            "lavfi",
            "-i",
            "color=size=128x128:rate=30",
            "-vf",
            "format=nv12,hwupload",
            "-c:v",
            "h264_vaapi",
            "-frames:v",
            "2",
            "-f",
            "null",
            "-",
        ]);
        probe(ffmpeg, &args, None).await
    };
    let vpp = hdr10(format!("format=p010le,hwupload,{VPP_TONEMAP}"));
    let tonemap = probe(
        ffmpeg,
        &vpp.iter().map(String::as_str).collect::<Vec<_>>(),
        Some(HDR10_PROBE),
    )
    .await;
    let placebo = encode
        && !tonemap
        && {
            let args = hdr10(
            "libplacebo=format=nv12:tonemapping=bt.2390:range=tv:color_primaries=bt709:color_trc=bt709:colorspace=bt709,hwupload".into(),
        );
            probe(
                ffmpeg,
                &args.iter().map(String::as_str).collect::<Vec<_>>(),
                Some(HDR10_PROBE),
            )
            .await
        };
    Vaapi {
        encode,
        tonemap,
        placebo,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn stream(value: serde_json::Value) -> ProbeStream {
        serde_json::from_value(value).unwrap()
    }
    const QSV: Accel = Accel {
        qsv: true,
        vaapi: Vaapi {
            encode: false,
            tonemap: false,
            placebo: false,
        },
    };

    #[test]
    fn hardware_never_overrides_copy_and_preserves_complex_filters() {
        let video = stream(
            serde_json::json!({"codec_name":"hevc","pix_fmt":"yuv420p10le","width":3840,"height":2160,"sample_aspect_ratio":"1:1"}),
        );
        assert_eq!(plan(true, QSV, &video, None, false), Pipeline::Copy);
        assert_eq!(
            plan(false, Accel::default(), &video, None, false),
            Pipeline::Software
        );
        assert_eq!(plan(false, QSV, &video, None, false), Pipeline::QsvDecode);
        assert_eq!(
            plan(false, QSV, &video, Some("smpte2084"), false),
            Pipeline::QsvEncode
        );
        assert_eq!(plan(false, QSV, &video, None, true), Pipeline::QsvEncode);
        assert_eq!(
            scale(&video, 1920, 1080),
            "vpp_qsv=w=1920:h=1080:format=nv12"
        );
        assert_eq!(
            Pipeline::QsvDecode.fallback().and_then(Pipeline::fallback),
            Some(Pipeline::Software)
        );
        assert_eq!(Pipeline::Software.fallback(), None);
    }

    /// Production is Quick Sync plus the same device's VPP: SDR keeps the
    /// measured QSV path, HDR10 moves to VAAPI decode + VPP tone mapping,
    /// and HLG (unsupported by FFmpeg 5.1's tonemap_vaapi) stays on the CPU.
    #[test]
    fn quick_sync_hosts_tone_map_hdr10_on_the_vpp_and_keep_sdr_unchanged() {
        let accel = Accel {
            qsv: true,
            vaapi: Vaapi {
                encode: false,
                tonemap: true,
                placebo: false,
            },
        };
        let hevc = stream(
            serde_json::json!({"codec_name":"hevc","pix_fmt":"yuv420p10le","width":3840,"height":1600,"sample_aspect_ratio":"1:1"}),
        );
        let av1 = stream(
            serde_json::json!({"codec_name":"av1","pix_fmt":"yuv420p10le","width":3840,"height":1600}),
        );
        assert_eq!(plan(false, accel, &hevc, None, false), Pipeline::QsvDecode);
        assert_eq!(
            plan(false, accel, &hevc, Some("smpte2084"), false),
            Pipeline::VaapiDecode
        );
        // Undecodable on the GPU (AV1 on Gen 9.5) or interlaced: CPU decode,
        // GPU tone mapping and encode.
        assert_eq!(
            plan(false, accel, &av1, Some("smpte2084"), false),
            Pipeline::VaapiEncode
        );
        assert_eq!(
            plan(false, accel, &hevc, Some("smpte2084"), true),
            Pipeline::VaapiEncode
        );
        assert_eq!(
            plan(false, accel, &hevc, Some("arib-std-b67"), false),
            Pipeline::QsvEncode
        );
        assert_eq!(
            tone_map(accel.vaapi, Some("arib-std-b67")),
            Some(ToneMap::Software)
        );
        assert_eq!(
            Pipeline::VaapiDecode
                .fallback()
                .and_then(Pipeline::fallback),
            Some(Pipeline::Software),
            "a failing GPU tone mapper ends on the proven software path"
        );
        let filter = vaapi_filter(
            Pipeline::VaapiDecode,
            tone_map(accel.vaapi, Some("smpte2084")),
            &hevc,
            1920,
            1080,
            None,
            "unused",
        );
        assert_eq!(
            filter,
            "procamp_vaapi=b=16,tonemap_vaapi=format=nv12:t=bt709:m=bt709:p=bt709,scale_vaapi=w=1920:h=800:format=nv12"
        );
        let uploaded = vaapi_filter(
            Pipeline::VaapiEncode,
            Some(ToneMap::Vpp),
            &hevc,
            1920,
            1080,
            Some("bwdif=mode=send_frame:parity=auto:deint=all,setfield=prog"),
            "unused",
        );
        assert!(uploaded.starts_with("bwdif=mode=send_frame:parity=auto:deint=all,setfield=prog,format=p010le,hwupload,procamp_vaapi"));
        assert_eq!(Pipeline::VaapiDecode.encoder(), "h264_vaapi");
        assert!(Pipeline::VaapiEncode.accelerated());
    }

    /// An AMD (Mesa) VAAPI device: everything transcodes on the GPU, HDR is
    /// tone mapped by libplacebo after a CPU decode, and without any GPU tone
    /// mapper the CPU chain is uploaded for the GPU encoder.
    #[test]
    fn vaapi_hosts_transcode_on_the_gpu_with_vulkan_tone_mapping() {
        let accel = Accel {
            qsv: false,
            vaapi: Vaapi {
                encode: true,
                tonemap: false,
                placebo: true,
            },
        };
        let h264 = stream(
            serde_json::json!({"codec_name":"h264","pix_fmt":"yuv420p","width":1920,"height":804,"sample_aspect_ratio":"1:1"}),
        );
        let anamorphic = stream(
            serde_json::json!({"codec_name":"h264","pix_fmt":"yuv420p","width":720,"height":576,"sample_aspect_ratio":"16:15"}),
        );
        let hevc = stream(
            serde_json::json!({"codec_name":"hevc","pix_fmt":"yuv420p10le","width":3840,"height":2160}),
        );
        assert_eq!(
            plan(false, accel, &h264, None, false),
            Pipeline::VaapiDecode
        );
        assert_eq!(
            plan(false, accel, &anamorphic, None, false),
            Pipeline::VaapiEncode
        );
        assert_eq!(
            plan(false, accel, &hevc, Some("smpte2084"), false),
            Pipeline::VaapiEncode
        );
        assert_eq!(
            plan(false, accel, &hevc, Some("arib-std-b67"), false),
            Pipeline::VaapiEncode,
            "libplacebo also converts HLG"
        );
        assert_eq!(
            vaapi_filter(
                Pipeline::VaapiDecode,
                None,
                &h264,
                1920,
                1080,
                None,
                "unused"
            ),
            "scale_vaapi=w=1920:h=804:format=nv12"
        );
        let placebo = vaapi_filter(
            Pipeline::VaapiEncode,
            tone_map(accel.vaapi, Some("smpte2084")),
            &hevc,
            1920,
            1080,
            None,
            "unused",
        );
        assert!(placebo.starts_with("libplacebo=w=1920:h=1080:format=nv12:tonemapping=bt.2390"));
        assert!(placebo.ends_with(",setsar=1,hwupload"));
        let software = "zscale=transfer=linear:npl=100,format=yuv420p";
        assert_eq!(
            vaapi_filter(
                Pipeline::VaapiEncode,
                tone_map(
                    Vaapi {
                        encode: true,
                        tonemap: false,
                        placebo: false
                    },
                    Some("smpte2084")
                ),
                &hevc,
                1920,
                1080,
                None,
                software,
            ),
            format!("{software},format=nv12,hwupload")
        );
        assert_eq!(
            plan(
                false,
                Accel {
                    qsv: false,
                    vaapi: Vaapi {
                        encode: true,
                        tonemap: false,
                        placebo: false
                    }
                },
                &hevc,
                Some("smpte2084"),
                false
            ),
            Pipeline::VaapiEncode
        );
        assert_eq!(tone_map(accel.vaapi, None), None);
    }

    #[test]
    fn only_render_nodes_are_accepted_as_devices() {
        for good in ["/dev/dri/renderD128", "/dev/dri/renderD129"] {
            assert!(valid_device(std::path::Path::new(good)), "{good}");
        }
        for bad in [
            "/dev/dri/renderD",
            "/dev/dri/card0",
            "/dev/dri/renderD128,driver=iHD",
            "vaapi=x:/dev/dri/renderD128",
        ] {
            assert!(!valid_device(std::path::Path::new(bad)), "{bad}");
        }
    }

    /// The embedded sample really is HDR10 with the metadata tonemap_vaapi needs.
    #[test]
    fn embedded_probe_is_an_hevc_hdr10_bitstream() {
        assert!(HDR10_PROBE.len() < 4096);
        assert!(HDR10_PROBE.starts_with(&[0, 0, 0, 1]));
        // HEVC SEI prefix NAL (type 39) carrying mastering display (137) and
        // content light level (144) payloads.
        let sei = HDR10_PROBE
            .windows(6)
            .filter(|w| w[..4] == [0, 0, 1, 0x4e] && w[4] == 1)
            .count();
        assert!(sei > 0, "no SEI prefix NAL");
        assert!(HDR10_PROBE.windows(2).any(|w| w == [137, 24]));
        assert!(HDR10_PROBE.windows(2).any(|w| w == [144, 4]));
    }
}
