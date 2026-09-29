//! Bootstrap-only management routes. This router deliberately does not advertise
//! playback readiness until the media runtime has been integrated and verified.
use crate::{
    credentials::{Credentials, KeySpec},
    sessions::Sessions,
};
use axum::{
    extract::{rejection::JsonRejection, DefaultBodyLimit, Path, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get},
    Json, Router,
};
use std::{
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
pub struct KeyApi {
    pub credentials: Arc<Credentials>,
    pub sessions: Arc<Mutex<Sessions>>,
}

pub fn router(state: KeyApi) -> Router {
    Router::new()
        .route("/v1/keys", get(list).post(issue))
        .route("/v1/keys/:id", delete(revoke))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .layer(middleware::map_response(private_response))
        .with_state(state)
}

pub(crate) fn bearer(headers: &HeaderMap) -> Result<String, ApiError> {
    if headers.get_all(header::AUTHORIZATION).iter().count() != 1 {
        return Err(ApiError("unauthorized"));
    }
    let raw = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or(ApiError("unauthorized"))?;
    let (scheme, token) = raw.split_once(' ').ok_or(ApiError("unauthorized"))?;
    if !scheme.eq_ignore_ascii_case("bearer")
        || token.is_empty()
        || token.len() > 256
        || token.chars().any(char::is_whitespace)
    {
        return Err(ApiError("unauthorized"));
    }
    Ok(token.into())
}

async fn authenticate(State(state): State<KeyApi>, request: Request, next: Next) -> Response {
    match bearer(request.headers()) {
        Ok(token) if state.credentials.is_bootstrap(&token) => next.run(request).await,
        _ => ApiError("unauthorized").into_response(),
    }
}

pub(crate) async fn private_response(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
}

pub(crate) fn now() -> Result<i64, ApiError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .ok_or(ApiError("clock_unavailable"))
}

#[derive(Debug)]
pub(crate) struct ApiError(pub(crate) &'static str);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.0 {
            "unauthorized" | "media_unauthorized" => StatusCode::UNAUTHORIZED,
            "forbidden" => StatusCode::FORBIDDEN,
            "session_not_found" | "media_not_found" | "not_found" => StatusCode::NOT_FOUND,
            "method_not_allowed" => StatusCode::METHOD_NOT_ALLOWED,
            "session_expired" => StatusCode::GONE,
            "idempotency_conflict" | "input_cleanup_pending" => StatusCode::CONFLICT,
            "invalid_key_configuration"
            | "invalid_request"
            | "invalid_input"
            | "invalid_headers"
            | "invalid_output"
            | "invalid_position"
            | "invalid_namespace"
            | "invalid_idempotency_key" => StatusCode::BAD_REQUEST,
            "unsupported_output" => StatusCode::UNPROCESSABLE_ENTITY,
            "viewer_capacity" | "input_capacity" | "output_capacity" => {
                StatusCode::TOO_MANY_REQUESTS
            }
            "startup_timeout" => StatusCode::GATEWAY_TIMEOUT,
            "source_preparation_failed" | "processing_failed" | "media_unavailable" => {
                StatusCode::BAD_GATEWAY
            }
            "request_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
            _ => StatusCode::SERVICE_UNAVAILABLE,
        };
        (status, Json(serde_json::json!({"error": {"code": self.0}}))).into_response()
    }
}

async fn issue(
    State(state): State<KeyApi>,
    headers: HeaderMap,
    body: Result<Json<KeySpec>, JsonRejection>,
) -> Result<Response, ApiError> {
    let bootstrap = bearer(&headers)?;
    let Json(spec) = body.map_err(|rejection| {
        ApiError(if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            "request_too_large"
        } else {
            "invalid_request"
        })
    })?;
    let timestamp = now()?;
    // SQLite operations must not block the asynchronous HTTP executor.
    let issued =
        tokio::task::spawn_blocking(move || state.credentials.issue(&bootstrap, spec, timestamp))
            .await
            .map_err(|_| ApiError("credential_store_unavailable"))?
            .map_err(ApiError)?;
    Ok((StatusCode::CREATED, Json(issued)).into_response())
}

