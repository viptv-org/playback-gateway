//! Generic API v1. Control credentials never authorize media, and media URLs
//! never authorize control operations. No request-path/body logging is installed.
use crate::{
    credentials::{Principal, Scope},
    key_api::{self, ApiError},
    protocol::{CreateSession, API_VERSION, RENEW_AFTER_SECONDS, VIEWER_LEASE_SECONDS},
    sessions::{Status, Viewer},
    workers::Workers,
};
use axum::{
    extract::{rejection::JsonRejection, DefaultBodyLimit, Path, Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use serde::Serialize;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(Clone)]
pub struct Api {
    pub workers: Arc<Workers>,
    pub ready: Arc<AtomicBool>,
}
#[derive(Serialize)]
struct SessionResponse {
    #[serde(flatten)]
    viewer: Viewer,
    renew_after_seconds: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    playback: Option<playback_engine::PlaybackResponse>,
}

pub fn router(state: Api) -> Router {
    let control = Router::new()
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/sessions", post(create))
        .route("/v1/sessions/:id", get(session).delete(release))
        .route("/v1/sessions/:id/renew", post(renew))
        .layer(DefaultBodyLimit::max(256 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate));
    let keys = key_api::router(key_api::KeyApi {
        credentials: state.workers.credentials.clone(),
        sessions: state.workers.sessions.clone(),
    });
    Router::new()
        .merge(control)
        .route(
            "/media/:viewer/:token/:file",
            get(media).options(media_options),
        )
        .route(
            "/health",
            get(|| async { Json(serde_json::json!({"status":"ok"})) }),
        )
        .route("/ready", get(ready))
        .fallback(|| async { ApiError("not_found") })
        .method_not_allowed_fallback(|| async { ApiError("method_not_allowed") })
        .with_state(state)
        .merge(keys)
        .layer(middleware::map_response(key_api::private_response))
}
fn now() -> Result<u64, ApiError> {
    Ok(key_api::now()? as u64)
}

async fn authenticate(State(state): State<Api>, mut request: Request, next: Next) -> Response {
    let token = match key_api::bearer(request.headers()) {
        Ok(token) => token,
        Err(error) => return error.into_response(),
    };
    let timestamp = match key_api::now() {
        Ok(timestamp) => timestamp,
        Err(error) => return error.into_response(),
    };
    let credentials = state.workers.credentials.clone();
    let result = tokio::task::spawn_blocking(move || credentials.authenticate(&token, timestamp))
        .await
        .map_err(|_| ApiError("credential_store_unavailable"))
        .and_then(|result| result.map_err(ApiError));
    match result {
        Ok(principal) => {
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        Err(error) => error.into_response(),
    }
}
fn payload<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    body.map(|Json(value)| value).map_err(|error| {
        ApiError(if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
            "request_too_large"
        } else {
            "invalid_request"
        })
    })
}
fn reply(
    state: &Api,
    principal: &Principal,
    viewer: Viewer,
    timestamp: u64,
) -> Result<SessionResponse, ApiError> {
    let playback = state
        .workers
        .delivery(principal, &viewer, timestamp)
        .map_err(ApiError)?;
    Ok(SessionResponse {
        viewer,
        renew_after_seconds: RENEW_AFTER_SECONDS,
        playback,
    })
}
async fn create(
    State(state): State<Api>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    body: Result<Json<CreateSession>, JsonRejection>,
) -> Result<Response, ApiError> {
    if !state.ready.load(Ordering::Acquire) {
        return Err(ApiError("not_ready"));
    }
    let request = payload(body)?;
    if headers.get_all("idempotency-key").iter().count() != 1 {
        return Err(ApiError("invalid_idempotency_key"));
    }
    let idempotency = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .ok_or(ApiError("invalid_idempotency_key"))?;
    let viewer = state
        .workers
        .create(&principal, &request, idempotency, now()?)
        .await
        .map_err(ApiError)?;
    let status = if viewer.status == Status::Starting {
        StatusCode::ACCEPTED
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(reply(&state, &principal, viewer, now()?)?)).into_response())
}
async fn session(
    State(state): State<Api>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<SessionResponse>, ApiError> {
    let timestamp = now()?;
    let viewer = state
        .workers
        .sessions
        .lock()
        .map_err(|_| ApiError("session_store_unavailable"))?
        .get(&principal, &id, timestamp)
        .map_err(ApiError)?;
    Ok(Json(reply(&state, &principal, viewer, timestamp)?))
}
async fn renew(
    State(state): State<Api>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<SessionResponse>, ApiError> {
    let timestamp = now()?;
    let viewer = state
        .workers
        .sessions
        .lock()
        .map_err(|_| ApiError("session_store_unavailable"))?
        .renew(&principal, &id, timestamp)
        .map_err(ApiError)?;
    Ok(Json(reply(&state, &principal, viewer, timestamp)?))
}
async fn release(
    State(state): State<Api>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state
        .workers
        .sessions
        .lock()
        .map_err(|_| ApiError("session_store_unavailable"))?
        .release(&principal, &id, now()?)
        .map_err(ApiError)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn capabilities(
    State(state): State<Api>,
    Extension(principal): Extension<Principal>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !principal.key.spec.scopes.contains(&Scope::Capabilities) {
        return Err(ApiError("forbidden"));
    }
    let available=state.workers.available(&principal,now()?).map_err(ApiError)?;
    Ok(Json(
        serde_json::json!({"version":API_VERSION, "ready":state.ready.load(Ordering::Acquire), "protocols":["hls","progressive"], "viewer_lease_seconds":VIEWER_LEASE_SECONDS, "renew_after_seconds":RENEW_AFTER_SECONDS, "namespaces":principal.key.spec.namespaces, "scopes":principal.key.spec.scopes, "limits":principal.key.spec.quotas,"available":available}),
    ))
}
async fn ready(State(state): State<Api>) -> Response {
    if state.ready.load(Ordering::Acquire) {
        Json(serde_json::json!({"status":"ready"})).into_response()
    } else {
        ApiError("not_ready").into_response()
    }
}
fn media_cors(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
    response.headers_mut().insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        "Content-Length,Content-Range,Accept-Ranges"
            .parse()
            .unwrap(),
    );
    response
}
async fn media(
    State(state): State<Api>,
    Path((viewer, token, file)): Path<(String, String, String)>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let response = match now() {
        Ok(timestamp) => match state
            .workers
            .media(&viewer, &token, &file, timestamp, method, headers)
            .await
        {
            Ok(response) => response,
            Err(code) => ApiError(code).into_response(),
        },
        Err(error) => error.into_response(),
    };
    media_cors(response)
}
async fn media_options() -> Response {
    let mut response = media_cors(StatusCode::NO_CONTENT.into_response());
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        "GET, HEAD, OPTIONS".parse().unwrap(),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        "Range".parse().unwrap(),
    );
    response
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::workers::tests::{fixture, request, BOOTSTRAP};
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use serde_json::{json, Value};
    use tower::ServiceExt;

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        token: &str,
        idem: Option<&str>,
        body: String,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json");
        if let Some(idem) = idem {
            request = request.header("idempotency-key", idem);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, headers, bytes.to_vec())
    }

    #[tokio::test]
    async fn http_session_lifecycle_keeps_media_credentials_scoped_and_stable() {
        let (root, workers, p) = fixture(false).await;
        let app = router(Api {
            workers: workers.clone(),
            ready: Arc::new(AtomicBool::new(true)),
        });
        let mut spec = p.key.spec.clone();
        spec.scopes.push(Scope::Capabilities);
        let (status, _, issued) = call(
            &app,
            "POST",
            "/v1/keys",
            BOOTSTRAP,
            None,
            serde_json::to_string(&spec).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let issued: Value = serde_json::from_slice(&issued).unwrap();
        let key = issued["secret"].as_str().unwrap();
        let (_, _, capabilities) = call(&app, "GET", "/v1/capabilities", key, None, String::new()).await;
        let capabilities: Value = serde_json::from_slice(&capabilities).unwrap();
        for scope in ["capabilities", "create", "read", "renew", "release"] {
            assert!(capabilities["scopes"].as_array().unwrap().contains(&json!(scope)));
        }
        assert_eq!(
            call(&app, "GET", "/v1/capabilities", key, None, String::new())
                .await
                .0,
            StatusCode::OK
        );
        let body = serde_json::to_string(&request(1920)).unwrap();
        let (status, _, first) =
            call(&app, "POST", "/v1/sessions", key, Some("one"), body.clone()).await;
        assert_eq!(status, StatusCode::CREATED);
        let first: Value = serde_json::from_slice(&first).unwrap();
        let id = first["id"].as_str().unwrap();
        let url = first["playback"]["url"].as_str().unwrap();
        assert!(url.starts_with(&format!("/media/{id}/pgm_")));
        assert_eq!(first["playback"]["id"], id);
        let (_, _, replay) = call(&app, "POST", "/v1/sessions", key, Some("one"), body).await;
        let replay: Value = serde_json::from_slice(&replay).unwrap();
        assert_eq!(replay["id"], id);
        assert_eq!(replay["playback"]["url"], url);
        assert_eq!(
            std::fs::read_to_string(root.path().join("encoder.count")).unwrap(),
            "x"
        );
        let (status, headers, playlist) = call(&app, "GET", url, "", None, String::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
        let playlist = String::from_utf8(playlist).unwrap();
        assert!(playlist.contains("segment-000000000.ts"));
        let segment = url.replace("index.m3u8", "segment-000000000.ts");
        assert_eq!(
            call(&app, "GET", &segment, "", None, String::new()).await.2,
            b"fixture"
        );
        assert!(call(&app, "HEAD", &segment, "", None, String::new())
            .await
            .2
            .is_empty());
        let media_token = url.split('/').nth(3).unwrap();
        assert_eq!(
            call(
                &app,
                "GET",
                "/v1/capabilities",
                media_token,
                None,
                String::new()
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        let other = workers.credentials.issue(BOOTSTRAP, spec, 0).unwrap();
        assert_eq!(
            call(
                &app,
                "GET",
                &format!("/v1/sessions/{id}"),
                &other.secret,
                None,
                String::new()
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(
                &app,
                "POST",
                &format!("/v1/sessions/{id}/renew"),
                key,
                None,
                String::new()
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &app,
                "DELETE",
                &format!("/v1/sessions/{id}"),
                key,
                None,
                String::new()
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(&app, "GET", url, "", None, String::new()).await.0,
            StatusCode::UNAUTHORIZED
        );
        workers.reap(now().unwrap() + 16).await.unwrap();
        workers.shutdown().await;
    }

    #[tokio::test]
    async fn authentication_readiness_and_invalid_bodies_fail_without_secret_echo() {
        let (_root, workers, p) = fixture(false).await;
        let ready = Arc::new(AtomicBool::new(false));
        let app = router(Api {
            workers: workers.clone(),
            ready: ready.clone(),
        });
        let key = workers.credentials.issue(BOOTSTRAP, p.key.spec, 0).unwrap();
        assert_eq!(
            call(&app, "GET", "/health", "", None, String::new())
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            call(&app, "GET", "/ready", "", None, String::new()).await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let sensitive = "fixture-source-password-invalid-json";
        let (status, _, error) = call(
            &app,
            "POST",
            "/v1/sessions",
            BOOTSTRAP,
            Some("one"),
            sensitive.into(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(!String::from_utf8(error).unwrap().contains(sensitive));
        assert_eq!(
            call(
                &app,
                "POST",
                "/v1/sessions",
                &key.secret,
                Some("one"),
                json!({}).to_string()
            )
            .await
            .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        ready.store(true, Ordering::Release);
        let (status, _, error) = call(
            &app,
            "POST",
            "/v1/sessions",
            &key.secret,
            Some("one"),
            sensitive.into(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(!String::from_utf8(error).unwrap().contains(sensitive));
        assert_eq!(
            call(
                &app,
                "POST",
                "/v1/sessions",
                &key.secret,
                Some("one"),
                "x".repeat(257 * 1024)
            )
            .await
            .0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            call(
                &app,
                "PATCH",
                "/v1/sessions",
                &key.secret,
                None,
                String::new()
            )
            .await
            .0,
            StatusCode::METHOD_NOT_ALLOWED
        );
        workers.shutdown().await;
    }
}
