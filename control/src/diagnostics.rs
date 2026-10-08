use axum::{extract::Request, middleware::Next, response::Response};
use sha2::{Digest, Sha256};
use std::time::Instant;
use tracing::Instrument;

pub(crate) fn tag(value: &str) -> String {
    Sha256::digest(value.as_bytes())[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn operation(path: &str) -> &'static str {
    if path == "/v1/sessions" {
        "create"
    } else if path.starts_with("/v1/sessions/") && path.ends_with("/renew") {
        "renew"
    } else if path.starts_with("/v1/sessions/") {
        "session"
    } else if path.starts_with("/media/") {
        "media"
    } else if path == "/v1/capabilities" {
        "capabilities"
    } else {
        "other"
    }
}
pub(crate) async fn observe(request: Request, next: Next) -> Response {
    let op = operation(request.uri().path());
    let parts: Vec<_> = request.uri().path().split('/').collect();
    let viewer = if op == "media" {
        parts.get(2)
    } else if op == "session" || op == "renew" {
        parts.get(3)
    } else {
        None
    };
    let session_tag = viewer.map(|id| tag(id)).unwrap_or_default();
    let trace = request
        .headers()
        .get("x-playback-trace")
        .and_then(|h| h.to_str().ok())
        .filter(|s| s.len() <= 48 && s.bytes().all(|b| b.is_ascii_digit() || b == b'-'))
        .unwrap_or("none")
        .to_owned();
    let started = Instant::now();
    let span = tracing::info_span!("gateway_request", %trace, operation = op, %session_tag);
    async move {
        let response = next.run(request).await;
        let status = response.status().as_u16();
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if status >= 400 {
            tracing::warn!(status, elapsed_ms, "Gateway request failed");
        } else if op != "media" && op != "other" {
            tracing::info!(status, elapsed_ms, "Gateway request completed");
        } else if elapsed_ms > 1000 {
            tracing::warn!(status, elapsed_ms, "Gateway response slow");
        }
        response
    }
    .instrument(span)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };
    use tower::ServiceExt;
    use tracing::instrument::WithSubscriber;
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[tokio::test]
    async fn failed_media_records_status_but_never_capability_token() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let writer = Capture(bytes.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let app = axum::Router::new()
            .route(
                "/media/:viewer/:token/:file",
                axum::routing::get(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
            )
            .layer(axum::middleware::from_fn(observe));
        let request = Request::builder()
            .uri("/media/private-viewer/never-print/secret.ts?password=secret")
            .header("x-playback-trace", "12345-6")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app
            .oneshot(request)
            .with_subscriber(subscriber)
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 503);
        let logs = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        for fact in ["media", "503", "elapsed_ms", "12345-6", "session_tag"] {
            assert!(logs.contains(fact), "missing diagnostic {fact}");
        }
        for secret in ["private-viewer", "never-print", "secret.ts", "password="] {
            assert!(!logs.contains(secret));
        }
    }
    #[test]
    fn media_token_and_source_are_not_diagnostic_fields() {
        assert_eq!(
            operation("/media/viewer/private-token/private-name.ts"),
            "media"
        );
        assert_eq!(operation("/v1/sessions/private-id/renew"), "renew");
        assert_eq!(tag("private-secret").len(), 16);
        assert!(!tag("private-secret").contains("private-secret"));
    }
}
