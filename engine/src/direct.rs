//! Original-media transport. Only server-discovered resources enter the map;
//! viewers supply opaque resource keys, never upstream URLs.
use super::*;
use axum::{
    body::{Body, Bytes},
    http::{header, HeaderMap, Method, StatusCode},
    response::Response,
};
use futures::StreamExt;
use playlist::{bounded, content_range, range_bounds, rewrite};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Semaphore;
use url::Url;

mod playlist;
#[cfg(test)]
mod real_tests;
#[cfg(test)]
mod tests;
mod transport;

const CHUNK: u64 = 1024 * 1024;
// A failed or stalled chunk read is retried before the body fails. Measured
// providers answer sustained range fetching with intermittent 502s and
// throttling pages, so the backoff doubles per attempt to wait the patch out
// instead of failing every in-flight body.
const CHUNK_ATTEMPTS: usize = 6;
const CHUNK_BACKOFF: Duration = Duration::from_millis(400);
const CHUNK_BACKOFF_MAX: Duration = Duration::from_millis(3200);
// Chunks fetched beyond the one being sent: upstream latency overlaps client
// consumption, and the bound caps buffered bytes per response.
const CHUNK_LOOKAHEAD: usize = 2;
const RESOURCE_LIMIT: usize = 4096;
const CACHE_LIMIT: usize = 8 * 1024 * 1024;
// Players poll live playlists near every second; a matching TTL absorbs the
// cadence without freezing the live window.
const PLAYLIST_TTL: Duration = Duration::from_secs(1);
const PLAYLIST_CACHE_LIMIT: usize = 4 * 1024 * 1024;

pub(super) struct Direct {
    client: reqwest::Client,
    headers: HeaderMap,
    root: Url,
    resources: Mutex<HashMap<String, Url>>,
    // Six upstream reservations so playlist polls, segment bodies, and file
    // chunk lookahead overlap instead of serializing; a stalled viewer pins
    // at most its chunk lookahead, never a permit.
    fetch: Arc<Semaphore>,
    proxy: Option<String>,
    destinations: Mutex<HashMap<String, reqwest::Client>>,
    cache: Mutex<HashMap<String, (Instant, Bytes)>>,
    playlists: Mutex<HashMap<String, (Instant, Bytes)>>,
    pub closed: AtomicBool,
    pub failed: AtomicBool,
    pub waiting_consumers: std::sync::atomic::AtomicUsize,
    progress: Mutex<Option<String>>,
    size: Option<u64>,
    pub(super) format: String,
    validator: Option<String>,
    permits: Arc<InputPermits>,
}

