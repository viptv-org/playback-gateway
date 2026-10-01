use super::*;

/// IPTV origins disguise MPEG-TS segments as images, stylesheets or fonts:
/// odd or missing suffixes, a real image header before the first sync byte,
/// and a Content-Type that matches the disguise instead of the bytes.
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0x00,
    0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xdd, 0x8d, 0xb0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e,
    0x44, 0xae, 0x42, 0x60, 0x82,
];

#[test]
fn ffmpeg_segment_name_refusals_are_classified() {
    for stderr in [
        "[in#0 @ 0x1] URL http://origin/seg0.png is not in allowed_segment_extensions, consider updating hls.c",
        "[in#0 @ 0x1] detected format mpegts extension none mismatches allowed extensions in url http://origin/seg0",
    ] {
        assert_eq!(probe_failure(stderr.as_bytes()), ProbeFailure::SegmentExtension);
    }
    assert!(!ProbeFailure::SegmentExtension.retryable());
}

#[test]
fn relaxed_segment_names_are_requested_only_for_refused_sources() {
    let args = |relaxed| -> Vec<String> {
        let mut cmd = Command::new("ffmpeg");
        relaxed_hls_args(&mut cmd, relaxed);
        cmd.as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    };
    assert_eq!(args(true), ["-extension_picky", "0"]);
    // The ffmpeg CLI rejects a demuxer option its input does not consume.
    assert!(args(false).is_empty());
}

#[tokio::test]
#[ignore = "requires configured real FFmpeg/ffprobe; local disguised HLS fixture only"]
async fn real_disguised_hls_segments_probe_and_copy() {
    let ffmpeg = PathBuf::from(std::env::var("PLAYBACK_TEST_FFMPEG").unwrap());
    let ffprobe = PathBuf::from(std::env::var("PLAYBACK_TEST_FFPROBE").unwrap());
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let status = Command::new(&ffmpeg)
        .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=160x90:rate=25")
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
        .args([
            "-t",
            "6",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "25",
        ])
        .args([
            "-c:a",
            "aac",
            "-f",
            "hls",
            "-hls_time",
            "2",
            "-hls_list_size",
            "0",
        ])
        .arg("-hls_segment_filename")
        .arg(source.join("segment%d.ts"))
        .arg(source.join("index.m3u8"))
        .status()
        .await
        .unwrap();
    assert!(status.success());
    let segment = |index: usize| std::fs::read(source.join(format!("segment{index}.ts"))).unwrap();
    // (path, Content-Type, body): a PNG-prefixed .png, an extensionless
    // segment and a .css one, every one labelled as something it is not.
    let segments = [
        ("seg0.png", "image/png", [PNG_1X1, &segment(0)].concat()),
        ("seg1", "image/png", segment(1)),
        ("seg2.css", "text/css", segment(2)),
    ];
    let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n");
    for (path, _, _) in &segments {
        playlist.push_str(&format!("#EXTINF:2.0,\n{path}\n"));
    }
    playlist.push_str("#EXT-X-ENDLIST\n");
    let mut router = axum::Router::new().route(
        "/index.m3u8",
        axum::routing::get(move || {
            let playlist = playlist.clone();
            async move { playlist }
        }),
    );
    for (path, content_type, body) in segments {
        router = router.route(
            &format!("/{path}"),
            axum::routing::get(move || {
                let body = body.clone();
                async move { ([(axum::http::header::CONTENT_TYPE, content_type)], body) }
            }),
        );
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/index.m3u8", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let manager = PlaybackManager::new(Config {
        ffmpeg: ffmpeg.clone(),
        ffprobe,
        root: root.path().join("media"),
        max_sessions: 1,
        ttl: Duration::from_secs(60),
    });
    let probe = manager
        .probe(&url, "", None)
        .await
        .expect("disguised HLS must probe");
    assert!(probe.relaxed_hls_segments);
    assert_eq!(probe.video().unwrap().codec_name.as_deref(), Some("h264"));
    // Read every segment with the input arguments the engine builds.
    let mut cmd = Command::new(&ffmpeg);
    cmd.kill_on_drop(true).args(["-v", "error", "-nostdin"]);
    input_args(&mut cmd, "");
    relaxed_hls_args(&mut cmd, probe.relaxed_hls_segments);
    cmd.arg("-i")
        .arg(&url)
        .args(["-map", "0:v:0", "-c", "copy", "-f", "framemd5", "-"]);
    let output = timeout(Duration::from_secs(30), cmd.output())
        .await
        .unwrap()
        .unwrap();
    server.abort();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let packets = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with("0,"))
        .count();
    assert_eq!(packets, 150, "all three disguised segments must demux");
}
