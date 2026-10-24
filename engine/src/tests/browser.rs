use super::*;
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Method, StatusCode},
    response::Response,
    routing::get,
    Router,
};

async fn fixture(bytes: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
    let bytes = Bytes::from(bytes);
    let app = Router::new().route(
        "/media",
        get(move |headers: HeaderMap| {
            let bytes = bytes.clone();
            async move {
                let range = headers
                    .get("range")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|h| h.strip_prefix("bytes="))
                    .and_then(|h| h.split_once('-'));
                let mut builder = Response::builder().header("Accept-Ranges", "bytes");
                if let Some((start, end)) = range {
                    let start = start.parse::<usize>().unwrap_or(0);
                    let end = end
                        .parse::<usize>()
                        .unwrap_or(bytes.len() - 1)
                        .min(bytes.len() - 1);
                    if start >= bytes.len() {
                        return builder.status(416).body(Body::empty()).unwrap();
                    }
                    builder = builder.status(206).header(
                        "Content-Range",
                        format!("bytes {start}-{end}/{}", bytes.len()),
                    );
                    builder.body(Body::from(bytes.slice(start..=end))).unwrap()
                } else {
                    builder.body(Body::from(bytes)).unwrap()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/media", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }),
    )
}
fn browser_caps(direct: bool) -> Capabilities {
    serde_json::from_value(serde_json::json!({"direct_play":direct,"max_width":1920,"max_height":1080,
        "browser":{"version":1,"inspect_original":true,"local_remux":true,"fmp4":true,"engines":[]}})).unwrap()
}

#[tokio::test]
async fn client_inspection_needs_no_ffprobe_or_encoder_and_revokes_media() {
    std::env::set_var("PLAYBACK_BROWSER_PREPARATION", "1");
    let root = tempfile::tempdir().unwrap();
    let manager = PlaybackManager::new(Config {
        ffmpeg: root.path().join("absent-encoder"),
        ffprobe: root.path().join("absent-probe"),
        root: root.path().join("media"),
        max_sessions: 1,
        ttl: Duration::from_secs(30),
    });
    let (url, task) = fixture(vec![42; 4096]).await;
    let response = manager
        .start(url, HashMap::new(), 9.0, Some(browser_caps(true)), false)
        .await
        .unwrap();
    assert_eq!(response.mode, "direct");
    assert_eq!(response.position, 9.0);
    assert!(serde_json::to_value(&response)
        .unwrap()
        .get("authorization")
        .is_none());
    let parts = response.url.split('/').collect::<Vec<_>>();
    let served = manager
        .serve_original(parts[2], parts[3], parts[4], Method::GET, HeaderMap::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(served.status(), StatusCode::OK);
    let body = axum::body::to_bytes(served.into_body(), 8192)
        .await
        .unwrap();
    assert_eq!(body, vec![42; 4096]);
    manager.stop(&response.id).await;
    assert!(manager
        .serve_original(parts[2], parts[3], parts[4], Method::GET, HeaderMap::new())
        .await
        .is_none());
    manager.shutdown().await;
    task.abort();
    std::env::remove_var("PLAYBACK_BROWSER_PREPARATION");
}

#[cfg(unix)]
#[tokio::test]
async fn identical_concurrent_probes_share_one_child() {
    let root = tempfile::tempdir().unwrap();
    let manager = scripted_probe(root.path(), "sleep 0.05; printf '%s' '{\"streams\":[]}'");
    let results = futures::future::join_all(
        (0..8).map(|_| manager.cached_probe("https://example.invalid/movie", "", false, None)),
    )
    .await;
    assert!(results.iter().all(Option::is_some));
    assert_eq!(
        std::fs::read(root.path().join("probe.sh.count"))
            .unwrap()
            .len(),
        1
    );
    manager.shutdown().await;
}

#[tokio::test]
#[ignore = "requires real FFmpeg/libx264 and ffprobe binaries"]
async fn copied_seek_and_audio_conversion_stream_without_video_encoding() {
    let ffmpeg = PathBuf::from(std::env::var("PLAYBACK_TEST_FFMPEG").unwrap());
    let ffprobe = PathBuf::from(std::env::var("PLAYBACK_TEST_FFPROBE").unwrap());
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("source.mkv");
    let status = Command::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "12",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "300",
            "-keyint_min",
            "300",
            "-sc_threshold",
            "0",
            "-c:a",
            "ac3",
            "-ac",
            "2",
        ])
        .arg(&file)
        .status()
        .await
        .unwrap();
    assert!(status.success());
    let (url, task) = fixture(tokio::fs::read(&file).await.unwrap()).await;
    let manager = PlaybackManager::new(Config {
        ffmpeg,
        ffprobe: ffprobe.clone(),
        root: root.path().join("media"),
        max_sessions: 1,
        ttl: Duration::from_secs(30),
    });
    let response = manager
        .start(
            url.clone(),
            HashMap::new(),
            7.0,
            Some(browser_caps(false)),
            false,
        )
        .await
        .unwrap();
    assert_eq!(response.format, "fmp4");
    assert_eq!(response.video_mode, "copy");
    assert_eq!(response.audio_mode, "encode");
    assert!(
        response.position < 1.0,
        "the preceding keyframe supplies preroll"
    );
    let parts = response.url.split('/').collect::<Vec<_>>();
    let served = manager
        .serve_original(parts[2], parts[3], parts[4], Method::GET, HeaderMap::new())
        .await
        .unwrap()
        .unwrap();
    let bytes = timeout(
        Duration::from_secs(20),
        axum::body::to_bytes(served.into_body(), 16 * 1024 * 1024),
    )
    .await
    .unwrap()
    .unwrap();
    let output = root.path().join("output.mp4");
    tokio::fs::write(&output, bytes).await.unwrap();
    let probe = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "json",
        ])
        .arg(&output)
        .output()
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    assert_eq!(json["streams"][0]["codec_name"], "h264");
    assert_eq!(json["streams"][1]["codec_name"], "aac");
    manager.stop(&response.id).await;
    let audio_retry = manager
        .start_with_selection(
            url,
            HashMap::new(),
            7.0,
            Some(browser_caps(false)),
            true,
            false,
            None,
            TrackSelection {
                conversion_reason: Some("audio-codec".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        audio_retry.video_mode, "copy",
        "an audio refusal must not encode compatible video"
    );
    assert_eq!(audio_retry.audio_mode, "encode");
    manager.stop(&audio_retry.id).await;
    let (aac_url, aac_task) = fixture(tokio::fs::read(&output).await.unwrap()).await;
    let video_retry = manager
        .start_with_selection(
            aac_url,
            HashMap::new(),
            0.0,
            Some(browser_caps(false)),
            true,
            false,
            None,
            TrackSelection {
                conversion_reason: Some("video-codec".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(video_retry.video_mode, "encode");
    assert_eq!(
        video_retry.audio_mode, "copy",
        "a video refusal must preserve compatible AAC"
    );
    manager.stop(&video_retry.id).await;
    aac_task.abort();
    manager.shutdown().await;
    task.abort();
}

#[tokio::test]
#[ignore = "requires real FFmpeg/libx265 and ffprobe binaries"]
async fn qualified_live_hevc_audio_conversion_keeps_video_and_continuous_progress() {
    let ffmpeg = PathBuf::from(std::env::var("PLAYBACK_TEST_FFMPEG").unwrap());
    let ffprobe = PathBuf::from(std::env::var("PLAYBACK_TEST_FFPROBE").unwrap());
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("live.mkv");
    let fixture_ffmpeg = std::env::var("PLAYBACK_FIXTURE_FFMPEG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| ffmpeg.clone());
    let result = Command::new(fixture_ffmpeg)
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=24",
            "-f",
            "lavfi",
            "-i",
            "sine=sample_rate=48000",
            "-t",
            "6",
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "log-level=error:pools=1:keyint=24",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "ac3",
            "-ac",
            "2",
        ])
        .arg(&file)
        .output()
        .await
        .unwrap();
    assert!(result.status.success());
    let (url, task) = fixture(tokio::fs::read(file).await.unwrap()).await;
    let manager = PlaybackManager::new(Config {
        ffmpeg,
        ffprobe,
        root: root.path().join("media"),
        max_sessions: 1,
        ttl: Duration::from_secs(30),
    });
    let mut caps = browser_caps(false);
    caps.browser.as_mut().unwrap().engines = serde_json::from_value(serde_json::json!([{
        "engine":"mse", "codec":"hevc", "evidence":"decoded", "max_width":1920, "max_height":1080, "max_frame_rate":30, "bit_depth":8, "max_level":150, "hdr":false
    }])).unwrap();
    let response = manager
        .start_with_selection(
            url,
            HashMap::new(),
            0.0,
            Some(caps),
            false,
            true,
            None,
            TrackSelection {
                conversion_reason: Some("audio-codec".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(response.video_mode, "copy");
    assert_eq!(response.audio_mode, "encode");
    assert_eq!(
        response.format, "fmp4",
        "copied live GOPs must not depend on an HLS target duration"
    );
    assert!(response.live);
    assert!(manager.live_progress(&response.id).await.is_some());
    let parts = response.url.split('/').collect::<Vec<_>>();
    let served = manager
        .serve_original(parts[2], parts[3], parts[4], Method::GET, HeaderMap::new())
        .await
        .unwrap()
        .unwrap();
    let bytes = timeout(
        Duration::from_secs(20),
        axum::body::to_bytes(served.into_body(), 16 * 1024 * 1024),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(bytes.windows(4).any(|window| window == b"moof"));
    assert!(bytes.windows(4).any(|window| window == b"hvc1"));
    manager.stop(&response.id).await;
    manager.shutdown().await;
    task.abort();
}
