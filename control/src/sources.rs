//! Converts torrent/archive and header-bearing HTTP inputs to owned byte-range sources.
//! HTTP metainfo and archive ranges always traverse the vetted egress proxy.
use crate::protocol::Input;
use playback_engine::{
    archive_cache::{ArchiveCache, ArchiveCacheReservation},
    rar::{RarMedia, RarVolume},
    LoopbackStream, TorrentConfig, TorrentFile, TorrentRangeSource,
};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Client, Method, StatusCode,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::{Read, Seek, SeekFrom},
    sync::{
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        Arc, Weak,
    },
    time::Duration,
};
use tokio::sync::{Mutex, Notify};

pub struct Sources {
    enabled: bool,
    runtime: Arc<crate::runtime_sources::RuntimeSources>,
    archive_cache: ArchiveCache,
    client: Client,
    resolved: Mutex<HashMap<[u8; 32], CachedSource>>,
    acquiring: Mutex<HashMap<[u8; 32], Weak<Acquisition>>>,
}
struct Acquisition {
    lock: Mutex<()>,
    stage: Arc<AtomicU8>,
}
struct CachedSource {
    source: Weak<ResolvedSource>,
    reads: Arc<ArchiveReads>,
}
pub struct ResolvedSource {
    pub url: String,
    pub file: TorrentFile,
    // HTTP relay sizing is transport metadata, never an invented source file.
    pub file_metadata: bool,
    pub client_frame_ack: bool,
    _archive: Option<LoopbackStream>,
    reads: Arc<ArchiveReads>,
    runtime: Option<Arc<crate::runtime_sources::RuntimeSource>>,
}
impl ResolvedSource {
    #[cfg(test)]
    pub(crate) async fn fixture_stats(&self) -> serde_json::Value {
        self.runtime.as_ref().unwrap().fixture_stats().await
    }
}
impl Sources {
    pub fn new(config: TorrentConfig, proxy: &str) -> Result<Self, &'static str> {
        let enabled = config.enabled;
        let runtime = Arc::new(crate::runtime_sources::RuntimeSources::new(
            std::env::var_os("TORRENT_WORKER")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from("torrent-worker")),
            config.cache_dir.join("shared-runtime-v2"),
            config.max_bytes,
        ));
        let archive_cache = ArchiveCache::new(config.cache_dir, enabled, config.max_bytes)
            .map_err(|_| "invalid_torrent_configuration")?;
        let client = Client::builder()
            .no_proxy()
            .proxy(reqwest::Proxy::all(proxy).map_err(|_| "egress_unavailable")?)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|_| "egress_unavailable")?;
        Ok(Self {
            enabled,
            runtime,
            archive_cache,
            client,
            resolved: Mutex::new(HashMap::new()),
            acquiring: Mutex::new(HashMap::new()),
        })
    }
    pub async fn initialize(&self) -> Result<(), &'static str> {
        if self.enabled {
            self.archive_cache
                .initialize()
                .await
                .map_err(|_| "torrent_cache_unavailable")?;
            self.runtime.initialize().await?;
        }
        Ok(())
    }
    pub async fn shutdown(&self) -> Result<(), &'static str> {
        self.runtime.shutdown().await
    }
    pub async fn output_ready(&self, source: &Arc<ResolvedSource>) -> Result<(), &'static str> {
        self.report_first_frame(source).await
    }
    pub async fn report_first_frame(
        &self,
        source: &Arc<ResolvedSource>,
    ) -> Result<(), &'static str> {
        if let Some(runtime) = &source.runtime {
            runtime.report_first_frame().await?;
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn owned_runtime_budget(&mut self, cache: std::path::PathBuf, budget: u64) {
        self.runtime = Arc::new(crate::runtime_sources::RuntimeSources::new(
            std::env::var_os("TORRENT_WORKER")
                .expect("owned Go fixture worker")
                .into(),
            cache,
            budget,
        ));
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub async fn reap(&self) -> Result<(), &'static str> {
        let mut resolved = self.resolved.lock().await;
        // Pending preparation cancellation can drop the public source while a
        // blocking probe reader still owns its archive. Wait only orphans;
        // unrelated live output jobs keep serving through the same resolver.
        for cached in resolved
            .values()
            .filter(|cached| cached.source.strong_count() == 0)
        {
            cached.reads.close();
            cached.reads.wait().await;
        }
        resolved.retain(|_, cached| cached.source.strong_count() > 0);
        drop(resolved);
        self.runtime.reap().await?;
        self.archive_cache
            .reap()
            .await
            .map_err(|_| "torrent_cache_unavailable")
    }
    /// A reaper holds one map reference and one local clone. Additional
    /// references belong to another output or pending preparation and retain
    /// their live listener. The resolution mutex prevents new clone admission
    /// while the final source is closing.
    pub async fn close_if_unused(&self, source: &Arc<ResolvedSource>) -> Result<(), &'static str> {
        let mut resolved = self.resolved.lock().await;
        if Arc::strong_count(source) > 2 {
            return Ok(());
        }
        source.reads.close();
        if let Some(runtime) = &source.runtime {
            runtime.close().await?;
        }
        if let Some(stream) = &source._archive {
            stream.stop();
        }
        source.reads.wait().await;
        resolved.retain(|_, cached| !cached.source.ptr_eq(&Arc::downgrade(source)));
        Ok(())
    }
    pub async fn stage(
        &self,
        namespace: &str,
        input: &Input,
        client_frame_ack: bool,
    ) -> Option<&'static str> {
        let identity = serde_json::to_vec(&(namespace, input, client_frame_ack, false)).ok()?;
        let key: [u8; 32] = Sha256::digest(identity).into();
        let observed = self
            .acquiring
            .lock()
            .await
            .get(&key)
            .and_then(Weak::upgrade)
            .map(|acquisition| acquisition.stage.load(Ordering::Acquire));
        match observed {
            Some(1) => Some("finding_peers"),
            Some(2) => Some("fetching_metadata"),
            Some(3) => Some("opening_archive"),
            Some(4) => Some("buffering"),
            _ => self
                .resolved
                .lock()
                .await
                .get(&key)
                .and_then(|cached| cached.source.upgrade())
                .map(|_| "buffering"),
        }
    }
    #[cfg(test)]
    pub async fn resolve(
        &self,
        namespace: &str,
        input: &Input,
        pending: Option<Arc<dyn Send + Sync>>,
    ) -> Result<Arc<ResolvedSource>, &'static str> {
        self.resolve_with_ack(namespace, input, pending, false)
            .await
    }
    pub async fn resolve_with_ack(
        &self,
        namespace: &str,
        input: &Input,
        pending: Option<Arc<dyn Send + Sync>>,
        client_frame_ack: bool,
    ) -> Result<Arc<ResolvedSource>, &'static str> {
        self.resolve_with_http(namespace, input, pending, client_frame_ack, false)
            .await
    }
    pub async fn resolve_with_http(
        &self,
        namespace: &str,
        input: &Input,
        pending: Option<Arc<dyn Send + Sync>>,
        client_frame_ack: bool,
        force_http: bool,
    ) -> Result<Arc<ResolvedSource>, &'static str> {
        if !self.enabled && !input.mediated_http() && !force_http {
            return Err("unsupported_output");
        }
        if (input.mediated_http() || force_http) && input.live {
            return Err("unsupported_output");
        }
        let identity = serde_json::to_vec(&(namespace, input, client_frame_ack, force_http))
            .map_err(|_| "invalid_input")?;
        let key: [u8; 32] = Sha256::digest(identity).into();
        if input.torrent() {
            let lock = {
                let mut acquiring = self.acquiring.lock().await;
                acquiring.retain(|_, lock| lock.strong_count() > 0);
                if let Some(lock) = acquiring.get(&key).and_then(Weak::upgrade) {
                    lock
                } else {
                    if acquiring.len() >= 1024 {
                        return Err("input_capacity");
                    }
                    let lock = Arc::new(Acquisition {
                        lock: Mutex::new(()),
                        stage: Arc::new(AtomicU8::new(0)),
                    });
                    acquiring.insert(key, Arc::downgrade(&lock));
                    lock
                }
            };
            let _acquiring = lock.lock.lock().await;
            // Only this identity is serialized. Other torrents can discover concurrently.
            if let Some(cached) = self
                .resolved
                .lock()
                .await
                .get(&key)
                .and_then(|cached| cached.source.upgrade())
            {
                if cached.reads.closed.load(Ordering::Acquire) {
                    return Err("input_cleanup_pending");
                }
                return Ok(cached);
            }
            {
                let mut resolved = self.resolved.lock().await;
                resolved.retain(|_, cached| {
                    cached.source.strong_count() > 0
                        || cached.reads.active.load(Ordering::Acquire) > 0
                });
                if resolved.len() >= 1024 {
                    return Err("input_capacity");
                }
            }
            let metainfo = if input.url.starts_with("magnet:") {
                None
            } else {
                lock.stage.store(2, Ordering::Release);
                let response = fetch(
                    &self.client,
                    &input.url,
                    Method::GET,
                    &input_headers(input)?,
                )
                .await?;
                if !response.status().is_success() {
                    return Err("source_preparation_failed");
                }
                Some(bounded_body(response, 4 * 1024 * 1024).await?)
            };
            let runtime = self
                .runtime
                .resolve(input, metainfo, pending, lock.stage.clone())
                .await?;
            let source = Arc::new(ResolvedSource {
                client_frame_ack,
                url: runtime.url.clone(),
                file_metadata: true,
                file: TorrentFile {
                    index: runtime.file_index,
                    name: "media".into(),
                    size: runtime.size,
                },
                _archive: None,
                reads: Arc::new(ArchiveReads::default()),
                runtime: Some(runtime),
            });
            self.resolved.lock().await.insert(
                key,
                CachedSource {
                    source: Arc::downgrade(&source),
                    reads: source.reads.clone(),
                },
            );
            return Ok(source);
        }
        // One preparation per source identity. Concurrent viewers receive the
        // same capability URL, so existing normalized output sharing still works.
        let mut resolved = self.resolved.lock().await;
        if let Some(cached) = resolved.get(&key) {
            if let Some(source) = cached.source.upgrade() {
                if source.reads.closed.load(Ordering::Acquire) {
                    return Err("input_cleanup_pending");
                }
                return Ok(source);
            }
            if cached.reads.active.load(Ordering::Acquire) > 0 {
                return Err("input_cleanup_pending");
            }
        }
        if resolved.len() >= 1024 {
            resolved.retain(|_, cached| {
                cached.source.strong_count() > 0 || cached.reads.active.load(Ordering::Acquire) > 0
            });
        }
        if resolved.len() >= 1024 {
            return Err("input_capacity");
        }
        let headers = input_headers(input)?;
        let volume = HttpVolume::new(self.client.clone(), input.url.clone(), headers).await?;
        if input.mediated_http() || force_http {
            // Admit binary media only. Header-bearing manifests and nonranged
            // sources require their own nested-resource mediator and fail closed.
            let prefix = volume
                .read(0, 512)
                .await
                .map_err(|_| "source_preparation_failed")?;
            if !binary_media(&prefix) {
                return Err("unsupported_output");
            }
            let reads = Arc::new(ArchiveReads::default());
            let size = volume.size;
            let stream = LoopbackStream::from_source(Arc::new(HttpSource {
                volume,
                reads: reads.clone(),
            }))
            .await
            .map_err(torrent_error)?;
            let source = Arc::new(ResolvedSource {
                client_frame_ack,
                url: stream.url(0),
                file: TorrentFile {
                    index: 0,
                    name: String::new(),
                    size,
                },
                file_metadata: false,
                _archive: Some(stream),
                runtime: None,
                reads,
            });
            resolved.insert(
                key,
                CachedSource {
                    source: Arc::downgrade(&source),
                    reads: source.reads.clone(),
                },
            );
            return Ok(source);
        }
        let volumes: Vec<Arc<dyn RarVolume>> = vec![Arc::new(volume)];
        let inspected = tokio::task::spawn_blocking({
            let volumes = volumes.clone();
            let pending = pending.clone();
            move || {
                let _pending = pending;
                RarMedia::inspect(&volumes)
            }
        })
        .await
        .map_err(|_| "source_preparation_failed")??;
        if !inspected.stored {
            return Err("compressed_archive_unsupported");
        }
        let reservation = self
            .archive_cache
            .reserve_cache(0)
            .await
            .map_err(torrent_error)?;
        let (media, reservation) = extract_owned(volumes, reservation, pending).await?;
        let file = TorrentFile {
            index: 0,
            name: media.file().name.clone(),
            size: media.file().size,
        };
        let reads = Arc::new(ArchiveReads::default());
        let stream = LoopbackStream::from_source(Arc::new(ArchiveSource {
            media,
            _reservation: Arc::new(reservation),
            reads: reads.clone(),
        }))
        .await
        .map_err(torrent_error)?;
        let source = ResolvedSource {
            client_frame_ack,
            url: stream.url(0),
            file,
            file_metadata: true,
            _archive: Some(stream),
            runtime: None,
            reads,
        };
        let source = Arc::new(source);
        resolved.insert(
            key,
            CachedSource {
                source: Arc::downgrade(&source),
                reads: source.reads.clone(),
            },
        );
        Ok(source)
    }
}
async fn extract_owned(
    volumes: Vec<Arc<dyn RarVolume>>,
    reservation: ArchiveCacheReservation,
    pending: Option<Arc<dyn Send + Sync>>,
) -> Result<(RarMedia, ArchiveCacheReservation), &'static str> {
    let path = reservation.path().to_owned();
    let bytes = reservation.bytes();
    tokio::task::spawn_blocking(move || {
        let _pending = pending;
        // Blocking decoders outlive cancellation of their async waiter.
        // Retain both admission and cache until actual decoder completion.
        let media = RarMedia::open(volumes, &path, bytes)?;
        Ok((media, reservation))
    })
    .await
    .map_err(|_| "source_preparation_failed")?
}
fn torrent_error(error: String) -> &'static str {
    match error.as_str() {
        "Torrent exceeds cache budget" | "Torrent cache capacity reached" => {
            "torrent_cache_capacity"
        }
        "No supported media file in torrent"
        | "Invalid torrent file index"
        | "Private tracker torrents unsupported" => "unsupported_output",
        "Torrent metadata timed out" | "Torrent read timed out" => "startup_timeout",
        _ => "source_preparation_failed",
    }
}
fn input_headers(input: &Input) -> Result<HeaderMap, &'static str> {
    let mut headers = HeaderMap::new();
    for (key, value) in &input.headers {
        headers.insert(
            HeaderName::from_bytes(key.as_bytes()).map_err(|_| "invalid_headers")?,
            HeaderValue::from_str(value).map_err(|_| "invalid_headers")?,
        );
    }
    headers.insert(
        reqwest::header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    Ok(headers)
}
async fn fetch(
    client: &Client,
    source: &str,
    method: Method,
    headers: &HeaderMap,
) -> Result<reqwest::Response, &'static str> {
    let mut url = url::Url::parse(source).map_err(|_| "invalid_input")?;
    let mut headers = headers.clone();
    for _ in 0..=5 {
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err("invalid_input");
        }
        let response = client
            .request(method.clone(), url.clone())
            .headers(headers.clone())
            .send()
            .await
            .map_err(|_| "source_preparation_failed")?;
        if !response.status().is_redirection() {
            return Ok(response);
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or("source_preparation_failed")?;
        let next = url
            .join(location)
            .map_err(|_| "source_preparation_failed")?;
        if next.origin() != url.origin() {
            // Arbitrary provider headers can be credentials. Preserve only the
            // byte-range negotiation when a redirect changes the recipient.
            headers = headers
                .iter()
                .filter(|(name, _)| {
                    **name == reqwest::header::RANGE || **name == reqwest::header::ACCEPT_ENCODING
                })
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
        }
        url = next;
    }
    Err("source_preparation_failed")
}
async fn bounded_body(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, &'static str> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err("source_preparation_failed");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "source_preparation_failed")?
    {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err("source_preparation_failed");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
#[derive(Default)]
struct ArchiveReads {
    closed: AtomicBool,
    active: AtomicUsize,
    changed: Notify,
}
struct ArchiveReadGuard(Arc<ArchiveReads>);
impl ArchiveReads {
    fn begin(self: &Arc<Self>) -> Result<ArchiveReadGuard, String> {
        if self.closed.load(Ordering::Acquire) {
            return Err("Archive source closed".into());
        }
        self.active.fetch_add(1, Ordering::AcqRel);
        let guard = ArchiveReadGuard(self.clone());
        // Closing can race the increment. In that case no blocking IO starts;
        // the guard refunds the count before the future reports the refusal.
        if self.closed.load(Ordering::Acquire) {
            return Err("Archive source closed".into());
        }
        Ok(guard)
    }
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
    async fn wait(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }
}
impl Drop for ArchiveReadGuard {
    fn drop(&mut self) {
        if self.0.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.changed.notify_waiters();
        }
    }
}
struct ArchiveSource {
    media: RarMedia,
    _reservation: Arc<ArchiveCacheReservation>,
    reads: Arc<ArchiveReads>,
}
impl TorrentRangeSource for ArchiveSource {
    fn file_size(&self, index: u32) -> Option<u64> {
        (index == 0).then_some(self.media.file().size)
    }
    fn file_extension(&self, _: u32) -> String {
        self.media
            .file()
            .name
            .rsplit('.')
            .next()
            .unwrap_or("bin")
            .to_ascii_lowercase()
    }
    fn is_closed(&self) -> bool {
        self.reads.closed.load(Ordering::Acquire)
    }
    fn read_range(
        &self,
        _: u32,
        offset: u64,
        length: u32,
    ) -> futures::future::BoxFuture<'_, Result<Vec<u8>, String>> {
        let guard = self.reads.begin();
        let media = self.media.clone();
        let reservation = self._reservation.clone();
        Box::pin(async move {
            let guard = guard?;
            tokio::task::spawn_blocking(move || {
                // Drop the activity guard last, after every IO owner. Cancelling
                // the async response never cancels this blocking reader.
                let _guard = guard;
                let _reservation = reservation;
                let media = media;
                media.read_range(offset, length)
            })
            .await
            .map_err(|_| "Archive read failed".to_owned())?
            .map_err(str::to_owned)
        })
    }
}
// Conservative signatures keep manifest bytes out of FFprobe/FFmpeg. The
// capability suffix is private transport detail; source file metadata is absent.
fn binary_media(prefix: &[u8]) -> bool {
    prefix.get(4..8) == Some(b"ftyp")
        || prefix.starts_with(&[0x1a, 0x45, 0xdf, 0xa3])
        || prefix.starts_with(b"OggS")
        || prefix.starts_with(b"fLaC")
        || prefix.starts_with(b"ID3")
        || (prefix.starts_with(b"RIFF") && matches!(prefix.get(8..12), Some(b"AVI " | b"WAVE")))
        || (prefix.len() > 376 && prefix[0] == 0x47 && prefix[188] == 0x47 && prefix[376] == 0x47)
        || (prefix.len() > 1 && prefix[0] == 0xff && prefix[1] & 0xe0 == 0xe0)
}
struct HttpSource {
    volume: HttpVolume,
    reads: Arc<ArchiveReads>,
}
impl TorrentRangeSource for HttpSource {
    fn file_size(&self, index: u32) -> Option<u64> {
        (index == 0).then_some(self.volume.size)
    }
    fn is_closed(&self) -> bool {
        self.reads.closed.load(Ordering::Acquire)
    }
    fn read_range(
        &self,
        _: u32,
        offset: u64,
        length: u32,
    ) -> futures::future::BoxFuture<'_, Result<Vec<u8>, String>> {
        Box::pin(async move {
            let _guard = self.reads.begin()?;
            self.volume
                .read(offset, length)
                .await
                .map_err(|_| "Media range failed".to_owned())
        })
    }
}
#[derive(Clone)]
struct HttpVolume {
    client: Client,
    url: String,
    headers: HeaderMap,
    size: u64,
}
impl HttpVolume {
    async fn new(
        client: Client,
        url: String,
        mut headers: HeaderMap,
    ) -> Result<Self, &'static str> {
        headers.insert(
            reqwest::header::RANGE,
            HeaderValue::from_static("bytes=0-0"),
        );
        let response = fetch(&client, &url, Method::GET, &headers).await?;
        let (start, end, size) = content_range(&response)?;
        if start != 0 || end != 0 || bounded_body(response, 1).await?.len() != 1 {
            return Err("source_preparation_failed");
        }
        Ok(Self {
            client,
            url,
            headers,
            size,
        })
    }
    async fn read(&self, offset: u64, length: u32) -> Result<Vec<u8>, std::io::Error> {
        if length == 0 || offset >= self.size {
            return Ok(Vec::new());
        }
        let end = offset.saturating_add(u64::from(length)).min(self.size) - 1;
        let mut headers = self.headers.clone();
        headers.insert(
            reqwest::header::RANGE,
            HeaderValue::from_str(&format!("bytes={offset}-{end}"))
                .map_err(|_| std::io::Error::other("Archive range failed"))?,
        );
        let response = fetch(&self.client, &self.url, Method::GET, &headers)
            .await
            .map_err(std::io::Error::other)?;
        let observed = content_range(&response).map_err(std::io::Error::other)?;
        if observed != (offset, end, self.size) {
            return Err(std::io::Error::other("Archive range changed"));
        }
        let bytes = bounded_body(response, (end - offset + 1) as usize)
            .await
            .map_err(std::io::Error::other)?;
        if bytes.len() as u64 != end - offset + 1 {
            return Err(std::io::Error::other("Archive range truncated"));
        }
        Ok(bytes)
    }
}
fn content_range(response: &reqwest::Response) -> Result<(u64, u64, u64), &'static str> {
    if response.status() != StatusCode::PARTIAL_CONTENT {
        return Err("source_preparation_failed");
    }
    let value = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes "))
        .ok_or("source_preparation_failed")?;
    let (range, size) = value.split_once('/').ok_or("source_preparation_failed")?;
    let (start, end) = range.split_once('-').ok_or("source_preparation_failed")?;
    let parse = |value: &str| {
        value
            .parse::<u64>()
            .map_err(|_| "source_preparation_failed")
    };
    let (start, end, size) = (parse(start)?, parse(end)?, parse(size)?);
    if start > end || end >= size {
        return Err("source_preparation_failed");
    }
    Ok((start, end, size))
}
impl RarVolume for HttpVolume {
    fn size(&self) -> u64 {
        self.size
    }
    fn open(&self) -> Result<Box<dyn playback_engine::rar::ReadSeek>, &'static str> {
        let source = self.clone();
        let runtime = tokio::runtime::Handle::current();
        Ok(Box::new(RangeReader {
            position: 0,
            size: self.size,
            read: Box::new(move |offset, length| runtime.block_on(source.read(offset, length))),
        }))
    }
}
struct RangeReader {
    position: u64,
    size: u64,
    read: Box<dyn Fn(u64, u32) -> std::io::Result<Vec<u8>> + Send>,
}
impl Read for RangeReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() || self.position >= self.size {
            return Ok(0);
        }
        let length = buffer.len().min(64 * 1024) as u32;
        let bytes = (self.read)(self.position, length)?;
        if bytes.is_empty() || bytes.len() > length as usize {
            return Err(std::io::Error::other("Archive source truncated"));
        }
        buffer[..bytes.len()].copy_from_slice(&bytes);
        self.position += bytes.len() as u64;
        Ok(bytes.len())
    }
}
impl Seek for RangeReader {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        let next = match position {
            SeekFrom::Start(value) => i128::from(value),
            SeekFrom::End(value) => i128::from(self.size) + i128::from(value),
            SeekFrom::Current(value) => i128::from(self.position) + i128::from(value),
        };
        self.position =
            u64::try_from(next).map_err(|_| std::io::Error::other("Invalid archive seek"))?;
        Ok(self.position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::{EgressProxy, NetworkPolicy};
    use axum::{
        body::Body,
        http::{HeaderMap as HttpHeaders, Response},
        routing::get,
        Router,
    };
    fn fixture_input(url: &str) -> Input {
        serde_json::from_value(serde_json::json!({
            "url": url, "live": false,
            "headers": {"Authorization":"Bearer fixture", "X-api-key":"private-fixture", "X-csrf-token":"csrf-fixture"}
        })).unwrap()
    }
    async fn fixture_sources(proxy: &str) -> Sources {
        Sources::new(TorrentConfig::default(), proxy).unwrap()
    }
    fn fixture_range(headers: &HttpHeaders, bytes: &[u8]) -> Response<Body> {
        let range = headers
            .get("range")
            .unwrap()
            .to_str()
            .unwrap()
            .strip_prefix("bytes=")
            .unwrap();
        let (start, end) = range.split_once('-').unwrap();
        let (start, end) = (
            start.parse::<usize>().unwrap(),
            end.parse::<usize>().unwrap(),
        );
        Response::builder()
            .status(206)
            .header(
                "content-range",
                format!("bytes {start}-{end}/{}", bytes.len()),
            )
            .body(Body::from(bytes[start..=end].to_vec()))
            .unwrap()
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn header_media_relay_coalesces_scopes_ranges_and_revokes_last_owner() {
        let contacts = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new([b"\x00\x00\x00\x18ftypisom".as_slice(), &[7; 1012]].concat());
        let router = Router::new().route(
            "/media",
            get({
                let bytes = bytes.clone();
                let contacts = contacts.clone();
                move |headers: HttpHeaders| {
                    let bytes = bytes.clone();
                    let contacts = contacts.clone();
                    async move {
                        assert_eq!(headers.get("x-api-key").unwrap(), "private-fixture");
                        assert_eq!(headers.get("x-csrf-token").unwrap(), "csrf-fixture");
                        contacts.fetch_add(1, Ordering::SeqCst);
                        fixture_range(&headers, &bytes)
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let input = fixture_input(&format!("http://{}/media", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let proxy = EgressProxy::start(NetworkPolicy::loopback_fixture())
            .await
            .unwrap();
        let sources = fixture_sources(&proxy.url()).await;
        let (first, second) = tokio::join!(
            sources.resolve("a", &input, None),
            sources.resolve("a", &input, None)
        );
        let (first, second) = (first.unwrap(), second.unwrap());
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(
            contacts.load(Ordering::SeqCst),
            2,
            "one size probe and one media signature probe"
        );
        assert!(!first.file_metadata);
        let other_scope = sources.resolve("b", &input, None).await.unwrap();
        assert_ne!(first.url, other_scope.url);
        let client = Client::builder().no_proxy().build().unwrap();
        let response = client
            .get(&first.url)
            .header("Range", "bytes=511-519")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.bytes().await.unwrap().as_ref(), &bytes[511..520]);
        let url = first.url.clone();
        let map_owner = first.clone();
        sources.close_if_unused(&first).await.unwrap();
        assert!(client
            .head(&url)
            .send()
            .await
            .unwrap()
            .status()
            .is_success());
        drop(map_owner);
        drop(first);
        sources.close_if_unused(&second).await.unwrap();
        assert!(client.get(&url).send().await.is_err());
        assert!(client
            .head(&other_scope.url)
            .send()
            .await
            .unwrap()
            .status()
            .is_success());
        drop(second);
        drop(other_scope);
        sources.reap().await.unwrap();
        server.abort();
    }
    #[tokio::test]
    async fn header_media_redirect_strips_all_provider_headers_on_every_range() {
        let contacts = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new([b"\x00\x00\x00\x18ftypisom".as_slice(), &[7; 1012]].concat());
        let target = Router::new().route(
            "/media",
            get({
                let contacts = contacts.clone();
                move |headers: HttpHeaders| {
                    let contacts = contacts.clone();
                    let bytes = bytes.clone();
                    async move {
                        assert!(!headers.contains_key("authorization"));
                        assert!(!headers.contains_key("x-api-key"));
                        assert!(!headers.contains_key("x-csrf-token"));
                        assert_eq!(headers.get("accept-encoding").unwrap(), "identity");
                        contacts.fetch_add(1, Ordering::SeqCst);
                        fixture_range(&headers, &bytes)
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let location = format!("http://{}/media", listener.local_addr().unwrap());
        let target = tokio::spawn(async move { axum::serve(listener, target).await.unwrap() });
        let origin = Router::new().route(
            "/start",
            get(move |headers: HttpHeaders| {
                let location = location.clone();
                async move {
                    assert_eq!(headers.get("x-api-key").unwrap(), "private-fixture");
                    Response::builder()
                        .status(307)
                        .header("Location", location)
                        .body(Body::empty())
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let input = fixture_input(&format!("http://{}/start", listener.local_addr().unwrap()));
        let origin = tokio::spawn(async move { axum::serve(listener, origin).await.unwrap() });
        let proxy = EgressProxy::start(NetworkPolicy::loopback_fixture())
            .await
            .unwrap();
        let sources = fixture_sources(&proxy.url()).await;
        let source = sources.resolve("a", &input, None).await.unwrap();
        let response = Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(&source.url)
            .header("Range", "bytes=40-47")
            .send()
            .await
            .unwrap();
        assert_eq!(response.bytes().await.unwrap().len(), 8);
        assert_eq!(contacts.load(Ordering::SeqCst), 3);
        drop(source);
        sources.reap().await.unwrap();
        origin.abort();
        target.abort();
    }
    #[tokio::test]
    async fn header_media_nonrange_and_disguised_manifest_fail_closed() {
        let router = Router::new()
            .route("/nonrange", get(|| async { "binary without range" }))
            .route(
                "/fake.mp4",
                get(|headers: HttpHeaders| async move {
                    fixture_range(
                        &headers,
                        b"#EXTM3U\n#EXT-X-VERSION:3\nhttp://example.org/secret.ts\n",
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let proxy = EgressProxy::start(NetworkPolicy::loopback_fixture())
            .await
            .unwrap();
        let sources = fixture_sources(&proxy.url()).await;
        assert_eq!(
            sources
                .resolve("a", &fixture_input(&format!("{origin}/nonrange")), None)
                .await
                .err(),
            Some("source_preparation_failed")
        );
        assert_eq!(
            sources
                .resolve("a", &fixture_input(&format!("{origin}/fake.mp4")), None)
                .await
                .err(),
            Some("unsupported_output")
        );
        let mut live = fixture_input(&format!("{origin}/nonrange"));
        live.live = true;
        assert_eq!(
            sources.resolve("a", &live, None).await.err(),
            Some("unsupported_output")
        );
        assert!(sources.resolved.lock().await.is_empty());
        server.abort();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canceled_blocking_archive_keeps_budget_until_reader_and_spool_close() {
        struct GatedVolume {
            opened: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
            release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl RarVolume for GatedVolume {
            fn size(&self) -> u64 {
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../engine/assets/rar/compressed.rar"
                ))
                .len() as u64
            }
            fn open(&self) -> Result<Box<dyn playback_engine::rar::ReadSeek>, &'static str> {
                if let Some(opened) = self.opened.lock().unwrap().take() {
                    let _ = opened.send(());
                    self.release.lock().unwrap().recv().unwrap();
                }
                Ok(Box::new(std::io::Cursor::new(
                    include_bytes!(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/../engine/assets/rar/compressed.rar"
                    ))
                    .to_vec(),
                )))
            }
        }
        let root = tempfile::tempdir().unwrap();
        let archive_cache = ArchiveCache::new(
            root.path().join("cache"),
            true,
            playback_engine::input::MIN_CACHE_BYTES,
        )
        .unwrap();
        let retained = archive_cache
            .reserve_cache(playback_engine::input::MIN_CACHE_BYTES - 10000)
            .await
            .unwrap();
        let reservation = archive_cache.reserve_cache(10000).await.unwrap();
        let (opened, opening) = tokio::sync::oneshot::channel();
        let (release, releasing) = std::sync::mpsc::channel();
        let task = tokio::spawn(extract_owned(
            vec![Arc::new(GatedVolume {
                opened: std::sync::Mutex::new(Some(opened)),
                release: std::sync::Mutex::new(releasing),
            })],
            reservation,
            None,
        ));
        opening.await.unwrap();
        task.abort();
        let _ = task.await;
        assert!(
            archive_cache.reserve_cache(1).await.is_err(),
            "cancellation must not refund bytes still owned by a blocking decoder"
        );
        release.send(()).unwrap();
        let restored = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(reservation) = archive_cache.reserve_cache(10000).await {
                    break reservation;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(restored);
        drop(retained);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_archive_range_keeps_orphan_budget_and_cleanup_until_blocking_reader_finishes(
    ) {
        struct GatedVolume {
            armed: Arc<AtomicBool>,
            opened: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
            release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl RarVolume for GatedVolume {
            fn size(&self) -> u64 {
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../engine/assets/rar/stored.rar"
                ))
                .len() as u64
            }
            fn open(&self) -> Result<Box<dyn playback_engine::rar::ReadSeek>, &'static str> {
                if self.armed.load(Ordering::Acquire) {
                    if let Some(opened) = self.opened.lock().unwrap().take() {
                        let _ = opened.send(());
                        self.release.lock().unwrap().recv().unwrap();
                    }
                }
                Ok(Box::new(std::io::Cursor::new(
                    include_bytes!(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/../engine/assets/rar/stored.rar"
                    ))
                    .to_vec(),
                )))
            }
        }
        let root = tempfile::tempdir().unwrap();
        let sources = Arc::new(
            Sources::new(
                TorrentConfig {
                    enabled: true,
                    cache_dir: root.path().join("cache"),
                    enable_dht: false,
                    max_bytes: playback_engine::input::MIN_CACHE_BYTES,
                    ..Default::default()
                },
                "http://127.0.0.1:9",
            )
            .unwrap(),
        );
        let retained = sources
            .archive_cache
            .reserve_cache(playback_engine::input::MIN_CACHE_BYTES - 10000)
            .await
            .unwrap();
        let reservation = sources.archive_cache.reserve_cache(10000).await.unwrap();
        let armed = Arc::new(AtomicBool::new(false));
        let (opened, opening) = tokio::sync::oneshot::channel();
        let (release, releasing) = std::sync::mpsc::channel();
        let media = RarMedia::open(
            vec![Arc::new(GatedVolume {
                armed: armed.clone(),
                opened: std::sync::Mutex::new(Some(opened)),
                release: std::sync::Mutex::new(releasing),
            })],
            root.path(),
            0,
        )
        .unwrap();
        let reads = Arc::new(ArchiveReads::default());
        let stream = LoopbackStream::from_source(Arc::new(ArchiveSource {
            media,
            _reservation: Arc::new(reservation),
            reads: reads.clone(),
        }))
        .await
        .unwrap();
        let url = stream.url(0);
        let source = Arc::new(ResolvedSource {
            client_frame_ack: false,
            url: url.clone(),
            file_metadata: true,
            file: TorrentFile {
                index: 0,
                name: "clip.mp4".into(),
                size: 100,
            },
            _archive: Some(stream),
            runtime: None,
            reads: reads.clone(),
        });
        sources.resolved.lock().await.insert(
            [0; 32],
            CachedSource {
                source: Arc::downgrade(&source),
                reads,
            },
        );
        armed.store(true, Ordering::Release);
        let request = tokio::spawn({
            let url = url.clone();
            async move {
                Client::builder()
                    .no_proxy()
                    .build()
                    .unwrap()
                    .get(url)
                    .header("Range", "bytes=10-17")
                    .send()
                    .await
                    .unwrap()
                    .bytes()
                    .await
            }
        });
        opening.await.unwrap();
        let cleanup = tokio::spawn({
            let sources = sources.clone();
            let source = source.clone();
            async move { sources.close_if_unused(&source).await }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            !cleanup.is_finished(),
            "last source cleanup must wait for the actual blocking range reader"
        );
        cleanup.abort();
        let _ = cleanup.await;
        request.abort();
        let _ = request.await;
        drop(source);
        assert!(
            sources.archive_cache.reserve_cache(1).await.is_err(),
            "cancelled HTTP/source futures cannot refund a blocking reader's cache owner"
        );
        let reap = tokio::spawn({
            let sources = sources.clone();
            async move { sources.reap().await }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            !reap.is_finished(),
            "orphan cleanup must retain pending source admission until the reader returns"
        );
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), reap)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let restored = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(reservation) = sources.archive_cache.reserve_cache(10000).await {
                    break reservation;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(restored);
        drop(retained);
        assert!(Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(url)
            .send()
            .await
            .is_err());
    }

    #[tokio::test]
    async fn disabled_sources_reject_before_creating_any_cache() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache");
        let sources = Sources::new(
            TorrentConfig {
                cache_dir: cache.clone(),
                ..Default::default()
            },
            "http://127.0.0.1:9",
        )
        .unwrap();
        let input: Input = serde_json::from_value(serde_json::json!({"url":"magnet:?xt=urn:btih:1111111111111111111111111111111111111111","live":false})).unwrap();
        assert_eq!(
            sources.resolve("fixture", &input, None).await.err(),
            Some("unsupported_output")
        );
        assert!(!cache.exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stored_http_archive_uses_private_ranges_and_shared_source_lifetime() {
        let bytes = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../engine/assets/rar/stored.rar"
        ));
        let router = Router::new().route(
            "/clip.rar",
            get(move |headers: HttpHeaders| async move {
                let value = headers
                    .get("range")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .strip_prefix("bytes=")
                    .unwrap();
                let (start, end) = value.split_once('-').unwrap();
                let (start, end) = (
                    start.parse::<usize>().unwrap(),
                    end.parse::<usize>().unwrap(),
                );
                Response::builder()
                    .status(206)
                    .header(
                        "content-range",
                        format!("bytes {start}-{end}/{}", bytes.len()),
                    )
                    .header("content-length", end - start + 1)
                    .body(Body::from(bytes[start..=end].to_vec()))
                    .unwrap()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/clip.rar", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let proxy = EgressProxy::start(NetworkPolicy::loopback_fixture())
            .await
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let sources = Sources::new(
            TorrentConfig {
                enabled: true,
                cache_dir: root.path().join("cache"),
                enable_dht: false,
                ..Default::default()
            },
            &proxy.url(),
        )
        .unwrap();
        let input: Input =
            serde_json::from_value(serde_json::json!({"url":url,"live":false})).unwrap();
        let first = sources.resolve("fixture", &input, None).await.unwrap();
        let second = sources.resolve("fixture", &input, None).await.unwrap();
        assert_eq!(first.url, second.url);
        assert_eq!(first.file.name, "clip.mp4");
        let client = Client::builder().no_proxy().build().unwrap();
        let response = client
            .get(&first.url)
            .header("range", "bytes=10-17")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.bytes().await.unwrap().as_ref(), b"01234567");
        let range_url = first.url.clone();
        // Simulate two job-map owners plus the reaper's local reference.
        let other_job = first.clone();
        sources.close_if_unused(&first).await.unwrap();
        assert!(
            client
                .head(&range_url)
                .send()
                .await
                .unwrap()
                .status()
                .is_success(),
            "closing one output must preserve another output sharing its source"
        );
        drop(other_job);
        drop(first);
        assert!(client
            .head(&range_url)
            .send()
            .await
            .unwrap()
            .status()
            .is_success());
        drop(second);
        sources.reap().await.unwrap();
        assert!(client.get(range_url).send().await.is_err());
        server.abort();
    }

    #[tokio::test]
    async fn ordinary_egress_policy_blocks_private_archive_before_contact() {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = EgressProxy::start(NetworkPolicy::default()).await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let sources = Sources::new(
            TorrentConfig {
                enabled: true,
                cache_dir: root.path().join("cache"),
                ..Default::default()
            },
            &proxy.url(),
        )
        .unwrap();
        let input: Input = serde_json::from_value(serde_json::json!({"url":format!("http://{}/secret.rar",target.local_addr().unwrap()),"live":false})).unwrap();
        assert_eq!(
            sources.resolve("fixture", &input, None).await.err(),
            Some("source_preparation_failed")
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), target.accept())
                .await
                .is_err()
        );
    }
}
