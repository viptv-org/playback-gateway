//! Loopback sanitizer for disguised HLS read by FFmpeg.
//!
//! IPTV origins disguise HLS segments as images or stylesheets: odd or
//! missing suffixes, a lying `Content-Type`, and a real image header or CSS
//! text before the media. FFmpeg typefinds such a segment as an image. The
//! original-media transport (`direct::Direct`) already strips the prefix and
//! enforces the source rules (private-destination refusal, the consumed
//! egress-proxy route, cross-origin credential stripping, bounded caches and
//! redirects, opaque resource names). This module serves that transport on a
//! loopback port behind a capability token so every FFmpeg reader of a
//! disguised source reads cleaned bytes without receiving credentials, a
//! proxy route or an upstream URL.
use super::*;
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use std::sync::{atomic::Ordering, Weak};

/// Concurrent loopback requests per sanitizer. One FFmpeg child issues a
/// playlist reload and one or two segment reads at a time.
const REQUEST_LIMIT: usize = 16;
/// Cleaned-coordinate prefix records kept per sanitizer (one per segment).
const PREFIX_LIMIT: usize = 4096;

pub(super) struct HlsSanitizer {
    direct: Arc<direct::Direct>,
    token: String,
    port: u16,
    listener: tokio::task::JoinHandle<()>,
}

struct Shared {
    // Weak: a lingering loopback connection never keeps the transport, its
    // upstream clients or the caller's reservations alive past the job.
    direct: Weak<direct::Direct>,
    token: String,
    requests: Semaphore,
    prefixes: Mutex<HashMap<String, u64>>,
}

impl std::fmt::Debug for HlsSanitizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HlsSanitizer(<redacted>)")
    }
}

impl HlsSanitizer {
    /// Prepare the HLS transport for `url` (which fetches and validates the
    /// root playlist) and bind a loopback listener for it.
    pub(super) async fn start(
        url: &str,
        headers: &HashMap<String, String>,
        permits: Option<Arc<InputPermits>>,
    ) -> Result<Arc<Self>, String> {
        let url = validate_url(url)?;
        let direct = direct::Direct::prepare_with(url, headers, "hls", permits).await?;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| "Media sanitizer unavailable")?;
        let port = listener
            .local_addr()
            .map_err(|_| "Media sanitizer unavailable")?
            .port();
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let shared = Arc::new(Shared {
            direct: Arc::downgrade(&direct),
            token: token.clone(),
            requests: Semaphore::new(REQUEST_LIMIT),
            prefixes: Mutex::new(HashMap::new()),
        });
        let router = Router::new()
            .route("/:token/:file", get(handle).head(handle))
            .fallback(|| async { StatusCode::NOT_FOUND })
            .with_state(shared);
        let listener = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok(Arc::new(Self {
            direct,
            token,
            port,
            listener,
        }))
    }

    /// The capability URL FFmpeg reads. Never log it.
    pub(super) fn url(&self) -> String {
        format!("http://127.0.0.1:{}/{}/index.m3u8", self.port, self.token)
    }

    /// Remove the capability from text that may reach diagnostics.
    pub(super) fn redact(&self, text: &str) -> String {
        text.replace(&self.url(), "[link omitted]")
            .replace(&self.token, "[private value omitted]")
    }
}

impl Drop for HlsSanitizer {
    fn drop(&mut self) {
        // In-flight bodies end at their next chunk; no new request is served.
        self.direct.closed.store(true, Ordering::Release);
        self.listener.abort();
    }
}

