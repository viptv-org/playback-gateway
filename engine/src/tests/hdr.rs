use super::*;

#[test]
fn hdr_resize_precedes_expensive_float_filters() {
    let filter = hdr_filter(1280, 720);
    assert!(filter.starts_with("zscale=transfer=linear:npl=100,format=gbrpf32le,zscale=w='trunc(min(iw,min(1280,iw*720/ih))/2)*2':h='trunc(min(ih,min(720,ih*1280/iw))/2)*2'"));
    assert!(filter.find("format=gbrpf32le").unwrap() < filter.find("w=").unwrap());
    assert!(filter.find("w=").unwrap() < filter.find("primaries=bt709").unwrap());
    assert!(filter.find("format=gbrpf32le").unwrap() < filter.find("primaries=bt709").unwrap());
    assert!(
        filter.find("primaries=bt709").unwrap() < filter.find("tonemap=tonemap=mobius").unwrap()
    );
    assert!(filter.ends_with("format=yuv420p,sidedata=mode=delete,setsar=1"));
    assert!(!filter.contains("force_original_aspect_ratio"));
}

fn hdr_fixture_transform(transfer: &str) -> String {
    format!("zscale=transferin=bt709:primariesin=bt709:matrixin=bt709:transfer={transfer}:primaries=bt2020:matrix=bt2020nc:npl=100,format=yuv420p10le")
}