impl Direct {
    pub(super) fn inspection_source(&self) -> (String, String, Arc<InputPermits>) {
        let mut headers = self
            .headers
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|v| (name.as_str().to_owned(), v.to_owned()))
            })
            .collect::<HashMap<_, _>>();
        if let Some(proxy) = &self.proxy {
            headers.insert(crate::EGRESS_PROXY_HEADER.into(), proxy.clone());
        }
        (
            self.root.as_str().to_owned(),
            header_block(&headers).unwrap_or_default(),
            self.permits.clone(),
        )
    }
    pub async fn prepare(
        url: Url,
        headers: &HashMap<String, String>,
        format: &str,
        permits: Arc<InputPermits>,
    ) -> Result<Arc<Self>, String> {
        let mut public = HeaderMap::new();
        for (name, value) in headers {
            if name.eq_ignore_ascii_case(crate::EGRESS_PROXY_HEADER) {
                continue;
            }
            if [
                "host",
                "connection",
                "content-length",
                "transfer-encoding",
                "range",
                "accept-encoding",
            ]
            .contains(&name.to_ascii_lowercase().as_str())
            {
                continue;
            }
            public.insert(
                header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| "Invalid media header")?,
                value.parse().map_err(|_| "Invalid media header")?,
            );
        }
        let client = crate::egress_proxy_builder(
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .read_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none()),
            headers.get(crate::EGRESS_PROXY_HEADER).map(String::as_str),
        )?
        .build()
        .map_err(|_| "Media transport unavailable")?;
        let mut direct = Self {
            client,
            headers: public,
            root: url,
            resources: Mutex::new(HashMap::new()),
            fetch: Arc::new(Semaphore::new(6)),
            proxy: headers.get(crate::EGRESS_PROXY_HEADER).cloned(),
            destinations: Mutex::new(HashMap::new()),
            cache: Mutex::new(HashMap::new()),
            playlists: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            waiting_consumers: std::sync::atomic::AtomicUsize::new(0),
            progress: Mutex::new(None),
            size: None,
            format: format.to_owned(),
            validator: None,
            permits,
        };
        // Original files of any container need the same byte-range discovery as
        // MP4; only HLS uses playlist rewriting and per-segment routing.
        if format == "auto" {
            let response = direct
                .request(direct.root.clone(), Some("bytes=0-1023"))
                .await?;
            let range = content_range(response.headers());
            let mime = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            direct.validator = response
                .headers()
                .get(header::ETAG)
                .and_then(|v| v.to_str().ok())
                .filter(|s| !s.starts_with("W/"))
                .map(str::to_owned);
            let status = response.status();
            let mut body = response.bytes_stream();
            let mut prefix = Vec::new();
            while prefix.len() < 16 {
                match body.next().await {
                    Some(Ok(bytes)) => {
                        prefix.extend_from_slice(&bytes[..bytes.len().min(1024 - prefix.len())])
                    }
                    Some(Err(_)) => return Err("Media origin unavailable".into()),
                    None => break,
                }
            }
            let hls = prefix.starts_with(b"#EXTM3U")
                || mime.contains("mpegurl")
                || mime.contains("mpegURL");
            if mime.contains("text/html") {
                return Err("Media origin unavailable".into());
            }
            direct.format = if hls { "hls" } else { "bin" }.into();
            if !hls && status == StatusCode::PARTIAL_CONTENT {
                let (start, _, size) = range.ok_or("Invalid source range")?;
                if start != 0 || size == 0 {
                    return Err("Invalid source range".into());
                }
                direct.size = Some(size);
            }
        } else if format != "hls" {
            let response = direct
                .request(direct.root.clone(), Some("bytes=0-0"))
                .await?;
            let (start, end, total) =
                content_range(response.headers()).ok_or("Source does not support byte ranges")?;
            if response.status() != StatusCode::PARTIAL_CONTENT
                || start != 0
                || end != 0
                || total == 0
            {
                return Err("Source does not support byte ranges".into());
            }
            direct.size = Some(total);
            direct.validator = response
                .headers()
                .get(header::ETAG)
                .and_then(|v| v.to_str().ok())
                .filter(|s| !s.starts_with("W/"))
                .map(str::to_owned);
            let bytes = bounded(response, 1).await?;
            if bytes.len() != 1 {
                return Err("Invalid source range".into());
            }
        }
        let direct = Arc::new(direct);
        if direct.format == "hls" {
            direct.playlist(direct.root.clone()).await?;
        }
        Ok(direct)
    }
}

