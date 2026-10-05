use super::*;

#[cfg(unix)]
fn scripted(root: &std::path::Path, probe: &str, fails_on: &str) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let ffprobe = root.join("probe");
    std::fs::write(&ffprobe, format!("#!/bin/sh\nprintf '%s' '{probe}'\n")).unwrap();
    let ffmpeg = root.join("encoder");
    std::fs::write(
        &ffmpeg,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$0.attempts"
case "$*" in
 *"{fails_on}"*) exit 1 ;;
esac
for last do :; done
dir=${{last%/*}}
printf x > "$dir/segment-000000000.ts"
printf '#EXTM3U\n#EXTINF:1,\nsegment-000000000.ts\n' > "$last"
"#
        ),
    )
    .unwrap();
    for path in [&ffprobe, &ffmpeg] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    (ffprobe, ffmpeg)
}

const HDR10_HEVC: &str = r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"hevc","profile":"Main 10","width":3840,"height":1600,"pix_fmt":"yuv420p10le","sample_aspect_ratio":"1:1","field_order":"progressive","color_transfer":"smpte2084","avg_frame_rate":"24000/1001","r_frame_rate":"24000/1001"},{"index":1,"codec_type":"audio","codec_name":"aac","profile":"LC","channels":2}],"format":{"format_name":"matroska,webm","duration":"10"}}"#;

#[cfg(unix)]
#[tokio::test]
async fn software_hdr_conversion_retains_encoder_color_metadata() {
    let root = tempfile::tempdir().unwrap();
    let (ffprobe, ffmpeg) = scripted(root.path(), HDR10_HEVC, "never-matches");
    let manager = PlaybackManager::new(Config {
        ffmpeg: ffmpeg.clone(),
        ffprobe,
        root: root.path().join("media"),
        max_sessions: 1,
        ttl: Duration::from_secs(60),
    });
    manager
        .filters
        .set(["zscale", "tonemap", "sidedata"].map(String::from).into())
        .unwrap();
    let result = manager
        .start_with_permit(
            "http://fixture.invalid/video".into(),
            HashMap::new(),
            0.0,
            Some(Capabilities {
                max_width: 1920,
                max_height: 1080,
                ..Default::default()
            }),
            false,
            false,
            None,
        )
        .await
        .unwrap();
    let attempts = std::fs::read_to_string(ffmpeg.with_extension("attempts")).unwrap();
    assert!(attempts.contains("-c:v libx264"));
    assert!(attempts
        .contains("-color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv"));
    assert!(attempts.contains("sidedata=mode=delete"));
    assert!(!attempts.contains("setparams="));
    assert!(manager.stop(&result.id).await);
    manager.shutdown().await;
}

/// A Quick Sync host whose VPP tone maps: HDR10 decodes and tone maps on the
/// GPU. When the GPU tone mapper fails for a source, the same reservation
/// retries with CPU decode + GPU tone mapping, then CPU tone mapping + QSV encode.
#[cfg(unix)]
#[tokio::test]
async fn failing_gpu_tone_mapping_keeps_a_qualified_qsv_encoder() {
    let root = tempfile::tempdir().unwrap();
    let (ffprobe, ffmpeg) = scripted(root.path(), HDR10_HEVC, "tonemap_vaapi");
    let manager = PlaybackManager::new_with_qsv(
        Config {
            ffmpeg: ffmpeg.clone(),
            ffprobe,
            root: root.path().join("media"),
            max_sessions: 1,
            ttl: Duration::from_secs(60),
        },
        Some("/dev/dri/renderD128".into()),
    );
    manager.qsv_ready.set(true).unwrap();
    manager
        .vaapi_ready
        .set(hardware::Vaapi {
            encode: false,
            tonemap: true,
            placebo: false,
        })
        .unwrap();
    manager
        .filters
        .set(["zscale", "tonemap", "sidedata"].map(String::from).into())
        .unwrap();
    let result = manager
        .start_with_permit(
            "http://fixture.invalid/video".into(),
            HashMap::new(),
            0.0,
            Some(Capabilities {
                max_width: 1920,
                max_height: 1080,
                ..Default::default()
            }),
            false,
            false,
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.video_mode, "encode");
    let attempts = std::fs::read_to_string(ffmpeg.with_extension("attempts")).unwrap();
    let attempts: Vec<_> = attempts.lines().collect();
    assert_eq!(attempts.len(), 3, "{attempts:?}");
    // 1: VAAPI decode, VPP tone mapping, VAAPI encode on the QSV render node.
    assert!(attempts[0].contains("-init_hw_device vaapi=viptv:/dev/dri/renderD128"));
    assert!(attempts[0].contains("-hwaccel vaapi"));
    assert!(attempts[0].contains(
        "procamp_vaapi=b=16,tonemap_vaapi=format=nv12:t=bt709:m=bt709:p=bt709,scale_vaapi=w=1920:h=800:format=nv12"
    ));
    assert!(attempts[0].contains("-c:v h264_vaapi -profile:v main -level:v 4"));
    // 2: CPU decode, uploaded P010, VPP tone mapping.
    assert!(!attempts[1].contains("-hwaccel"));
    assert!(attempts[1].contains("format=p010le,hwupload,procamp_vaapi"));
    // 3: CPU tone mapping still retains the qualified Quick Sync encoder.
    assert!(attempts[2].contains("zscale=transfer=linear"));
    assert!(attempts[2].contains("h264_qsv"));
    assert!(attempts[2].contains("-init_hw_device qsv=viptv,child_device=/dev/dri/renderD128"));
    for attempt in &attempts {
        assert!(
            attempt.contains(
                "setparams=range=limited:color_primaries=bt709:color_trc=bt709:colorspace=bt709"
            ),
            "GPU HDR frames must carry SDR metadata: {attempt}"
        );
        assert!(!attempt.contains("-color_primaries"), "output color options must not request an automatic software format conversion: {attempt}");
        assert!(attempt.contains("sidedata=mode=delete"));
        assert!(attempt.contains("-c:a copy"));
    }
    assert!(manager.stop(&result.id).await);
    manager.shutdown().await;
}

/// An explicit VAAPI device (e.g. AMD) transcodes SDR on the GPU and falls
/// back to CPU decode + upload, then software.
#[cfg(unix)]
#[tokio::test]
async fn vaapi_device_transcodes_sdr_on_the_gpu_with_cpu_fallback() {
    let root = tempfile::tempdir().unwrap();
    let (ffprobe, ffmpeg) = scripted(
        root.path(),
        r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"hevc","profile":"Main 10","width":3840,"height":2160,"pix_fmt":"yuv420p10le","sample_aspect_ratio":"1:1","field_order":"progressive","avg_frame_rate":"24/1","r_frame_rate":"24/1"},{"index":1,"codec_type":"audio","codec_name":"eac3","channels":6}],"format":{"format_name":"matroska,webm","duration":"10"}}"#,
        "-hwaccel vaapi",
    );
    let manager = PlaybackManager::new_with_hardware(
        Config {
            ffmpeg: ffmpeg.clone(),
            ffprobe,
            root: root.path().join("media"),
            max_sessions: 1,
            ttl: Duration::from_secs(60),
        },
        None,
        Some("/dev/dri/renderD128".into()),
    );
    manager
        .vaapi_ready
        .set(hardware::Vaapi {
            encode: true,
            tonemap: false,
            placebo: true,
        })
        .unwrap();
    let result = manager
        .start_with_permit(
            "http://fixture.invalid/video".into(),
            HashMap::new(),
            0.0,
            Some(Capabilities {
                max_width: 1920,
                max_height: 1080,
                ..Default::default()
            }),
            false,
            false,
            None,
        )
        .await;
    let attempts = std::fs::read_to_string(ffmpeg.with_extension("attempts")).unwrap();
    let result = result.unwrap_or_else(|error| panic!("{error}: {attempts}"));
    assert_eq!(
        (result.video_mode.as_str(), result.audio_mode.as_str()),
        ("encode", "encode")
    );
    let attempts = std::fs::read_to_string(ffmpeg.with_extension("attempts")).unwrap();
    let attempts: Vec<_> = attempts.lines().collect();
    assert_eq!(attempts.len(), 2, "{attempts:?}");
    assert!(attempts[0].contains("-vf scale_vaapi=w=1920:h=1080:format=nv12"));
    assert!(attempts[1].contains("fast_bilinear,setsar=1,format=nv12,hwupload -"));
    assert!(attempts[1].contains("-c:v h264_vaapi"));
    assert_eq!(manager.acceleration_status(), "vaapi");
    assert_eq!(manager.tone_mapping_status(), "libplacebo");
    assert!(manager.stop(&result.id).await);
    manager.shutdown().await;
}

/// Run with PLAYBACK_TEST_FFMPEG/PLAYBACK_TEST_FFPROBE and PLAYBACK_TEST_VAAPI_DEVICE
/// (a render node) on a machine with a GPU; skipped without the device.
/// Proves the real probes and a real HDR10 session: GPU tone mapping (VPP or
/// libplacebo) producing BT.709 H.264 from a PQ HEVC Main 10 source.
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires real FFmpeg/ffprobe binaries"]
async fn real_vaapi_hdr10_session_is_tone_mapped_on_the_gpu() {
    let Some(device) = std::env::var_os("PLAYBACK_TEST_VAAPI_DEVICE").map(PathBuf::from) else {
        return;
    };
    let ffmpeg =
        PathBuf::from(std::env::var("PLAYBACK_TEST_FFMPEG").expect("PLAYBACK_TEST_FFMPEG"));
    let ffprobe =
        PathBuf::from(std::env::var("PLAYBACK_TEST_FFPROBE").expect("PLAYBACK_TEST_FFPROBE"));
    let vaapi = hardware::vaapi(&ffmpeg, &device, true).await;
    assert!(vaapi.encode, "the device must encode H.264");
    assert!(
        vaapi.tonemap || vaapi.placebo,
        "the device must tone map on the GPU: {vaapi:?}"
    );
    let root = tempfile::tempdir().unwrap();
    let fixture = root.path().join("hdr10.mkv");
    let status = Command::new(&ffmpeg)
        .args([
            "-v", "error", "-f", "lavfi", "-i",
            "testsrc2=size=1280x720:rate=24,format=yuv420p10le,setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc:range=tv",
            "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "4",
            "-map", "0:v:0", "-map", "1:a:0", "-c:v", "libx265", "-preset", "ultrafast", "-x265-params",
            "profile=main10:hdr10=1:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,50):max-cll=1000,400:keyint=24:log-level=error",
            "-c:a", "aac",
        ])
        .arg(&fixture)
        .status()
        .await
        .unwrap();
    assert!(status.success());
    let bytes = tokio::fs::read(&fixture).await.unwrap();
    let router = axum::Router::new().route(
        "/hdr10.mkv",
        axum::routing::get(move || {
            let bytes = bytes.clone();
            async move { bytes }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hdr10.mkv", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let media = root.path().join("media");
    let wrapper = root.path().join("ffmpeg-wrapper.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.attempts\"\nexec \"{}\" \"$@\"\n",
            ffmpeg.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let manager = PlaybackManager::new_with_hardware(
        Config {
            ffmpeg: wrapper.clone(),
            ffprobe: ffprobe.clone(),
            root: media.clone(),
            max_sessions: 1,
            ttl: Duration::from_secs(60),
        },
        None,
        Some(device),
    );
    manager.vaapi_ready.set(vaapi).unwrap();
    let response = manager
        .start_with_permit(url, HashMap::new(), 0.0, None, false, false, None)
        .await
        .unwrap();
    assert_eq!(response.video_mode, "encode");
    let attempts = std::fs::read_to_string(wrapper.with_extension("sh.attempts")).unwrap();
    // The filter inspection is not a playback attempt.
    let attempts = attempts
        .lines()
        .filter(|line| line.contains(" -i "))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(attempts.lines().count(), 1, "no fallback: {attempts}");
    assert!(attempts.contains("-c:v h264_vaapi"));
    assert!(
        attempts.contains("tonemap_vaapi") || attempts.contains("libplacebo"),
        "{attempts}"
    );
    let dir = media.join(&response.id);
    let segment = dir.join("segment-000000000.ts");
    let output = Command::new(&ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,pix_fmt,color_transfer,color_primaries,color_space",
            "-of",
            "json",
        ])
        .arg(&segment)
        .output()
        .await
        .unwrap();
    let probed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let stream = &probed["streams"][0];
    assert_eq!(stream["codec_name"], "h264");
    assert_eq!(stream["pix_fmt"], "yuv420p");
    assert_eq!(stream["color_transfer"], "bt709");
    assert_eq!(stream["color_primaries"], "bt709");
    assert!(manager.stop(&response.id).await);
    manager.shutdown().await;
    server.abort();
    let _ = server.await;
}