async fn hdr_test_command(command: &mut Command) -> std::process::Output {
    command.kill_on_drop(true);
    let output = timeout(Duration::from_secs(45), command.output())
        .await
        .expect("bounded FFmpeg test")
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[tokio::test]
#[ignore = "requires configured real FFmpeg zscale/tonemap; no network"]
async fn real_hdr_resize_linear_reference() {
    let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").unwrap();
    for transfer in ["smpte2084", "arib-std-b67"] {
        for fixture in [
            "nullsrc=s=128x96:r=1,geq=lum='if(mod(X,2),235,16)':cb=128:cr=128",
            "testsrc2=size=128x96:rate=1",
        ] {
            let production = hdr_filter(32, 24);
            let reference = format!(
            "zscale=transfer=linear:npl=100,format=gbrpf32le,zscale={}:filter=bilinear,zscale=primaries=bt709,tonemap=tonemap=mobius:desat=2,zscale=transfer=bt709:matrix=bt709:range=limited,format=yuv420p,sidedata=mode=delete,setsar=1",
            hdr_size(32, 24)
        );
            let mut pixels = Vec::new();
            for filter in [&production, &reference] {
                let mut command = Command::new(&ffmpeg);
                command
                    .args([
                        "-v",
                        "error",
                        "-nostdin",
                        "-filter_threads",
                        "2",
                        "-threads",
                        "2",
                        "-f",
                        "lavfi",
                        "-i",
                        fixture,
                        "-vf",
                    ])
                    .arg(format!("{},{filter}", hdr_fixture_transform(transfer)))
                    .args([
                        "-frames:v",
                        "1",
                        "-threads",
                        "2",
                        "-pix_fmt",
                        "rgb24",
                        "-f",
                        "rawvideo",
                        "pipe:1",
                    ]);
                pixels.push(hdr_test_command(&mut command).await.stdout);
            }
            assert_eq!(pixels[0].len(), 32 * 24 * 3);
            assert_eq!(pixels[0].len(), pixels[1].len());
            let mean_error = pixels[0]
                .iter()
                .zip(&pixels[1])
                .map(|(a, b)| a.abs_diff(*b) as f64)
                .sum::<f64>()
                / pixels[0].len() as f64;
            eprintln!("{transfer} fused vs explicit linear resize mean byte error={mean_error:.4}");
            assert!(
                mean_error <= 2.0,
                "linear-light resize mismatch: {mean_error}"
            );
        }
    }
}

#[tokio::test]
#[ignore = "requires configured real FFmpeg/ffprobe libx264 and HDR filters; no network"]
async fn real_hdr_resize_dimensions_and_sdr_tags() {
    let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").unwrap();
    let ffprobe = std::env::var("PLAYBACK_TEST_FFPROBE").unwrap();
    let root = tempfile::tempdir().unwrap();
    for (index, (iw, ih, w, h, expected_w, expected_h)) in [
        (3840, 1598, 1280, 720, 1280, 532),
        (3840, 1598, 1920, 1080, 1920, 798),
        (1080, 1920, 1280, 720, 404, 720),
        (320, 240, 1280, 720, 320, 240),
    ]
    .into_iter()
    .enumerate()
    {
        let path = root.path().join(format!("hdr-{index}.mp4"));
        let transfer = if index % 2 == 0 {
            "smpte2084"
        } else {
            "arib-std-b67"
        };
        let mut command = Command::new(&ffmpeg);
        command
            .args([
                "-v",
                "error",
                "-nostdin",
                "-filter_threads",
                "2",
                "-threads",
                "2",
                "-f",
                "lavfi",
                "-i",
            ])
            .arg(format!("testsrc2=size={iw}x{ih}:rate=1"))
            .arg("-vf")
            .arg(format!(
                "{},{}",
                hdr_fixture_transform(transfer),
                hdr_filter(w, h)
            ))
            .args([
                "-frames:v",
                "1",
                "-c:v",
                "libx264",
                "-threads",
                "2",
                "-preset",
                "ultrafast",
                "-color_trc",
                "bt709",
                "-color_primaries",
                "bt709",
                "-colorspace",
                "bt709",
            ])
            .arg(&path);
        hdr_test_command(&mut command).await;
        let mut command = Command::new(&ffprobe);
        command
            .args(["-v", "error", "-show_streams", "-of", "json"])
            .arg(&path);
        let output = hdr_test_command(&mut command).await;
        let data: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let video = &data["streams"][0];
        assert_eq!(video["width"], expected_w);
        assert_eq!(video["height"], expected_h);
        assert_eq!(video["pix_fmt"], "yuv420p");
        assert_eq!(video["sample_aspect_ratio"], "1:1");
        assert_eq!(video["field_order"], "progressive");
        for field in ["color_space", "color_transfer", "color_primaries"] {
            assert_eq!(video[field], "bt709");
        }
        assert!(!video["side_data_list"].to_string().contains("Mastering"));
    }
}

#[tokio::test]
#[ignore = "requires configured real FFmpeg/ffprobe libx264; no network"]
async fn real_hdr10_file_is_delivered_as_original_to_a_webcodecs_client() {
    // End-to-end proof of the reported failure: a real, genuinely tagged HDR10
    // file must reach a client that can demux and decode it. The old policy
    // refused this source before original delivery was ever considered.
    let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").unwrap();
    let ffprobe = std::env::var("PLAYBACK_TEST_FFPROBE").unwrap();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("hdr10.mp4");
    let mut command = Command::new(&ffmpeg);
    command
        .args([
            "-v", "error", "-nostdin", "-filter_threads", "2", "-threads", "2",
            "-f", "lavfi", "-i", "testsrc2=size=192x108:rate=24",
            "-frames:v", "4",
            "-vf", "format=yuv420p10le,setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc",
            "-c:v", "libx264", "-preset", "ultrafast", "-profile:v", "high10",
            "-pix_fmt", "yuv420p10le",
            "-color_trc", "smpte2084", "-color_primaries", "bt2020", "-colorspace", "bt2020nc",
        ])
        .arg(&path);
    hdr_test_command(&mut command).await;

    // Inspect it exactly as the server does.
    let mut command = Command::new(&ffprobe);
    command
        .args([
            "-v",
            "error",
            "-show_streams",
            "-show_format",
            "-of",
            "json",
        ])
        .arg(&path);
    let output = hdr_test_command(&mut command).await;
    let data: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let video = &data["streams"][0];
    assert_eq!(
        video["color_transfer"], "smpte2084",
        "fixture must be real HDR10"
    );
    assert_eq!(video["color_primaries"], "bt2020");

    let probe: Probe = serde_json::from_value(data.clone()).unwrap();
    // The transfer function alone decides HDR. A fully tagged HDR10 file is
    // inside the managed envelope (it gets tone-mapped), and so is the
    // partially tagged shape: ffprobe frequently reports the transfer
    // without primaries and matrix, and that shape used to be refused
    // outright before original delivery was ever considered.
    assert!(
        probe.ensure_supported().is_ok(),
        "a completely tagged HDR source is inside the managed envelope"
    );
    let mut untagged = data.clone();
    let fields = untagged["streams"][0].as_object_mut().unwrap();
    fields.remove("color_primaries");
    fields.remove("color_space");
    let partial: Probe = serde_json::from_value(untagged).unwrap();
    assert!(
        partial.ensure_supported().is_ok(),
        "a partially tagged HDR source must not be refused outright"
    );
    // A WebCodecs client that declares HDR-capable decoders receives the
    // original file for both shapes instead of an error.
    let declared: Capabilities = serde_json::from_value(serde_json::json!({
        "direct_play": true, "h264": true, "aac": true,
        "max_width": 3840, "max_height": 2160, "hevc": true,
        "direct_files": true,
        "direct_video_codecs": ["avc", "hevc", "av1"],
        "direct_audio_codecs": ["aac", "ac3", "eac3", "dts"]
    }))
    .unwrap();
    for (label, source) in [("fully tagged", &probe), ("partially tagged", &partial)] {
        let audio = source
            .streams
            .iter()
            .find(|stream| stream.codec_type.as_deref() == Some("audio"));
        assert_eq!(
            direct_format(source, &declared, audio, &TrackSelection::default()),
            Some("mp4"),
            "{label} HDR10 must be delivered as the original container"
        );
    }
}

#[tokio::test]
#[ignore = "requires real FFmpeg with libx264, zscale, tonemap and bwdif"]
async fn real_common_format_conversion() {
    let ffmpeg = PathBuf::from(std::env::var("PLAYBACK_TEST_FFMPEG").unwrap());
    let ffprobe = PathBuf::from(std::env::var("PLAYBACK_TEST_FFPROBE").unwrap());
    for kind in [
        "interlaced",
        "mpeg2-interlaced",
        "mpeg2-progressive",
        "smpte2084",
        "arib-std-b67",
    ] {
        let mpeg2 = kind.starts_with("mpeg2-");
        let interlaced = matches!(kind, "interlaced" | "mpeg2-interlaced");
        let hdr = matches!(kind, "smpte2084" | "arib-std-b67");
        let root = tempfile::tempdir().unwrap();
        let fixture = root
            .path()
            .join(if mpeg2 { "source.ts" } else { "source.mp4" });
        let mut generate = Command::new(&ffmpeg);
        generate.args([
            "-v",
            "error",
            "-filter_threads",
            "2",
            "-f",
            "lavfi",
            "-i",
            if mpeg2 {
                "nullsrc=s=320x240:r=50,geq=lum='if(lt(mod(X+N*8,32),16),235,16)':cb=128:cr=128"
            } else {
                "testsrc2=size=320x240:rate=60"
            },
            "-t",
            "2",
        ]);
        if interlaced {
            generate.args([
                "-vf",
                "tinterlace=mode=interleave_top",
                "-flags",
                "+ilme+ildct",
                "-field_order",
                "tt",
                "-pix_fmt",
                "yuv420p",
            ]);
        } else if hdr {
            generate.arg("-vf").arg(format!("zscale=transferin=bt709:primariesin=bt709:matrixin=bt709:transfer={kind}:primaries=bt2020:matrix=bt2020nc:npl=100,format=yuv420p10le"));
            generate.args([
                "-color_trc",
                kind,
                "-color_primaries",
                "bt2020",
                "-colorspace",
                "bt2020nc",
            ]);
        }
        if mpeg2 {
            generate.args([
                "-c:v",
                "mpeg2video",
                "-q:v",
                "2",
                "-threads",
                "2",
                "-f",
                "mpegts",
            ]);
        } else {
            generate.args([
                "-c:v",
                "libx264",
                "-threads",
                "2",
                "-preset",
                "ultrafast",
                "-movflags",
                "+faststart",
            ]);
        }
        let status = generate.arg(&fixture).status().await.unwrap();
        assert!(status.success(), "generate {kind}");
        let bytes = tokio::fs::read(&fixture).await.unwrap();
        let router = axum::Router::new().route(
            "/source.mp4",
            axum::routing::get(move || {
                let bytes = bytes.clone();
                async move { bytes }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/source.mp4", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let manager = PlaybackManager::new(Config {
            ffmpeg: ffmpeg.clone(),
            ffprobe: ffprobe.clone(),
            root: root.path().join("media"),
            max_sessions: 1,
            ttl: Duration::from_secs(60),
        });
        let source = manager.probe(&url, "", None).await.unwrap();
        if mpeg2 && interlaced {
            assert!(source.interlaced() || source.frame_interlace_detection_needed());
        } else {
            assert_eq!(source.interlaced(), interlaced);
        }
        assert_eq!(
            source.hdr_transfer().unwrap(),
            if hdr { Some(kind) } else { None }
        );
        let response = manager
            .start(url, HashMap::new(), 0.0, None, false)
            .await
            .unwrap();
        assert_eq!(response.mode, "transcode");
        let capability = response.url.split('/').nth(3).unwrap();
        let served = manager
            .serve(&response.id, capability, "index.m3u8")
            .await
            .unwrap();
        let served_playlist = axum::body::to_bytes(served.into_body(), 1 << 20)
            .await
            .unwrap();
        assert!(String::from_utf8(served_playlist.to_vec())
            .unwrap()
            .contains("#EXT-X-TARGETDURATION:2"));
        let dir = root.path().join("media").join(&response.id);
        let playlist = tokio::fs::read_to_string(dir.join("index.m3u8"))
            .await
            .unwrap();
        let segment = dir.join(
            playlist
                .lines()
                .find(|line| media_type(line) == Some("video/mp2t"))
                .unwrap(),
        );
        if mpeg2 {
            let mut expected = Command::new(&ffmpeg);
            expected.args(["-v", "error", "-i"]).arg(&fixture).args([
                "-vf",
                "bwdif=mode=send_frame:parity=auto:deint=interlaced,setfield=prog,setsar=1",
                "-frames:v",
                "1",
                "-pix_fmt",
                "rgb24",
                "-f",
                "rawvideo",
                "pipe:1",
            ]);
            let expected = hdr_test_command(&mut expected).await.stdout;
            let mut actual = Command::new(&ffmpeg);
            actual.args(["-v", "error", "-i"]).arg(&segment).args([
                "-frames:v",
                "1",
                "-pix_fmt",
                "rgb24",
                "-f",
                "rawvideo",
                "pipe:1",
            ]);
            let actual = hdr_test_command(&mut actual).await.stdout;
            assert_eq!(actual.len(), expected.len());
            let mse = actual
                .iter()
                .zip(&expected)
                .map(|(a, b)| (*a as f64 - *b as f64).powi(2))
                .sum::<f64>()
                / actual.len() as f64;
            assert!(
                mse < 25.0,
                "{kind} must match decoded-frame deinterlacing, MSE {mse}"
            );
        }
        let output = Command::new(&ffprobe)
            .args(["-v", "error", "-show_streams", "-of", "json"])
            .arg(&segment)
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let video = &value["streams"][0];
        assert_eq!(video["codec_name"], "h264");
        assert_eq!(video["pix_fmt"], "yuv420p");
        assert_eq!(video["field_order"], "progressive");
        assert!(conservative_frame_rate(video["r_frame_rate"].as_str()));
        if hdr {
            for field in ["color_transfer", "color_primaries", "color_space"] {
                assert_eq!(video[field], "bt709");
            }
            assert!(!video["side_data_list"].to_string().contains("Mastering"));
        }
        let decoded = Command::new(&ffmpeg)
            .args(["-v", "error", "-xerror", "-i"])
            .arg(&segment)
            .args([
                "-frames:v",
                "1",
                "-vf",
                "scale=32:24",
                "-pix_fmt",
                "rgb24",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .output()
            .await
            .unwrap();
        assert!(decoded.status.success(), "decode {kind}");
        assert_eq!(decoded.stdout.len(), 32 * 24 * 3);
        assert!(decoded.stdout.iter().copied().max().unwrap() > 100);
        assert!(manager.stop(&response.id).await);
        assert!(!dir.exists());
        manager.shutdown().await;
        server.abort();
        let _ = server.await;
    }
}
