//! Token-protected loopback Range transport; independent of the media/axum path.
use crate::torrent::HandleState;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full, StreamBody};
use hyper::{
    body::{Bytes, Frame, Incoming},
    header,
    service::service_fn,
    Method, Request, Response, StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use librqbit::OwnedTasks;
use std::{
    convert::Infallible,
    io,
    sync::{atomic::Ordering, Arc, Weak},
    time::Duration,
};
use tokio::{net::TcpListener, sync::Semaphore, task::JoinSet};
use uuid::Uuid;
type Body = UnsyncBoxBody<Bytes, io::Error>;
pub trait TorrentRangeSource: Send + Sync {
    fn file_size(&self, index: u32) -> Option<u64>;
    fn is_closed(&self) -> bool {
        false
    }
    fn file_extension(&self, _index: u32) -> String {
        "bin".into()
    }
    fn read_range(
        &self,
        index: u32,
        offset: u64,
        length: u32,
    ) -> futures::future::BoxFuture<'_, Result<Vec<u8>, String>>;
}
pub struct LoopbackStream {
    token: String,
    port: u16,
    tasks: OwnedTasks,
    source: Weak<dyn TorrentRangeSource>,
    _source: Option<Arc<dyn TorrentRangeSource>>,
}
impl LoopbackStream {
    pub(crate) async fn start(source: Weak<HandleState>) -> Result<Self, String> {
        let source: Weak<dyn TorrentRangeSource> = source;
        Self::start_weak(source, None, OwnedTasks::new(Default::default())).await
    }
    pub(crate) async fn start_owned(
        source: Weak<HandleState>,
        tasks: OwnedTasks,
    ) -> Result<Self, String> {
        let source: Weak<dyn TorrentRangeSource> = source;
        Self::start_weak(source, None, tasks).await
    }
    pub async fn from_source(source: Arc<dyn TorrentRangeSource>) -> Result<Self, String> {
        Self::start_weak(
            Arc::downgrade(&source),
            Some(source),
            OwnedTasks::new(Default::default()),
        )
        .await
    }
    async fn start_weak(
        source: Weak<dyn TorrentRangeSource>,
        owned: Option<Arc<dyn TorrentRangeSource>>,
        tasks: OwnedTasks,
    ) -> Result<Self, String> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| "Torrent stream unavailable")?;
        let port = listener
            .local_addr()
            .map_err(|_| "Torrent stream unavailable")?
            .port();
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let key = token.clone();
        let weak = source.clone();
        let connections = tasks.clone();
        tasks.spawn(tracing::Span::none(), async move {
            let permits = Arc::new(Semaphore::new(16));
            let mut requests = JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=> {
                        let Ok((socket,_))=accepted else {break};
                        let Ok(permit)=permits.clone().try_acquire_owned() else {drop(socket); continue};
                        let source=source.clone(); let key=key.clone();
                        let Some(task) = connections.spawn(tracing::Span::none(), async move {
                            let _permit=permit;
                            let service=service_fn(move |request| serve(request,source.clone(),key.clone()));
                            let mut builder=hyper::server::conn::http1::Builder::new();
                            builder.keep_alive(false).timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(10)).max_buf_size(16*1024);
                            let _=builder.serve_connection(TokioIo::new(socket),service).await;
                            Ok(())
                        }) else { continue };
                        requests.spawn(async move { let _ = task.await; });
                    },
                    _=requests.join_next(),if !requests.is_empty()=> {}
                }
            }
            Ok(())
        }).ok_or("Torrent stream stopped")?;
        Ok(Self {
            token,
            port,
            tasks,
            source: weak,
            _source: owned,
        })
    }
    pub fn url(&self, index: u32) -> String {
        format!(
            "http://127.0.0.1:{}/{}/{}/stream.{}",
            self.port,
            self.token,
            index,
            self.source
                .upgrade()
                .map(|s| s.file_extension(index))
                .filter(|s| s.len() <= 8 && s.chars().all(|c| c.is_ascii_alphanumeric()))
                .unwrap_or_else(|| "bin".into())
        )
    }
    pub fn stop(&self) {
        self.tasks.cancel();
    }
    pub async fn stop_and_wait_until(&self, deadline: tokio::time::Instant) -> bool {
        self.tasks.cancel_and_wait_until(deadline).await && !self.tasks.has_failed_settlement()
    }
    pub fn has_pending_work(&self) -> bool {
        self.tasks.has_pending_work()
    }
}
impl Drop for LoopbackStream {
    fn drop(&mut self) {
        self.stop();
    }
}
fn empty(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(
            Full::new(Bytes::new())
                .map_err(|e: Infallible| match e {})
                .boxed_unsync(),
        )
        .expect("static response")
}
fn equal_secret(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0;
    for (a, b) in a.iter().zip(b) {
        diff |= a ^ b;
    }
    diff == 0
}
async fn serve(
    request: Request<Incoming>,
    source: Weak<dyn TorrentRangeSource>,
    token: String,
) -> Result<Response<Body>, Infallible> {
    let parts: Vec<_> = request.uri().path().split('/').collect();
    if parts.len() != 4
        || !parts[0].is_empty()
        || !parts[3].strip_prefix("stream.").is_some_and(|s| {
            !s.is_empty() && s.len() <= 8 && s.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        || !equal_secret(parts[1].as_bytes(), token.as_bytes())
        || request.uri().query().is_some()
    {
        return Ok(empty(StatusCode::NOT_FOUND));
    }
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return Ok(empty(StatusCode::METHOD_NOT_ALLOWED));
    }
    let Some(state) = source.upgrade().filter(|s| !s.is_closed()) else {
        return Ok(empty(StatusCode::GONE));
    };
    let Some(index) = parts[2]
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| parts[2].parse::<u32>().ok())
        .flatten()
    else {
        return Ok(empty(StatusCode::NOT_FOUND));
    };
    let Some(size) = state.file_size(index) else {
        return Ok(empty(StatusCode::NOT_FOUND));
    };
    let duplicate_range = request.headers().get_all(header::RANGE).iter().count() > 1;
    let range = match request.headers().get(header::RANGE) {
        None => Ok((0, size, false)),
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|v| parse_range(v, size))
            .map(|(start, count)| (start, count, true))
            .ok_or(()),
    };
    let range = if duplicate_range { Err(()) } else { range };
    let Ok((start, count, partial)) = range else {
        let mut response = empty(StatusCode::RANGE_NOT_SATISFIABLE);
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            format!("bytes */{size}").parse().expect("numeric header"),
        );
        return Ok(response);
    };
    drop(state);
    let body = if request.method() == Method::HEAD {
        empty(StatusCode::OK).into_body()
    } else {
        let stream = async_stream::try_stream! {
            let mut offset=start; let end=start+count;
            while offset<end {
                let Some(state)=source.upgrade().filter(|s|!s.is_closed()) else { Err(io::Error::new(io::ErrorKind::Interrupted,"Torrent stream stopped"))?; unreachable!() };
                let length=(end-offset).min(64*1024) as u32;
                let bytes=state.read_range(index,offset,length).await.map_err(io::Error::other)?;
                if bytes.is_empty(){Err(io::Error::new(io::ErrorKind::UnexpectedEof,"Torrent stream truncated"))?;}
                offset+=bytes.len() as u64;
                yield Frame::data(Bytes::from(bytes));
            }
        };
        StreamBody::new(stream).boxed_unsync()
    };
    let mut builder = Response::builder()
        .status(if partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, count)
        .header(header::CACHE_CONTROL, "no-store");
    if partial {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{size}", start, start + count - 1),
        );
    }
    Ok(builder.body(body).expect("numeric headers"))
}
fn parse_range(value: &str, size: u64) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes=")?;
    if value.contains(',') || !value.bytes().all(|b| b.is_ascii_digit() || b == b'-') || size == 0 {
        return None;
    }
    let (start, end) = value.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?;
        if suffix == 0 {
            return None;
        }
        let count = suffix.min(size);
        return Some((size - count, count));
    }
    let start = start.parse::<u64>().ok()?;
    if start >= size {
        return None;
    }
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().ok()?.min(size - 1)
    };
    if end < start {
        return None;
    }
    Some((start, end - start + 1))
}

impl TorrentRangeSource for HandleState {
    fn file_size(&self, index: u32) -> Option<u64> {
        self.file(index).map(|f| f.size)
    }
    fn is_closed(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
    fn file_extension(&self, index: u32) -> String {
        self.file(index)
            .and_then(|f| f.name.rsplit('.').next())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_else(|| "bin".into())
    }
    fn read_range(
        &self,
        index: u32,
        offset: u64,
        length: u32,
    ) -> futures::future::BoxFuture<'_, Result<Vec<u8>, String>> {
        Box::pin(self.read_range(index, offset, length))
    }
}
