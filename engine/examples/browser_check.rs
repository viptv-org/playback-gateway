//! Isolated media-engine qualification. Fixed synthetic HTTPS fixtures only; no database/accounts.
use axum::{
    extract::{Path, State},
    http::{HeaderMap, Method, StatusCode},
    response::Response,
    routing::{delete, get, post},
    Json, Router,
};
use playback_engine::{Capabilities, Config, PlaybackManager, PlaybackResponse, TrackSelection};
use serde::Deserialize;
use std::{collections::HashMap, sync::Arc, time::Duration};

#[derive(Clone)]
struct Check {
    engine: Arc<PlaybackManager>,
    origin: String,
}
#[derive(Deserialize)]
struct Start {
    fixture: String,
    #[serde(default)]
    position: f64,
    capabilities: Capabilities,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    live: bool,
}
async fn start(
    State(state): State<Check>,
    Json(input): Json<Start>,
) -> Result<Json<PlaybackResponse>, (StatusCode, String)> {
    if ![
        "h264-aac.mkv",
        "h264-ac3.mkv",
        "continuous-ac3.ts",
        "h264-avi.avi",
        "mpeg2.mkv",
        "subtitles.mkv",
        "hevc-4k-main10.mkv",
    ]
    .contains(&input.fixture.as_str())
    {
        return Err((StatusCode::BAD_REQUEST, "Unknown fixture".into()));
    }
    let response = state
        .engine
        .start_with_selection(
            format!("{}/{}", state.origin, input.fixture),
            HashMap::new(),
            input.position,
            Some(input.capabilities),
            input.force,
            input.live,
            None,
            TrackSelection::default(),
        )
        .await
        .map_err(|error| (StatusCode::NOT_ACCEPTABLE, error))?;
    println!(
        "fixture={} mode={} video={} audio={}",
        input.fixture, response.mode, response.video_mode, response.audio_mode
    );
    Ok(Json(response))
}
async fn media(
    State(state): State<Check>,
    Path((id, cap, file)): Path<(String, String, String)>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    if let Some(result) = state
        .engine
        .serve_original(&id, &cap, &file, method, headers)
        .await
    {
        return result.map_err(|e| (StatusCode::BAD_GATEWAY, e));
    }
    state
        .engine
        .serve(&id, &cap, &file)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, e))
}
async fn stop(State(state): State<Check>, Path(id): Path<String>) -> StatusCode {
    state.engine.stop(&id).await;
    StatusCode::NO_CONTENT
}

#[tokio::main]
async fn main() {
    let origin = std::env::var("MEDIA_SOURCE_BASE").expect("MEDIA_SOURCE_BASE");
    assert!(origin.starts_with("https://"));
    let root = tempfile::tempdir().unwrap();
    let engine = PlaybackManager::new(Config {
        ffmpeg: "/usr/bin/ffmpeg".into(),
        ffprobe: "/usr/bin/ffprobe".into(),
        root: root.path().join("media"),
        max_sessions: 3,
        ttl: Duration::from_secs(90),
    });
    let state = Check {
        engine: engine.clone(),
        origin,
    };
    let app = Router::new()
        .route("/engine/start", post(start))
        .route("/engine/session/:id", delete(stop))
        .route("/media/:id/:cap/:file", get(media).head(media))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:18182")
        .await
        .unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .unwrap();
    engine.shutdown().await;
}