impl Direct {
    pub async fn serve(
        self: Arc<Self>,
        file: &str,
        method: Method,
        headers: HeaderMap,
    ) -> Result<Response, String> {
        if self.closed.load(Ordering::Acquire) {
            return Err("Media expired".into());
        }
        if let Some(size) = self.size {
            if !file.starts_with("source.") {
                return Err("Media not found".into());
            }
            let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
            let range = if headers
                .get(header::IF_RANGE)
                .is_some_and(|v| Some(v.to_str().unwrap_or("")) != self.validator.as_deref())
            {
                None
            } else {
                range
            };
            let Some((start, end)) = range_bounds(range, size) else {
                return Ok(Response::builder()
                    .status(416)
                    .header(header::CONTENT_RANGE, format!("bytes */{size}"))
                    .body(Body::empty())
                    .unwrap());
            };
            // The validated original file is immutable; ranges may cache.
            let mut builder = Response::builder()
                .status(if range.is_some() { 206 } else { 200 })
                .header(
                    header::CONTENT_TYPE,
                    media_type(file).unwrap_or("application/octet-stream"),
                )
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CACHE_CONTROL, "public, max-age=3600");
            if range.is_some() {
                builder =
                    builder.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
            }
            if let Some(value) = &self.validator {
                builder = builder.header(header::ETAG, value);
            }
            if method == Method::HEAD {
                // HEAD carries no body, so the exact length stays safe to
                // promise.
                return builder
                    .header(header::CONTENT_LENGTH, (end - start + 1).to_string())
                    .body(Body::empty())
                    .map_err(|_| "Invalid media response".into());
            }
            // GET streams without CONTENT_LENGTH on purpose: chunked transfer
            // means a mid-stream termination ends the response cleanly
            // instead of promising bytes the framing can never deliver. While
            // the client consumes chunk N, chunks N+1..N+LOOKAHEAD are already
            // in flight upstream, so one response sustains well above one
            // chunk per round trip. Permits are held only during upstream I/O,
            // never while waiting on the client.
            let (send, mut receive) =
                tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(CHUNK_LOOKAHEAD);
            let streaming = self.clone();
            tokio::spawn(async move {
                let _permits = streaming.permits.clone();
                let mut offset = start;
                let mut in_flight: VecDeque<tokio::task::JoinHandle<Result<Bytes, String>>> =
                    VecDeque::new();
                while offset <= end || !in_flight.is_empty() {
                    // Keep the pipeline ahead of the client; teardown or the
                    // end of the range stops new fetches.
                    while offset <= end
                        && in_flight.len() < CHUNK_LOOKAHEAD
                        && !streaming.closed.load(Ordering::Acquire)
                    {
                        let last = end.min(offset.saturating_add(CHUNK - 1));
                        let direct = streaming.clone();
                        in_flight.push_back(tokio::spawn(async move {
                            direct.retrying_range(offset, last).await
                        }));
                        offset = last + 1;
                    }
                    if streaming.closed.load(Ordering::Acquire) {
                        // The session was replaced or stopped: no new fetches,
                        // pending ones abort, completed chunks still stream
                        // out, and the body ends cleanly so the client retries
                        // against the new session instead of a protocol error.
                        for task in in_flight.drain(..) {
                            task.abort();
                            if let Ok(Ok(bytes)) = task.await {
                                let _ = timeout(Duration::from_secs(1), send.send(Ok(bytes))).await;
                            }
                        }
                        break;
                    }
                    let Some(task) = in_flight.pop_front() else {
                        break;
                    };
                    match task.await {
                        Ok(Ok(bytes)) => {
                            // Backpressure without a permit: a paused viewer
                            // parks the producer holding nothing upstream.
                            if send.send(Ok(bytes)).await.is_err() {
                                break;
                            }
                        }
                        // Retries were exhausted inside retrying_range, or a
                        // fetch task aborted; a teardown racing the failure
                        // ends the body cleanly instead.
                        _ => {
                            if !streaming.closed.load(Ordering::Acquire) {
                                let _ = send
                                    .send(Err(std::io::Error::other("Media range failed")))
                                    .await;
                            }
                            break;
                        }
                    }
                }
                for task in in_flight.drain(..) {
                    task.abort();
                }
            });
            let stream = async_stream::stream! {
                while let Some(chunk) = receive.recv().await {
                    yield chunk;
                }
            };
            return builder
                .body(Body::from_stream(stream))
                .map_err(|_| "Invalid media response".into());
        }
        if self.format != "hls" {
            if file != "source.bin" {
                return Err("Media not found".into());
            }
            let response = self.request(self.root.clone(), None).await?;
            let mut builder = Response::builder()
                .status(200)
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .header(header::CACHE_CONTROL, "no-store");
            if let Some(length) = response.content_length() {
                builder = builder.header(header::CONTENT_LENGTH, length);
            }
            if method == Method::HEAD {
                return builder
                    .body(Body::empty())
                    .map_err(|_| "Invalid media response".into());
            }
            let stream = async_stream::try_stream! {
                let _permits = self.permits.clone();
                let mut chunks = response.bytes_stream();
                let started = Instant::now();
                let mut received = 0usize;
                while let Some(chunk) = chunks.next().await {
                    if self.closed.load(Ordering::Acquire) { Err(std::io::Error::other("Media access revoked"))?; }
                    let chunk = chunk.map_err(|_| {
                        self.failed.store(true, Ordering::Release);
                        std::io::Error::other("Media delivery failed")
                    })?;
                    received = received.saturating_add(chunk.len());
                    *self.progress.lock().await = Some(format!("{started:?}:{received}"));
                    self.waiting_consumers.fetch_add(1, Ordering::AcqRel);
                    struct Waiting(Arc<Direct>);
                    impl Drop for Waiting {
                        fn drop(&mut self) { self.0.waiting_consumers.fetch_sub(1, Ordering::AcqRel); }
                    }
                    let waiting = Waiting(self.clone());
                    yield chunk;
                    drop(waiting);
                }
                self.failed.store(true, Ordering::Release);
            };
            return builder
                .body(Body::from_stream(futures::StreamExt::map(
                    stream,
                    |item: Result<Bytes, std::io::Error>| item,
                )))
                .map_err(|_| "Invalid media response".into());
        }
        let url = if file == "index.m3u8" {
            self.root.clone()
        } else {
            self.resources
                .lock()
                .await
                .get(file)
                .cloned()
                .ok_or("Media not found")?
        };
        if file.ends_with(".m3u8") {
            let bytes = self
                .playlist(url)
                .await
                .inspect_err(|_| self.failed.store(true, Ordering::Release))?;
            // An ended playlist is immutable; live playlists must revalidate.
            let policy = if std::str::from_utf8(&bytes).is_ok_and(|t| t.contains("#EXT-X-ENDLIST"))
            {
                "public, max-age=3600"
            } else {
                "no-store"
            };
            return Ok(Response::builder()
                .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
                .header(header::CACHE_CONTROL, policy)
                .body(if method == Method::HEAD {
                    Body::empty()
                } else {
                    Body::from(bytes)
                })
                .unwrap());
        }
        // Cached segments are shared between viewers without sharing their
        // capability URLs, and answer before any upstream admission.
        let range = headers
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.len() < 128 && v.starts_with("bytes=") && !v.contains(','));
        let key = format!("{}:{}", url, range.unwrap_or(""));
        // Ranges carry upstream status and lengths; do not use a status-less cache.
        if range.is_none() {
            if let Some((at, bytes)) = self.cache.lock().await.get(&key) {
                if at.elapsed() < Duration::from_secs(15) {
                    return Ok(Response::builder()
                        .header(header::CONTENT_TYPE, "application/octet-stream")
                        .header(header::CACHE_CONTROL, "public, max-age=3600")
                        .body(if method == Method::HEAD {
                            Body::empty()
                        } else {
                            Body::from(bytes.clone())
                        })
                        .unwrap());
                }
            }
        }
        let permit = self
            .fetch
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "Media expired".to_owned())?;
        let response = self
            .request(url, range)
            .await
            .inspect_err(|_| self.failed.store(true, Ordering::Release))?;
        if range.is_some() && response.status() != StatusCode::PARTIAL_CONTENT {
            return Err("Invalid HLS resource range".into());
        }
        // Segments, keys, and init sections are immutable upstream and the
        // opaque key never changes identity; clients may cache by it.
        let mut builder = Response::builder()
            .status(response.status())
            .header(header::CACHE_CONTROL, "public, max-age=3600");
        for name in [
            header::CONTENT_TYPE,
            header::CONTENT_RANGE,
            header::ACCEPT_RANGES,
        ] {
            if let Some(value) = response.headers().get(&name) {
                builder = builder.header(name, value);
            }
        }
        if let Some(value) = response.headers().get(header::CONTENT_LENGTH) {
            builder = builder.header(header::CONTENT_LENGTH, value);
        }
        if method == Method::HEAD {
            return builder
                .body(Body::empty())
                .map_err(|_| "Invalid media response".into());
        }
        let cacheable = range.is_none();
        // Sixteen chunks of slack absorb player jitter without pinning a permit.
        let (send, mut receive) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
        tokio::spawn(async move {
            let _permit = permit;
            let _permits = self.permits.clone();
            let result = async {
                let mut chunks = response.bytes_stream();
                let mut saved = Vec::new();
                let mut cacheable = cacheable;
                while let Some(chunk) = chunks.next().await {
                    if self.closed.load(Ordering::Acquire) {
                        return Err(std::io::Error::other("Media expired"));
                    }
                    let chunk = chunk.map_err(|_| {
                        self.failed.store(true, Ordering::Release);
                        std::io::Error::other("Media body failed")
                    })?;
                    if cacheable && saved.len() + chunk.len() <= CACHE_LIMIT {
                        saved.extend_from_slice(&chunk);
                    } else if cacheable {
                        cacheable = false;
                        saved.clear();
                        saved.shrink_to_fit();
                    }
                    // A stalled viewer must not keep the shared input locked.
                    timeout(Duration::from_secs(5), send.send(Ok(chunk)))
                        .await
                        .map_err(|_| std::io::Error::other("Media consumer stalled"))?
                        .map_err(|_| std::io::Error::other("Media consumer closed"))?;
                }
                if cacheable {
                    self.cache_insert(key, Bytes::from(saved)).await;
                }
                Ok::<(), std::io::Error>(())
            }
            .await;
            if let Err(error) = result {
                let _ = timeout(Duration::from_secs(1), send.send(Err(error))).await;
            }
        });
        let stream =
            async_stream::stream! {while let Some(chunk)=receive.recv().await {yield chunk;}};
        builder
            .body(Body::from_stream(stream))
            .map_err(|_| "Invalid media response".into())
    }

    pub async fn progress(&self) -> Option<String> {
        self.progress.lock().await.clone()
    }
}