async fn list(State(state): State<KeyApi>, headers: HeaderMap) -> Result<Response, ApiError> {
    let bootstrap = bearer(&headers)?;
    let records = tokio::task::spawn_blocking(move || state.credentials.list(&bootstrap))
        .await
        .map_err(|_| ApiError("credential_store_unavailable"))?
        .map_err(ApiError)?;
    Ok(Json(serde_json::json!({"items": records})).into_response())
}

async fn revoke(
    State(state): State<KeyApi>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let bootstrap = bearer(&headers)?;
    let timestamp = now()?;
    tokio::task::spawn_blocking(move || {
        state.credentials.revoke(&bootstrap, &id)?;
        state
            .sessions
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .revoke(&id, timestamp as u64);
        Ok::<_, &'static str>(())
    })
    .await
    .map_err(|_| ApiError("credential_store_unavailable"))?
    .map_err(ApiError)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use serde_json::{json, Value};
    use tower::ServiceExt;

    const BOOTSTRAP: &str = "bootstrap-secret-at-least-32-characters";
    fn fixture() -> (Router, Arc<Credentials>) {
        let credentials = Arc::new(
            Credentials::from_connection(
                rusqlite::Connection::open_in_memory().unwrap(),
                BOOTSTRAP,
            )
            .unwrap(),
        );
        (
            router(KeyApi {
                credentials: credentials.clone(),
                sessions: Arc::new(Mutex::new(Sessions::default())),
            }),
            credentials,
        )
    }
    fn spec() -> Value {
        json!({"label":"fixture", "namespaces":["test"], "scopes":["create","read","renew","release"],
            "quotas":{"inputs":1,"outputs":1,"viewers":5}, "expires_at":null})
    }
    async fn call(
        app: Router,
        method: &str,
        uri: &str,
        token: &str,
        body: String,
    ) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        (
            status,
            if bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&bytes).unwrap()
            },
        )
    }

    #[tokio::test]
    async fn issue_lists_without_secrets_and_revokes_idempotently() {
        let (app, credentials) = fixture();
        let (status, issued) = call(
            app.clone(),
            "POST",
            "/v1/keys",
            BOOTSTRAP,
            spec().to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let secret = issued["secret"].as_str().unwrap();
        let id = issued["key"]["id"].as_str().unwrap();
        assert!(credentials.authenticate(secret, now().unwrap()).is_ok());
        let (status, _) = call(app.clone(), "GET", "/v1/keys", secret, String::new()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (_, listed) = call(app.clone(), "GET", "/v1/keys", BOOTSTRAP, String::new()).await;
        assert_eq!(listed["items"][0]["id"], id);
        assert!(!listed.to_string().contains(secret));
        assert!(!listed.to_string().contains("secret"));
        for _ in 0..2 {
            assert_eq!(
                call(
                    app.clone(),
                    "DELETE",
                    &format!("/v1/keys/{id}"),
                    BOOTSTRAP,
                    String::new()
                )
                .await
                .0,
                StatusCode::NO_CONTENT
            );
        }
        assert!(credentials.authenticate(secret, now().unwrap()).is_err());
    }

    #[tokio::test]
    async fn rejects_before_parsing_and_does_not_echo_invalid_input() {
        let (app, _) = fixture();
        let secret_input = "private-source-password-malformed-json";
        let (status, error) = call(
            app.clone(),
            "POST",
            "/v1/keys",
            "wrong",
            secret_input.into(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(!error.to_string().contains(secret_input));
        let (status, error) = call(
            app.clone(),
            "POST",
            "/v1/keys",
            BOOTSTRAP,
            secret_input.into(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(!error.to_string().contains(secret_input));
        let (status, _) = call(app, "POST", "/v1/keys", BOOTSTRAP, "x".repeat(17 * 1024)).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }
}
