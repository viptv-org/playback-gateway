use super::*;

/// Run explicitly with PLAYBACK_TEST_FFMPEG and PLAYBACK_TEST_FFPROBE set to binaries.
#[tokio::test]
#[ignore = "requires real FFmpeg/libx264 and ffprobe binaries"]
async fn real_ffmpeg_remux_transcode_and_cleanup() {
    let ffmpeg = PathBuf::from(std::env::var("PLAYBACK_TEST_FFMPEG").expect("PLAYBACK_TEST_FFMPEG"));
    let ffprobe = PathBuf::from(std::env::var("PLAYBACK_TEST_FFPROBE").expect("PLAYBACK_TEST_FFPROBE"));
    let root = tempfile::tempdir().unwrap();
    let fixture = root.path().join("fixture.mp4");
    let status = Command::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=24",
            "-t",
            "6",
            "-c:v",
            "libx264",
            "-threads",
            "2",
            "-pix_fmt",
            "yuv420p",
            "-profile:v",
            "main",
            "-level:v",
            "3.1",
            "-g",
            "48",
            "-movflags",
            "+faststart",
        ])
        .arg(&fixture)
        .status()
        .await
        .unwrap();
    assert!(status.success());
    let bytes = tokio::fs::read(&fixture).await.unwrap();
    let router = axum::Router::new().route(
        "/fixture.mp4",
        axum::routing::get(move || {
            let bytes = bytes.clone();
            async move { bytes }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/fixture.mp4", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let media_root = root.path().join("media");
    let manager = PlaybackManager::new(Config {
        ffmpeg: ffmpeg.clone(),
        ffprobe: ffprobe.clone(),
        root: media_root.clone(),
        max_sessions: 1,
        ttl: Duration::from_secs(60),
    });
    let provider_slots = Arc::new(Semaphore::new(1));
    for (force, position) in [(false, 0.0), (true, 0.0), (false, 1.25)] {
        let caps = if force {
            Capabilities {
                max_width: 200,
                max_height: 100,
                ..Capabilities::default()
            }
        } else {
            Capabilities::default()
        };
        let response = manager
            .start_with_permit(
                url.clone(),
                HashMap::new(),
                position,
                Some(caps),
                force,
                false,
                Some(provider_slots.clone().try_acquire_owned().unwrap()),
            )
            .await
            .unwrap();
        assert_eq!(provider_slots.available_permits(), 0);
        assert_eq!(
            response.mode,
            if force || position > 0.0 {
                "transcode"
            } else {
                "remux"
            }
        );
        assert_eq!(
            response.video_mode,
            if force || position > 0.0 {
                "encode"
            } else {
                "copy"
            }
        );
        assert_eq!(response.audio_mode, "none");
        assert_eq!(response.position, position);
        assert!(!response.live);
        assert!((5.0..=7.0).contains(&response.duration));
        let capability = response.url.split('/').nth(3).unwrap();
        let playlist = axum::body::to_bytes(
            manager
                .serve(&response.id, capability, "index.m3u8")
                .await
                .unwrap()
                .into_body(),
            1024 * 1024,
        )
        .await
        .unwrap();
        let playlist = String::from_utf8(playlist.to_vec()).unwrap();
        let segment = playlist
            .lines()
            .find(|line| media_type(line) == Some("video/mp2t"))
            .unwrap();
        let served_segment = manager
            .serve(&response.id, capability, segment)
            .await
            .unwrap();
        assert!(
            !axum::body::to_bytes(served_segment.into_body(), 64 * 1024 * 1024)
                .await
                .unwrap()
                .is_empty()
        );
        let output = Command::new(&ffprobe)
            .args([
                "-v",
                "quiet",
                "-show_entries",
                "stream=width,height",
                "-of",
                "json",
            ])
            .arg(media_root.join(&response.id).join(segment))
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "fixture probe failed: {} {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let width = value["streams"][0]["width"].as_u64().unwrap();
        let height = value["streams"][0]["height"].as_u64().unwrap();
        assert!(
            width.is_multiple_of(2) && height.is_multiple_of(2) && width <= 320 && height <= 240
        );
        if force {
            assert!(width <= 200 && height <= 100);
        }
        if position > 0.0 {
            async fn frame(ffmpeg: &PathBuf, path: &std::path::Path, seek: f64) -> Vec<u8> {
                let output = Command::new(ffmpeg)
                    .args(["-v", "error", "-xerror", "-ss", &seek.to_string(), "-i"])
                    .arg(path)
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
                assert!(
                    output.status.success(),
                    "first segment must decode independently"
                );
                assert_eq!(output.stdout.len(), 32 * 24 * 3);
                output.stdout
            }
            let actual = frame(&ffmpeg, &media_root.join(&response.id).join(segment), 0.0).await;
            let expected = frame(&ffmpeg, &fixture, position).await;
            let earlier_keyframe = frame(&ffmpeg, &fixture, 0.0).await;
            let error = |reference: &[u8]| {
                actual
                    .iter()
                    .zip(reference)
                    .map(|(a, b)| (*a as f64 - *b as f64).abs())
                    .sum::<f64>()
                    / actual.len() as f64
            };
            assert!(
                error(&expected) < 8.0,
                "seek frame mismatch: {}",
                error(&expected)
            );
            assert!(
                error(&expected) < error(&earlier_keyframe),
                "must not echo offset while starting at prior keyframe"
            );
        }
        assert!(manager.stop(&response.id).await);
        assert_eq!(provider_slots.available_permits(), 1);
        assert!(!media_root.join(&response.id).exists());
    }
    manager.shutdown().await;
    assert!(tokio::fs::read_dir(media_root)
        .await
        .unwrap()
        .next_entry()
        .await
        .unwrap()
        .is_none());
    server.abort();
    let _ = server.await;
}

/// A real 720p60 H.264/AAC source must be stream-copied into HLS.
///
/// This is the reported live stutter: the old policy capped 720p at level 4.0
/// and silently capped frame rate at 30, so an ordinary 720p60 channel was
/// fully re-encoded in realtime. Encoding could not keep up, the managed HLS
/// window underran, and FFmpeg exited and restarted in a loop.
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires real FFmpeg/ffprobe binaries"]
async fn a_720p60_source_is_stream_copied_into_hls() {
    use std::os::unix::fs::PermissionsExt;
    let ffmpeg = PathBuf::from(std::env::var("PLAYBACK_TEST_FFMPEG").expect("PLAYBACK_TEST_FFMPEG"));
    let ffprobe = PathBuf::from(std::env::var("PLAYBACK_TEST_FFPROBE").expect("PLAYBACK_TEST_FFPROBE"));
    let root = tempfile::tempdir().unwrap();
    let fixture = root.path().join("live720p60.mkv");
    // Level 4.1 at 59.94fps, exactly like the production live channels.
    let status = Command::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=1280x720:rate=60000/1001",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "4",
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-profile:v",
            "high",
            "-level:v",
            "4.1",
            "-g",
            "120",
            "-keyint_min",
            "120",
            "-sc_threshold",
            "0",
            "-force_key_frames",
            "0,2",
            "-c:a",
            "aac",
            "-ac",
            "2",
            "-shortest",
        ])
        .arg(&fixture)
        .status()
        .await
        .unwrap();
    assert!(status.success());

    let probe = Command::new(&ffprobe)
        .args(["-v", "error", "-show_streams", "-of", "json"])
        .arg(&fixture)
        .output()
        .await
        .unwrap();
    let probed: Probe = serde_json::from_slice(&probe.stdout).unwrap();
    let video = probed
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video"))
        .unwrap();
    assert_eq!(video.level, Some(41), "fixture must be level 4.1");
    // FFmpeg builds spell the same NTSC rate differently (60000/1001 vs
    // 19001/317); assert the parsed rate, not ffprobe's rational form.
    let frame_rate = video.avg_frame_rate.as_deref().and_then(|rate| {
        let (numerator, denominator) = rate.split_once('/')?;
        match (numerator.parse::<f64>(), denominator.parse::<f64>()) {
            (Ok(numerator), Ok(denominator)) => Some(numerator / denominator),
            _ => None,
        }
    });
    assert!(
        frame_rate.is_some_and(|rate| (59.9..=60.1).contains(&rate)),
        "fixture must be ~59.94fps: {:?}",
        video.avg_frame_rate
    );
    assert!(
        video.width == Some(1280) && video.height == Some(720),
        "fixture must be 720p"
    );

    let bytes = tokio::fs::read(&fixture).await.unwrap();
    let router = axum::Router::new().route(
        "/live.mkv",
        axum::routing::get(move || {
            let bytes = bytes.clone();
            async move { bytes }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/live.mkv", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let arguments = root.path().join("ffmpeg-arguments.txt");
    let wrapper = root.path().join("ffmpeg-wrapper.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\nexec \"{}\" \"$@\"\n",
            arguments.display(),
            ffmpeg.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();

    let manager = PlaybackManager::new(Config {
        ffmpeg: wrapper,
        ffprobe,
        root: root.path().join("media"),
        max_sessions: 1,
        ttl: Duration::from_secs(60),
    });
    let response = manager
        .start_with_permit(url, HashMap::new(), 0.0, None, false, true, None)
        .await
        .unwrap();
    let args = tokio::fs::read_to_string(&arguments).await.unwrap();
    let args: Vec<_> = args.lines().collect();
    let value_after = |name: &str| {
        args.iter()
            .position(|arg| *arg == name)
            .map(|index| args[index + 1])
    };
    // The whole point: no video encoder runs for this source.
    assert_eq!(
        value_after("-c:v"),
        Some("copy"),
        "720p60 video must be copied"
    );
    assert_eq!(
        value_after("-c:a"),
        Some("copy"),
        "AAC-LC audio must be copied"
    );
    assert_eq!(
        response.video_mode, "copy",
        "a 720p60 channel must not be re-encoded"
    );
    assert!(
        !args.contains(&"libx264") && !args.contains(&"scale") && !args.contains(&"-vf"),
        "no encoder or filter may run for a directly compatible source: {args:?}"
    );
    // A live input already arrives in realtime; `-re` only withheld the
    // upstream's initial burst from the viewer's buffer.
    assert!(
        !args.contains(&"-re") && !args.contains(&"-readrate"),
        "live input must not be paced: {args:?}"
    );
    assert!(manager.stop(&response.id).await);
    manager.shutdown().await;
    server.abort();
    let _ = server.await;
}

/// Run explicitly with PLAYBACK_TEST_FFMPEG and PLAYBACK_TEST_FFPROBE set to binaries.
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires real FFmpeg/ffprobe binaries"]
async fn compatible_video_is_copied_while_incompatible_audio_is_encoded() {
    use std::os::unix::fs::PermissionsExt;
    let ffmpeg = PathBuf::from(std::env::var("PLAYBACK_TEST_FFMPEG").expect("PLAYBACK_TEST_FFMPEG"));
    let ffprobe = PathBuf::from(std::env::var("PLAYBACK_TEST_FFPROBE").expect("PLAYBACK_TEST_FFPROBE"));
    let root = tempfile::tempdir().unwrap();
    let fixture = root.path().join("hybrid.mkv");
    let status = Command::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=24",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000",
            "-t",
            "6",
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-profile:v",
            "high",
            "-level:v",
            "4.0",
            "-g",
            "240",
            "-keyint_min",
            "240",
            "-sc_threshold",
            "0",
            "-force_key_frames",
            "0,2,5",
            "-c:a",
            "ac3",
            "-ac",
            "2",
            "-metadata:s:a:0",
            "language=eng",
            "-shortest",
        ])
        .arg(&fixture)
        .status()
        .await
        .unwrap();
    assert!(status.success());
    let bytes = tokio::fs::read(&fixture).await.unwrap();
    let router = axum::Router::new().route(
        "/hybrid.mkv",
        axum::routing::get(move || {
            let bytes = bytes.clone();
            async move { bytes }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hybrid.mkv", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let arguments = root.path().join("ffmpeg-arguments.txt");
    let wrapper = root.path().join("ffmpeg-wrapper.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\nexec \"{}\" \"$@\"\n",
            arguments.display(),
            ffmpeg.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let manager = PlaybackManager::new(Config {
        ffmpeg: wrapper,
        ffprobe,
        root: root.path().join("media"),
        max_sessions: 1,
        ttl: Duration::from_secs(60),
    });
    let response = manager
        .start_with_permit(url, HashMap::new(), 0.0, None, false, false, None)
        .await
        .unwrap();
    assert_eq!(response.mode, "transcode");
    assert_eq!(response.video_mode, "copy");
    assert_eq!(response.audio_mode, "encode");
    let args = tokio::fs::read_to_string(&arguments).await.unwrap();
    let args: Vec<_> = args.lines().collect();
    let value_after = |name: &str| {
        args.iter()
            .position(|arg| *arg == name)
            .map(|index| args[index + 1])
    };
    assert_eq!(value_after("-c:v"), Some("copy"));
    assert_eq!(value_after("-c:a"), Some("aac"));
    assert!(!args.contains(&"libx264"));
    assert_eq!(value_after("-hls_init_time"), Some("1"));
    assert!(value_after("-hls_flags").is_some_and(|flags| flags.contains("split_by_time")));
    assert!(!args.contains(&"-threads"));
    assert!(!args.contains(&"-filter_threads"));
    let playlist_path = root
        .path()
        .join("media")
        .join(&response.id)
        .join("index.m3u8");
    let disk_playlist = timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(text) = tokio::fs::read_to_string(&playlist_path).await {
                if text.contains("#EXT-X-ENDLIST") {
                    return text;
                }
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("variable-GOP copy must finish");
    let durations: Vec<f64> = disk_playlist
        .lines()
        .filter_map(|line| line.strip_prefix("#EXTINF:"))
        .filter_map(|value| value.trim_end_matches(',').parse().ok())
        .collect();
    assert!(durations.len() >= 3, "{disk_playlist}");
    assert!(
        durations.iter().copied().fold(0.0, f64::max) <= 2.1,
        "{durations:?}"
    );
    let capability = response.url.split('/').nth(3).unwrap();
    let served = String::from_utf8(
        axum::body::to_bytes(
            manager
                .serve(&response.id, capability, "index.m3u8")
                .await
                .unwrap()
                .into_body(),
            1024 * 1024,
        )
        .await
        .unwrap()
        .to_vec(),
    )
    .unwrap();
    assert!(served.contains("#EXT-X-TARGETDURATION:2"), "{served}");
    let decoded = Command::new(&ffmpeg)
        .args(["-v", "error", "-xerror", "-i"])
        .arg(&playlist_path)
        .args(["-map", "0:v:0", "-map", "0:a:0", "-f", "null", "-"])
        .output()
        .await
        .unwrap();
    assert!(
        decoded.status.success(),
        "{}",
        String::from_utf8_lossy(&decoded.stderr)
    );
    assert!(manager.stop(&response.id).await);
    manager.shutdown().await;
    server.abort();
    let _ = server.await;
}
