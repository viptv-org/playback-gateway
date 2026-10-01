use super::*;
use crate::sanitize::HlsSanitizer;

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
/// Three MPEG-TS null packets: enough for the transport to classify a segment.
fn ts_packet() -> Vec<u8> {
    let mut packet = [0xff_u8; 188];
    packet[..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
    packet.repeat(3)
}

/// A loopback origin: `(path, Content-Type, body)` resources plus a playlist
/// produced per request, so a live window can advance.
async fn origin(
    playlist: impl Fn() -> String + Clone + Send + Sync + 'static,
    resources: Vec<(String, &'static str, Vec<u8>)>,
) -> (String, tokio::task::JoinHandle<()>) {
    let mut router = axum::Router::new().route(
        "/live/index.m3u8",
        axum::routing::get(move || {
            let playlist = playlist.clone();
            async move { playlist() }
        }),
    );
    for (path, content_type, body) in resources {
        router = router.route(
            &format!("/live/{path}"),
            axum::routing::get(move || {
                let body = body.clone();
                async move { ([(axum::http::header::CONTENT_TYPE, content_type)], body) }
            }),
        );
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/live/index.m3u8", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (url, server)
}

fn vod_playlist(names: &[&str]) -> String {
    let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n");
    for name in names {
        playlist.push_str(&format!("#EXTINF:2.0,\n{name}\n"));
    }
    playlist.push_str("#EXT-X-ENDLIST\n");
    playlist
}

fn args(cmd: &Command) -> Vec<String> {
    cmd.as_std()
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn ffmpeg_segment_name_refusals_are_classified() {
    for stderr in [
        "[in#0 @ 0x1] URL http://origin/seg0.png is not in allowed_segment_extensions, consider updating hls.c",
        "[in#0 @ 0x1] detected format mpegts extension none mismatches allowed extensions in url http://origin/seg0",
    ] {
        assert_eq!(probe_failure(stderr.as_bytes()), ProbeFailure::SegmentExtension);
    }
    assert!(!ProbeFailure::SegmentExtension.retryable());
    let image = "[hls @ 0x1] Opening 'http://origin/seg0.ts' for reading\n[jpeg_pipe @ 0x2] Invalid data found when processing input";
    assert_eq!(probe_failure(image.as_bytes()), ProbeFailure::ImageSegment);
    assert!(!ProbeFailure::ImageSegment.retryable());
}

#[test]
fn sanitizer_is_selected_only_for_disguise_signals() {
    let probe = |json: &str| -> Probe { serde_json::from_str(json).unwrap() };
    let image_only = probe(
        r#"{"format":{"format_name":"hls"},"streams":[{"index":0,"codec_type":"video","codec_name":"mjpeg"}]}"#,
    );
    let gif_only = probe(
        r#"{"format":{"format_name":"hls"},"streams":[{"index":0,"codec_type":"video","codec_name":"gif"}]}"#,
    );
    let media = probe(
        r#"{"format":{"format_name":"hls"},"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},{"index":1,"codec_type":"audio","codec_name":"aac"}]}"#,
    );
    // A real image file, or mjpeg with audio, is not a disguised HLS source.
    let picture = probe(
        r#"{"format":{"format_name":"image2"},"streams":[{"index":0,"codec_type":"video","codec_name":"mjpeg"}]}"#,
    );
    let mjpeg_with_audio = probe(
        r#"{"format":{"format_name":"hls"},"streams":[{"index":0,"codec_type":"video","codec_name":"mjpeg"},{"index":1,"codec_type":"audio","codec_name":"aac"}]}"#,
    );
    assert!(disguise_signal(&Ok(image_only)));
    assert!(disguise_signal(&Ok(gif_only)));
    assert!(!disguise_signal(&Ok(media)));
    assert!(!disguise_signal(&Ok(picture)));
    assert!(!disguise_signal(&Ok(mjpeg_with_audio)));
    assert!(disguise_signal(&Err(ProbeFailure::SegmentExtension)));
    assert!(disguise_signal(&Err(ProbeFailure::ImageSegment)));
    for failure in [
        ProbeFailure::Http(404),
        ProbeFailure::Network,
        ProbeFailure::Timeout,
        ProbeFailure::Protocol,
        ProbeFailure::Exit,
    ] {
        assert!(!disguise_signal(&Err(failure)));
    }
}

#[tokio::test]
async fn sanitized_input_carries_no_credentials_proxy_or_relaxation() {
    let (url, server) = origin(
        || vod_playlist(&["seg0.png"]),
        vec![(
            "seg0.png".into(),
            "image/png",
            [PNG_1X1, &ts_packet()].concat(),
        )],
    )
    .await;
    let sanitizer = HlsSanitizer::start(&url, &HashMap::new(), None)
        .await
        .unwrap();
    let block = "Authorization: Bearer secret\r\nx-playback-egress-proxy: http://10.0.0.1:3128\r\n";
    let mut cmd = Command::new("ffmpeg");
    let input = source_input(&mut cmd, &url, block, Some(&sanitizer));
    let args = args(&cmd);
    assert_eq!(input, sanitizer.url());
    assert!(input.starts_with("http://127.0.0.1:"));
    for refused in ["-headers", "-http_proxy", "-extension_picky"] {
        assert!(
            !args.iter().any(|arg| arg == refused),
            "{refused} in {args:?}"
        );
    }
    assert!(!args.iter().any(|arg| arg.contains("secret")));
    let whitelist = args.iter().position(|arg| arg == "-protocol_whitelist");
    assert_eq!(args[whitelist.unwrap() + 1], "http,tcp,crypto");
    assert!(args.iter().any(|arg| arg == "-rw_timeout"));
    assert!(args.iter().any(|arg| arg == "-reconnect"));
    // Without a sanitizer the origin input is unchanged.
    let mut cmd = Command::new("ffmpeg");
    assert_eq!(source_input(&mut cmd, &url, block, None), url);
    let args = self::args(&cmd);
    assert!(args.iter().any(|arg| arg == "-headers"));
    assert!(args.iter().any(|arg| arg == "-http_proxy"));
    assert!(args
        .iter()
        .any(|arg| arg == "http,https,httpproxy,tcp,tls,crypto"));
    server.abort();
}

#[tokio::test]
async fn sanitizer_requires_its_token_and_closes_on_drop() {
    let (url, server) = origin(
        || vod_playlist(&["seg0.png"]),
        vec![(
            "seg0.png".into(),
            "image/png",
            [PNG_1X1, &ts_packet()].concat(),
        )],
    )
    .await;
    let sanitizer = HlsSanitizer::start(&url, &HashMap::new(), None)
        .await
        .unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let playlist = client.get(sanitizer.url()).send().await.unwrap();
    assert_eq!(playlist.status(), 200);
    let text = playlist.text().await.unwrap();
    let segment = text
        .lines()
        .find(|line| !line.starts_with('#') && !line.is_empty())
        .unwrap()
        .to_owned();
    assert!(segment.ends_with(".ts"), "{text}");
    let base = sanitizer.url();
    let base = base.strip_suffix("index.m3u8").unwrap();
    let body = client.get(format!("{base}{segment}")).send().await.unwrap();
    assert_eq!(body.headers()["content-type"], "video/mp2t");
    assert_eq!(body.bytes().await.unwrap().as_ref(), ts_packet());
    // Wrong, shortened and absent tokens are indistinguishable from nothing.
    let port = base
        .strip_prefix("http://127.0.0.1:")
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    let token = base.trim_end_matches('/').rsplit('/').next().unwrap();
    for path in [
        format!("/{}/index.m3u8", "0".repeat(token.len())),
        format!("/{}/index.m3u8", &token[1..]),
        "/index.m3u8".to_owned(),
        format!("/{token}"),
        "/".to_owned(),
    ] {
        let response = client
            .get(format!("http://127.0.0.1:{port}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "{path}");
    }
    // Redaction removes the capability URL and the bare token.
    let line = format!("Opening '{}' for reading; token {token}", sanitizer.url());
    let redacted = sanitizer.redact(&line);
    assert!(!redacted.contains(token), "{redacted}");
    let display = crate::source_display_text(&line, 512, &url, &HashMap::new());
    assert!(!display.contains("127.0.0.1"), "{display}");
    let url = sanitizer.url();
    drop(sanitizer);
    tokio::task::yield_now().await;
    let after = timeout(Duration::from_secs(3), client.get(&url).send())
        .await
        .unwrap();
    assert!(
        after.is_err() || after.unwrap().status() == 404,
        "a dropped sanitizer must refuse"
    );
    server.abort();
}

#[tokio::test]
async fn sanitizer_refuses_private_cross_origin_children() {
    // Same-origin loopback is the source itself; a child on another private
    // origin is refused like it is for original delivery.
    let (url, server) = origin(
        || vod_playlist(&["http://10.255.255.1/seg0.ts"]),
        Vec::new(),
    )
    .await;
    assert!(HlsSanitizer::start(&url, &HashMap::new(), None)
        .await
        .is_err());
    server.abort();
}

/// Encode a clean 160x90 h264/aac HLS source of `segments` × `seconds`.
async fn clean_source(ffmpeg: &PathBuf, dir: &std::path::Path, segments: u32, seconds: u32) {
    let status = Command::new(ffmpeg)
        .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=160x90:rate=25")
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
        .args(["-t", &(segments * seconds).to_string()])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "25"])
        .args([
            "-c:a",
            "aac",
            "-f",
            "hls",
            "-hls_time",
            &seconds.to_string(),
        ])
        .args(["-hls_list_size", "0", "-hls_segment_filename"])
        .arg(dir.join("segment%d.ts"))
        .arg(dir.join("clean.m3u8"))
        .status()
        .await
        .unwrap();
    assert!(status.success());
}

/// A real still image of `format` (mjpeg, gif or png) from FFmpeg itself.
async fn image(ffmpeg: &PathBuf, dir: &std::path::Path, extension: &str) -> Vec<u8> {
    let path = dir.join(format!("prefix.{extension}"));
    let status = Command::new(ffmpeg)
        .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
        .arg("color=c=red:size=8x8")
        .args(["-frames:v", "1", "-update", "1"])
        .arg(&path)
        .status()
        .await
        .unwrap();
    assert!(status.success());
    std::fs::read(path).unwrap()
}

/// Count every demuxed packet of `input` with the engine's input arguments.
async fn packets(ffmpeg: &PathBuf, input: &str, sanitizer: Option<&HlsSanitizer>) -> usize {
    let mut cmd = Command::new(ffmpeg);
    cmd.kill_on_drop(true).args(["-v", "error", "-nostdin"]);
    let input = source_input(&mut cmd, input, "", sanitizer);
    cmd.arg("-i")
        .arg(input)
        .args(["-map", "0", "-c", "copy", "-f", "framemd5", "-"]);
    let output = timeout(Duration::from_secs(60), cmd.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .count()
}

fn manager(
    ffmpeg: &std::path::Path,
    ffprobe: PathBuf,
    root: &std::path::Path,
) -> Arc<PlaybackManager> {
    PlaybackManager::new(Config {
        ffmpeg: ffmpeg.to_path_buf(),
        ffprobe,
        root: root.join("media"),
        max_sessions: 1,
        ttl: Duration::from_secs(60),
    })
}

#[tokio::test]
#[ignore = "requires configured real FFmpeg/ffprobe; local disguised HLS fixture only"]
async fn real_disguised_hls_segments_probe_and_copy() {
    let ffmpeg = PathBuf::from(std::env::var("PLAYBACK_TEST_FFMPEG").unwrap());
    let ffprobe = PathBuf::from(std::env::var("PLAYBACK_TEST_FFPROBE").unwrap());
    let root = tempfile::tempdir().unwrap();
    clean_source(&ffmpeg, root.path(), 5, 2).await;
    let segment =
        |index: usize| std::fs::read(root.path().join(format!("segment{index}.ts"))).unwrap();
    let (jpeg, gif, png) = (
        image(&ffmpeg, root.path(), "jpg").await,
        image(&ffmpeg, root.path(), "gif").await,
        image(&ffmpeg, root.path(), "png").await,
    );
    assert_eq!(gif[0], b'G', "a GIF opens with the MPEG-TS sync byte");
    let css = b"body{background:#000;margin:0}\n.x{color:red}\n".to_vec();
    // Every segment is labelled as something it is not.
    let segments: Vec<(String, &'static str, Vec<u8>)> = vec![
        (
            "seg0.jpg".into(),
            "image/jpeg",
            [jpeg.clone(), segment(0)].concat(),
        ),
        (
            "seg1.gif".into(),
            "image/gif",
            [gif.clone(), segment(1)].concat(),
        ),
        (
            "seg2.png".into(),
            "image/png",
            [png.clone(), segment(2)].concat(),
        ),
        (
            "seg3.css".into(),
            "text/css",
            [css.clone(), segment(3)].concat(),
        ),
        (
            "seg4".into(),
            "image/png",
            [jpeg.clone(), segment(4)].concat(),
        ),
    ];
    let names: Vec<String> = segments.iter().map(|(name, _, _)| name.clone()).collect();
    let playlist = vod_playlist(&names.iter().map(String::as_str).collect::<Vec<_>>());
    let mut resources = segments;
    for index in 0..5 {
        resources.push((format!("segment{index}.ts"), "video/mp2t", segment(index)));
    }
    let clean_playlist = std::fs::read_to_string(root.path().join("clean.m3u8")).unwrap();
    resources.push((
        "clean.m3u8".into(),
        "application/vnd.apple.mpegurl",
        clean_playlist.into_bytes(),
    ));
    let (url, server) = origin(move || playlist.clone(), resources).await;
    let clean = url.replace("index.m3u8", "clean.m3u8");
    let manager = manager(&ffmpeg, ffprobe, root.path());
    let probe = manager
        .probe(&url, "", None)
        .await
        .expect("disguised HLS must probe");
    assert!(probe.disguised_hls);
    assert_eq!(probe.video().unwrap().codec_name.as_deref(), Some("h264"));
    assert!(probe
        .streams
        .iter()
        .any(|stream| stream.codec_name.as_deref() == Some("aac")));
    // The clean source probes directly; nothing is sanitized for it.
    let direct = manager.probe(&clean, "", None).await.unwrap();
    assert!(!direct.disguised_hls);
    let expected = packets(&ffmpeg, &clean, None).await;
    assert!(expected > 400, "{expected}");
    // The reader starts its own sanitizer from the probe's verdict.
    let sanitizer = crate::sanitize::for_reader(&probe, &url, "", None)
        .await
        .unwrap()
        .expect("a disguised probe selects the sanitizer");
    assert_eq!(packets(&ffmpeg, &url, Some(&sanitizer)).await, expected);
    drop(sanitizer);
    server.abort();
}

/// Honestly named `.ts` segments with an image prefix pass FFmpeg's name
/// check. FFmpeg before 7.1 typefinds them as the image (an image-only probe
/// selects the sanitizer); later versions force MPEG-TS by name and resync.
/// Either way, the reader the probe selects demuxes every packet.
#[tokio::test]
#[ignore = "requires configured real FFmpeg/ffprobe; local disguised HLS fixture only"]
async fn real_image_prefixed_ts_segments_demux_fully() {
    let ffmpeg = PathBuf::from(std::env::var("PLAYBACK_TEST_FFMPEG").unwrap());
    let ffprobe = PathBuf::from(std::env::var("PLAYBACK_TEST_FFPROBE").unwrap());
    let root = tempfile::tempdir().unwrap();
    clean_source(&ffmpeg, root.path(), 3, 2).await;
    let jpeg = image(&ffmpeg, root.path(), "jpg").await;
    let mut resources = Vec::new();
    let mut names = Vec::new();
    for index in 0..3 {
        let clean = std::fs::read(root.path().join(format!("segment{index}.ts"))).unwrap();
        names.push(format!("part{index}.ts"));
        resources.push((
            format!("part{index}.ts"),
            "video/mp2t",
            [jpeg.clone(), clean.clone()].concat(),
        ));
        resources.push((format!("segment{index}.ts"), "video/mp2t", clean));
    }
    let playlist = vod_playlist(&names.iter().map(String::as_str).collect::<Vec<_>>());
    let clean_playlist = std::fs::read_to_string(root.path().join("clean.m3u8")).unwrap();
    resources.push((
        "clean.m3u8".into(),
        "application/vnd.apple.mpegurl",
        clean_playlist.into_bytes(),
    ));
    let (url, server) = origin(move || playlist.clone(), resources).await;
    let manager = manager(&ffmpeg, ffprobe, root.path());
    let probe = manager.probe(&url, "", None).await.unwrap();
    assert!(!probe.image_only_hls());
    assert_eq!(probe.video().unwrap().codec_name.as_deref(), Some("h264"));
    let expected = packets(&ffmpeg, &url.replace("index.m3u8", "clean.m3u8"), None).await;
    let sanitizer = crate::sanitize::for_reader(&probe, &url, "", None)
        .await
        .unwrap();
    assert_eq!(packets(&ffmpeg, &url, sanitizer.as_deref()).await, expected);
    server.abort();
}

/// A live playlist (no ENDLIST, advancing window) is reloaded through the
/// loopback hop and every disguised segment is demuxed exactly once.
#[tokio::test]
#[ignore = "requires configured real FFmpeg/ffprobe; local disguised HLS fixture only"]
async fn real_disguised_live_window_reloads_through_sanitizer() {
    let ffmpeg = PathBuf::from(std::env::var("PLAYBACK_TEST_FFMPEG").unwrap());
    let ffprobe = PathBuf::from(std::env::var("PLAYBACK_TEST_FFPROBE").unwrap());
    let root = tempfile::tempdir().unwrap();
    const SEGMENTS: usize = 6;
    clean_source(&ffmpeg, root.path(), SEGMENTS as u32, 1).await;
    let gif = image(&ffmpeg, root.path(), "gif").await;
    let mut resources = Vec::new();
    for index in 0..SEGMENTS {
        let clean = std::fs::read(root.path().join(format!("segment{index}.ts"))).unwrap();
        resources.push((
            format!("live{index}.gif"),
            "image/gif",
            [gif.clone(), clean.clone()].concat(),
        ));
        resources.push((format!("segment{index}.ts"), "video/mp2t", clean));
    }
    let clean_playlist = std::fs::read_to_string(root.path().join("clean.m3u8")).unwrap();
    resources.push((
        "clean.m3u8".into(),
        "application/vnd.apple.mpegurl",
        clean_playlist.into_bytes(),
    ));
    let started = Arc::new(std::sync::Mutex::new(None::<Instant>));
    let clock = started.clone();
    // Three-segment window advancing one segment per second once the first
    // reader arrives; the final window ends the stream.
    let playlist = move || {
        let at = *clock.lock().unwrap().get_or_insert_with(Instant::now);
        let first = (at.elapsed().as_secs() as usize).min(SEGMENTS - 3);
        let mut text = format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{first}\n"
        );
        for index in first..first + 3 {
            text.push_str(&format!("#EXTINF:1.0,\nlive{index}.gif\n"));
        }
        if first == SEGMENTS - 3 {
            text.push_str("#EXT-X-ENDLIST\n");
        }
        text
    };
    let (url, server) = origin(playlist, resources).await;
    let manager = manager(&ffmpeg, ffprobe, root.path());
    let probe = manager.probe(&url, "", None).await.unwrap();
    assert!(probe.disguised_hls);
    let expected = packets(&ffmpeg, &url.replace("index.m3u8", "clean.m3u8"), None).await;
    // Restart the window so the reader sees it advance from the start.
    *started.lock().unwrap() = None;
    let sanitizer = crate::sanitize::for_reader(&probe, &url, "", None)
        .await
        .unwrap()
        .unwrap();
    let mut cmd = Command::new(&ffmpeg);
    cmd.kill_on_drop(true).args(["-v", "error", "-nostdin"]);
    let input = source_input(&mut cmd, &url, "", Some(&sanitizer));
    cmd.args(["-live_start_index", "0", "-i"])
        .arg(input)
        .args(["-map", "0", "-c", "copy", "-f", "framemd5", "-"]);
    let output = timeout(Duration::from_secs(60), cmd.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let live = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .count();
    assert_eq!(live, expected);
    server.abort();
}
