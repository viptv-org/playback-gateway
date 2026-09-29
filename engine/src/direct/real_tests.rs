use super::*;
use axum::{
    extract::{Path, State},
    routing::get,
    Router,
};
#[tokio::test]
#[ignore = "requires configured real FFmpeg/ffprobe; controlled fixtures only"]
async fn real_original_mp4_hls_resume_and_decode_without_encoder() {
    let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").unwrap();
    let ffprobe = std::env::var("PLAYBACK_TEST_FFPROBE").unwrap();
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("movie.mp4");
    let result = Command::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "60",
            "-c:v",
            "libx264",
            "-threads",
            "1",
            "-profile:v",
            "high",
            "-level:v",
            "4.1",
            "-g",
            "30",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-ac",
            "2",
            "-metadata:s:a:0",
            "language=eng",
            "-movflags",
            "+faststart",
        ])
        .arg(&file)
        .output()
        .await
        .unwrap();
    assert!(result.status.success(), "fixture generation failed");
    let result = Command::new(&ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(&file)
        .args([
            "-c",
            "copy",
            "-metadata:s:a:0",
            "language=",
            "-hls_time",
            "1",
            "-hls_playlist_type",
            "vod",
            "-hls_segment_filename",
        ])
        .arg(root.path().join("part-%03d.ts"))
        .arg(root.path().join("live.m3u8"))
        .output()
        .await
        .unwrap();
    assert!(result.status.success(), "HLS fixture generation failed");
    async fn origin(
        State(root): State<PathBuf>,
        Path(name): Path<String>,
        headers: HeaderMap,
    ) -> Response {
        let data = tokio::fs::read(root.join(name)).await.unwrap();
        let size = data.len() as u64;
        let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
        let (start, end) = range_bounds(range, size).unwrap();
        let mut builder = Response::builder()
            .status(if range.is_some() { 206 } else { 200 })
            .header(header::CONTENT_LENGTH, (end - start + 1).to_string())
            .header(header::ACCEPT_RANGES, "bytes");
        if range.is_some() {
            builder = builder.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
        }
        builder
            .body(Body::from(data[start as usize..=end as usize].to_vec()))
            .unwrap()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/:name", get(origin))
        .with_state(root.path().to_path_buf());
    let origin = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let manager = PlaybackManager::new(Config {
        ffmpeg: root.path().join("encoder-must-not-run"),
        ffprobe: PathBuf::from(ffprobe),
        root: root.path().join("sessions"),
        max_sessions: 4,
        ttl: Duration::from_secs(60),
    });
    async fn media(
        State(manager): State<Arc<PlaybackManager>>,
        Path((id, cap, file)): Path<(String, String, String)>,
        headers: HeaderMap,
    ) -> Response {
        manager
            .serve_original(&id, &cap, &file, Method::GET, headers)
            .await
            .unwrap()
            .unwrap()
    }
    let listener = tokio::net::TcpListener::bind(
        std::env::var("PLAYBACK_ROKU_BIND").unwrap_or_else(|_| "127.0.0.1:0".into()),
    )
    .await
    .unwrap();
    let proxy_base = format!("http://{}", listener.local_addr().unwrap());
    let catalogue = Arc::new(Mutex::new(Vec::<PlaybackResponse>::new()));
    let published = catalogue.clone();
    let app = Router::new()
        .route(
            "/acceptance.json",
            get(move || {
                let catalogue = published.clone();
                async move { axum::Json(catalogue.lock().await.clone()) }
            }),
        )
        .route("/media/:id/:cap/:file", get(media))
        .with_state(manager.clone());
    let proxy = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    for (name, format, position) in [("movie.mp4", "mp4", 2.0), ("live.m3u8", "hls", 0.0)] {
        let started = Instant::now();
        let response = manager
            .start_with_selection(
                format!("{base}/{name}"),
                HashMap::new(),
                position,
                Some(Capabilities {
                    max_width: 1920,
                    max_height: 1080,
                    direct_play: true,
                    ..Default::default()
                }),
                false,
                false,
                None,
                TrackSelection {
                    preferred_audio_language: Some("en".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(response.mode, "direct");
        assert_eq!(response.format, format);
        assert_eq!(response.position, position);
        assert_eq!(response.video_mode, "copy");
        let audio = response
            .selected_audio
            .as_ref()
            .expect("selected AAC track");
        assert_eq!(
            audio.language.as_deref(),
            if format == "mp4" { Some("eng") } else { None },
            "HLS fixture explicitly clears language tags; do not invent them"
        );
        let preparation = started.elapsed().as_millis();
        let mut decode = Command::new(&ffmpeg);
        decode
            .args(["-v", "error", "-ss", "2", "-i"])
            .arg(format!("{proxy_base}{}", response.url))
            .args(["-t", "1", "-f", "null", "-"]);
        let result = timeout(Duration::from_secs(15), decode.output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            result.status.success(),
            "proxied fixture decode failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        println!(
            "DIRECT_FIXTURE format={format} prepare_ms={preparation} decode_ms={}",
            started.elapsed().as_millis() - preparation
        );
        if std::env::var("PLAYBACK_ROKU_BIND").is_ok() {
            catalogue.lock().await.push(response);
        } else {
            assert!(manager.stop(&response.id).await);
        }
    }
    if std::env::var("PLAYBACK_ROKU_BIND").is_ok() {
        println!("ROKU_FIXTURES_READY");
        for _ in 0..180 {
            for response in catalogue.lock().await.iter() {
                manager.heartbeat(&response.id).await;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        for response in catalogue.lock().await.iter() {
            manager.stop(&response.id).await;
        }
    }
    assert_eq!(manager.active_count().await, 0);
    manager.shutdown().await;
    origin.abort();
    proxy.abort();
}
