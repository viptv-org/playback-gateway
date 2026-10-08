use super::*;
use axum::{
    body::Bytes,
    extract::{Request, State},
    http::StatusCode,
    middleware as axum_middleware,
    routing::post,
    Router,
};
use http_body_util::BodyExt;
use prost::Message;
use std::sync::Mutex;
use tower::ServiceExt;

#[derive(Clone)]
struct Collector {
    batches: Arc<Mutex<Vec<(String, Vec<u8>)>>>,
    status: Arc<std::sync::atomic::AtomicU16>,
    delay_ms: Arc<AtomicU64>,
}
async fn ingest(State(state): State<Collector>, request: Request) -> StatusCode {
    let path = request.uri().path().to_owned();
    assert_eq!(
        request.headers().get("api-key").unwrap(),
        "fixture-ingest-key-not-real"
    );
    let body = axum::body::to_bytes(request.into_body(), 256 * 1024)
        .await
        .unwrap();
    state.batches.lock().unwrap().push((path, body.to_vec()));
    tokio::time::sleep(Duration::from_millis(
        state.delay_ms.load(Ordering::Relaxed),
    ))
    .await;
    StatusCode::from_u16(state.status.load(Ordering::Relaxed)).unwrap()
}
async fn collector() -> (String, Collector, tokio::task::JoinHandle<()>) {
    let state = Collector {
        batches: Arc::new(Mutex::new(Vec::new())),
        status: Arc::new(std::sync::atomic::AtomicU16::new(200)),
        delay_ms: Arc::new(AtomicU64::new(0)),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/traces", post(ingest))
        .route("/v1/metrics", post(ingest))
        .route("/v1/logs", post(ingest))
        .with_state(state.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (endpoint, state, task)
}
async fn telemetry(endpoint: String, path: PathBuf, rate: f64, daily_bytes: u64) -> Telemetry {
    tokio::task::spawn_blocking(move || {
        Telemetry::build(
            Service::Api,
            config::Settings {
                endpoint,
                key: zeroize::Zeroizing::new("fixture-ingest-key-not-real".into()),
                budget: path,
                daily_bytes,
                sample_rate: rate,
                interval: Duration::from_secs(10),
            },
        )
    })
    .await
    .unwrap()
    .unwrap()
}
async fn flush(telemetry: &Telemetry) {
    let inner = telemetry.inner.clone().unwrap();
    tokio::task::spawn_blocking(move || {
        let _ = inner.traces.force_flush();
        let _ = inner.logs.force_flush();
        let _ = inner.metrics.force_flush();
    })
    .await
    .unwrap();
}
fn trace_spans(collector: &Collector) -> Vec<opentelemetry_proto::tonic::trace::v1::Span> {
    collector
        .batches
        .lock()
        .unwrap()
        .iter()
        .filter(|(path, _)| path == "/v1/traces")
        .flat_map(|(_, bytes)| {
            opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest::decode(
                bytes.as_slice(),
            )
            .unwrap()
            .resource_spans
        })
        .flat_map(|resource| resource.scope_spans)
        .flat_map(|scope| scope.spans)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_otlp_exports_failed_requests_and_metrics_without_sensitive_fields() {
    let (endpoint, collector, server) = collector().await;
    let dir = tempfile::tempdir().unwrap();
    let telemetry = telemetry(endpoint, dir.path().join("quota"), 0.0, 1024 * 1024).await;
    let app = Router::new()
        .route(
            "/api/v2/playback/:id/heartbeat",
            post(|| async { StatusCode::BAD_GATEWAY }),
        )
        .layer(axum_middleware::from_fn_with_state(
            telemetry.clone(),
            middleware::observe,
        ));
    let parent = "00-01010101010101010101010101010101-0202020202020202-00";
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v2/playback/private-capability/heartbeat?token=private-password")
                .header("authorization", "Bearer private-user-key")
                .header("cookie", "private-session-cookie")
                .header("traceparent", parent)
                .header("baggage", "password=private-baggage")
                .header("tracestate", "private-user=private-state")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    flush(&telemetry).await;
    let spans = trace_spans(&collector);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].trace_id, vec![1; 16]);
    assert_eq!(spans[0].name, "playback.heartbeat");
    assert_eq!(spans[0].status.as_ref().unwrap().code, 2);
    let batches = collector.batches.lock().unwrap().clone();
    assert!(batches.iter().any(|(path, _)| path == "/v1/metrics"));
    assert!(batches.iter().any(|(path, _)| path == "/v1/logs"));
    for (_, bytes) in batches {
        let text = String::from_utf8_lossy(&bytes);
        for secret in [
            "private-capability",
            "private-password",
            "private-user-key",
            "private-session-cookie",
            "private-baggage",
            "private-state",
            "fixture-ingest-key-not-real",
        ] {
            assert!(
                !text.contains(secret),
                "telemetry exposed an unapproved field"
            );
        }
    }
    telemetry.shutdown().await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn success_sampling_and_internal_cross_service_parenting_are_deterministic() {
    let (endpoint, collector, server) = collector().await;
    let dir = tempfile::tempdir().unwrap();
    let telemetry = telemetry(endpoint, dir.path().join("quota"), 0.0, 1024 * 1024).await;
    let parent = telemetry.start(Operation::PlaybackStart, Context::new(), SpanKind::Server);
    let parent_id = parent
        .context()
        .context
        .span()
        .span_context()
        .span_id()
        .to_bytes();
    in_context(parent.context(), async {
        let child = observe(Operation::GatewayControl);
        in_context(child.context(), async {
            assert!(traceparent().unwrap().starts_with("00-"));
        })
        .await;
        child.finish(Outcome::Failed(Failure::Dns));
    })
    .await;
    parent.finish(Outcome::Success);
    flush(&telemetry).await;
    let spans = trace_spans(&collector);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].name, "gateway.control");
    assert_eq!(spans[0].parent_span_id, parent_id);
    telemetry.shutdown().await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collector_outage_and_full_buffers_do_not_block_request_recording() {
    let (endpoint, collector, server) = collector().await;
    let dir = tempfile::tempdir().unwrap();
    collector.delay_ms.store(3000, Ordering::Relaxed);
    collector.status.store(503, Ordering::Relaxed);
    let telemetry = telemetry(endpoint, dir.path().join("quota"), 1.0, 1024 * 1024).await;
    let began = Instant::now();
    for _ in 0..3000 {
        telemetry
            .start(Operation::Api, Context::new(), SpanKind::Server)
            .finish(Outcome::Success);
    }
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "recording waited on the collector"
    );
    let began = Instant::now();
    telemetry.shutdown().await;
    assert!(
        began.elapsed() < Duration::from_secs(8),
        "shutdown was unbounded"
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_blocks_network_and_restart_cannot_reset_it() {
    let (endpoint, collector, server) = collector().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("quota");
    let first = telemetry(endpoint.clone(), path.clone(), 1.0, 1).await;
    first
        .start(Operation::Api, Context::new(), SpanKind::Server)
        .finish(Outcome::Failed(Failure::Network));
    flush(&first).await;
    assert!(collector.batches.lock().unwrap().is_empty());
    first.shutdown().await;
    let second = telemetry(endpoint, path, 1.0, 1).await;
    second
        .start(Operation::Api, Context::new(), SpanKind::Server)
        .finish(Outcome::Success);
    flush(&second).await;
    assert!(collector.batches.lock().unwrap().is_empty());
    second.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn disabled_streaming_layer_preserves_data_and_trailers_without_initialization() {
    let app = Router::new()
        .route(
            "/media/:viewer/:token/:file",
            axum::routing::get(|| async { Bytes::from_static(b"video-bytes") }),
        )
        .layer(axum_middleware::from_fn_with_state(
            Telemetry::default(),
            middleware::observe,
        ));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/media/viewer/token/segment.ts")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "video-bytes"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enabled_streaming_tracks_completion_and_drop_without_buffering_content() {
    let (endpoint, collector, server) = collector().await;
    let dir = tempfile::tempdir().unwrap();
    let telemetry = telemetry(endpoint, dir.path().join("quota"), 1.0, 1024 * 1024).await;
    let app = Router::new()
        .route(
            "/media/:viewer/:token/:file",
            axum::routing::get(|| async { Bytes::from_static(b"source-payload-not-a-log") }),
        )
        .layer(axum_middleware::from_fn_with_state(
            telemetry.clone(),
            middleware::observe,
        ));
    let request = || {
        Request::builder()
            .uri("/media/viewer/private-token/segment.ts")
            .body(axum::body::Body::empty())
            .unwrap()
    };
    let response = app.clone().oneshot(request()).await.unwrap();
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "source-payload-not-a-log"
    );
    let response = app.oneshot(request()).await.unwrap();
    drop(response);
    flush(&telemetry).await;
    let spans = trace_spans(&collector);
    assert_eq!(spans.len(), 2);
    assert!(spans.iter().any(|span| span
        .status
        .as_ref()
        .is_some_and(|status| status.message == "cancelled")));
    for (_, bytes) in collector.batches.lock().unwrap().iter() {
        assert!(!String::from_utf8_lossy(bytes).contains("source-payload-not-a-log"));
    }
    telemetry.shutdown().await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_preserves_trailers_and_propagates_body_errors() {
    let (endpoint, collector, server) = collector().await;
    let dir = tempfile::tempdir().unwrap();
    let telemetry = telemetry(endpoint, dir.path().join("quota"), 1.0, 1024 * 1024).await;
    let app = Router::new()
        .route(
            "/media/trailers",
            axum::routing::get(|| async {
                let mut trailers = http::HeaderMap::new();
                trailers.insert(
                    "x-checksum",
                    http::HeaderValue::from_static("fixture-checksum"),
                );
                let frames = futures::stream::iter(vec![
                    Ok::<_, std::io::Error>(http_body::Frame::data(Bytes::from_static(
                        b"unchanged",
                    ))),
                    Ok(http_body::Frame::trailers(trailers)),
                ]);
                axum::body::Body::new(http_body_util::StreamBody::new(frames))
            }),
        )
        .route(
            "/media/error",
            axum::routing::get(|| async {
                let frames = futures::stream::iter(vec![
                    Ok(http_body::Frame::data(Bytes::from_static(b"first-frame"))),
                    Err(std::io::Error::other("private-body-failure")),
                ]);
                axum::body::Body::new(http_body_util::StreamBody::new(frames))
            }),
        )
        .layer(axum_middleware::from_fn_with_state(
            telemetry.clone(),
            middleware::observe,
        ));
    let request = |path: &str| {
        Request::builder()
            .uri(path)
            .body(axum::body::Body::empty())
            .unwrap()
    };
    let response = app
        .clone()
        .oneshot(request("/media/trailers"))
        .await
        .unwrap();
    let collected = response.into_body().collect().await.unwrap();
    assert_eq!(
        collected.trailers().unwrap()["x-checksum"],
        "fixture-checksum"
    );
    assert_eq!(collected.to_bytes(), "unchanged");
    let response = app.oneshot(request("/media/error")).await.unwrap();
    assert!(response.into_body().collect().await.is_err());
    flush(&telemetry).await;
    assert!(trace_spans(&collector).iter().any(|span| span
        .status
        .as_ref()
        .is_some_and(|status| status.message == "network")));
    for (_, bytes) in collector.batches.lock().unwrap().iter() {
        assert!(!String::from_utf8_lossy(bytes).contains("private-body-failure"));
        assert!(!String::from_utf8_lossy(bytes).contains("fixture-checksum"));
    }
    telemetry.shutdown().await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collector_http_failure_is_not_retried_and_consumes_persisted_budget() {
    let (endpoint, collector, server) = collector().await;
    collector.status.store(503, Ordering::Relaxed);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("quota");
    let first = telemetry(endpoint.clone(), path.clone(), 1.0, 1024 * 1024).await;
    first
        .start(Operation::Api, Context::new(), SpanKind::Server)
        .finish(Outcome::Success);
    let provider = first.inner.clone().unwrap();
    tokio::task::spawn_blocking(move || {
        let _ = provider.traces.force_flush();
    })
    .await
    .unwrap();
    assert_eq!(
        collector
            .batches
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == "/v1/traces")
            .count(),
        1
    );
    first.shutdown().await;
    let before: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(before["bytes"].as_u64().unwrap() > 0);
    let second = telemetry(endpoint, path.clone(), 1.0, 1024 * 1024).await;
    let after: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(before, after);
    second.shutdown().await;
    server.abort();
}