async fn handle(
    State(shared): State<Arc<Shared>>,
    Path((token, file)): Path<(String, String)>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    if !constant_time_eq(token.as_bytes(), shared.token.as_bytes()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(direct) = shared.direct.upgrade() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if direct.closed.load(Ordering::Acquire) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(_request) = shared.requests.acquire().await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    // Only range negotiation reaches the transport; FFmpeg's own request
    // headers (user agent, cookies it may have stored) never go upstream.
    let mut forwarded = HeaderMap::new();
    let prefix = shared.prefixes.lock().await.get(&file).copied();
    let mut shifted = None;
    // FFmpeg opens every resource with `Range: bytes=0-`. That is the whole
    // resource, so it is read un-ranged: the transport then strips the
    // disguise and caches the body, and origins without range support work.
    if let Some(range) = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .filter(|range| range.trim() != "bytes=0-")
    {
        match prefix.and_then(|prefix| shift_range(range, prefix).map(|r| (r, prefix))) {
            Some((range, prefix)) => {
                forwarded.insert(
                    header::RANGE,
                    HeaderValue::from_str(&range).expect("ASCII range"),
                );
                shifted = Some(prefix);
            }
            None => {
                if let Ok(value) = HeaderValue::from_str(range) {
                    forwarded.insert(header::RANGE, value);
                }
            }
        }
    }
    match direct.serve(&file, method, forwarded).await {
        Ok(mut response) => {
            if let Some(direct::StrippedPrefix(prefix)) = response.extensions().get().copied() {
                let mut prefixes = shared.prefixes.lock().await;
                if prefixes.len() >= PREFIX_LIMIT {
                    prefixes.clear();
                }
                prefixes.insert(file, prefix);
            }
            if let Some(prefix) = shifted {
                unshift_content_range(response.headers_mut(), prefix);
            }
            response
        }
        Err(_) => Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::empty())
            .expect("static response"),
    }
}

/// A whole segment was answered without its disguise prefix, so an FFmpeg
/// reconnect asks for an offset in cleaned coordinates. The transport takes
/// origin coordinates; shift by the removed prefix.
fn shift_range(range: &str, prefix: u64) -> Option<String> {
    let spec = range.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let start = start.trim().parse::<u64>().ok()?.checked_add(prefix)?;
    let end = end.trim();
    if end.is_empty() {
        return Some(format!("bytes={start}-"));
    }
    let end = end.parse::<u64>().ok()?.checked_add(prefix)?;
    Some(format!("bytes={start}-{end}"))
}

fn unshift_content_range(headers: &mut HeaderMap, prefix: u64) {
    let Some(value) = headers
        .get(header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
    else {
        return;
    };
    let parsed = value.strip_prefix("bytes ").and_then(|rest| {
        let (span, total) = rest.split_once('/')?;
        let (start, end) = span.split_once('-')?;
        let start = start.parse::<u64>().ok()?.checked_sub(prefix)?;
        let end = end.parse::<u64>().ok()?.checked_sub(prefix)?;
        let total = match total {
            "*" => "*".to_owned(),
            total => total.parse::<u64>().ok()?.checked_sub(prefix)?.to_string(),
        };
        Some(format!("bytes {start}-{end}/{total}"))
    });
    match parsed.and_then(|v| HeaderValue::from_str(&v).ok()) {
        Some(value) => {
            headers.insert(header::CONTENT_RANGE, value);
        }
        None => {
            headers.remove(header::CONTENT_RANGE);
        }
    }
}

/// Start the sanitizer a reader needs: one per FFmpeg job whose probe found
/// the source disguised, owned by that job and dropped with it.
pub(super) async fn for_reader(
    probe: &Probe,
    url: &str,
    header_block: &str,
    permits: Option<Arc<InputPermits>>,
) -> Result<Option<Arc<HlsSanitizer>>, String> {
    if !probe.disguised_hls {
        return Ok(None);
    }
    HlsSanitizer::start(url, &header_map(header_block), permits)
        .await
        .map(Some)
        .map_err(|_| "Could not prepare the disguised source safely".to_owned())
}

/// Parse a validated header block back into the map the transport takes.
pub(super) fn header_map(block: &str) -> HashMap<String, String> {
    block
        .split("\r\n")
        .filter_map(|line| line.split_once(": "))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_ranges_map_between_cleaned_and_origin_coordinates() {
        assert_eq!(shift_range("bytes=100-", 32).as_deref(), Some("bytes=132-"));
        assert_eq!(
            shift_range("bytes=0-99", 32).as_deref(),
            Some("bytes=32-131")
        );
        assert_eq!(shift_range("bytes=0-1,5-9", 32), None);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_static("bytes 132-999/1000"),
        );
        unshift_content_range(&mut headers, 32);
        assert_eq!(headers[header::CONTENT_RANGE], "bytes 100-967/968");
    }

    #[test]
    fn header_blocks_round_trip_to_transport_headers() {
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_owned(), "Bearer x".to_owned());
        headers.insert(EGRESS_PROXY_HEADER.to_owned(), "http://proxy:1".to_owned());
        assert_eq!(header_map(&header_block(&headers).unwrap()), headers);
    }
}
