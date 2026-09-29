//! Bounded, consumption-paced fragmented MP4 for a single on-demand viewer.
//! The output never accumulates on disk or waits for the complete source.
use super::*;
use axum::{
    body::{Body, Bytes},
    http::Method,
    response::Response,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::{mpsc, watch};

pub(super) struct Stream {
    receiver: Mutex<Option<mpsc::Receiver<Result<Bytes, std::io::Error>>>>,
    ready: watch::Receiver<bool>,
    closed: watch::Sender<bool>,
    completed: watch::Sender<Option<bool>>,
    produced: Arc<AtomicU64>,
    consumed: AtomicU64,
    origin: f64,
    backpressured: Arc<AtomicBool>,
}
impl Stream {
    pub(super) fn new(
        mut output: tokio::process::ChildStdout,
        mut progress: tokio::process::ChildStderr,
        origin: f64,
        permits: Arc<InputPermits>,
    ) -> Arc<Self> {
        let (send, receiver) = mpsc::channel(32); // 32 * 64 KiB, plus one reader block.
        let (ready, ready_rx) = watch::channel(false);
        let (closed, mut cancelled) = watch::channel(false);
        let (completed, mut completion) = watch::channel(None);
        let produced = Arc::new(AtomicU64::new(0));
        let position = produced.clone();
        let mut stop_progress = closed.subscribe();
        tokio::spawn(async move {
            let mut buffer = [0; 4096];
            let mut line = Vec::new();
            loop {
                let read = tokio::select! { read = progress.read(&mut buffer) => read, _ = stop_progress.changed() => break };
                let Ok(count) = read else { break };
                if count == 0 {
                    break;
                }
                for byte in &buffer[..count] {
                    if *byte == b'\n' {
                        if let Some(value) = std::str::from_utf8(&line)
                            .ok()
                            .and_then(|line| line.strip_prefix("out_time_us="))
                            .and_then(|value| value.parse::<u64>().ok())
                        {
                            position.fetch_max(value, Ordering::Relaxed);
                        }
                        line.clear();
                    } else if line.len() < 256 {
                        line.push(*byte);
                    }
                }
            }
        });
        let backpressured = Arc::new(AtomicBool::new(false));
        let blocked = backpressured.clone();
        tokio::spawn(async move {
            let _permits = permits;
            let mut bytes = vec![0; 64 * 1024];
            loop {
                if *cancelled.borrow() {
                    break;
                }
                let read = tokio::select! { read = output.read(&mut bytes) => read, _ = cancelled.changed() => break };
                match read {
                    Ok(0) => {
                        while completion.borrow_and_update().is_none() {
                            tokio::select! { _ = completion.changed() => {}, _ = cancelled.changed() => break }
                            if *cancelled.borrow() {
                                break;
                            }
                        }
                        if *completion.borrow() != Some(true) {
                            let _ =
                                send.try_send(Err(std::io::Error::other("Media producer failed")));
                        }
                        break;
                    }
                    Ok(n) => {
                        ready.send_replace(true);
                        blocked.store(send.capacity() == 0, Ordering::Release);
                        tokio::select! { result = send.send(Ok(Bytes::copy_from_slice(&bytes[..n]))) => { if result.is_err() { break; } }, _ = cancelled.changed() => break }
                        blocked.store(false, Ordering::Release);
                    }
                    Err(_) => {
                        let _ =
                            send.try_send(Err(std::io::Error::other("Media output interrupted")));
                        break;
                    }
                }
            }
        });
        Arc::new(Self {
            receiver: Mutex::new(Some(receiver)),
            ready: ready_rx,
            closed,
            completed,
            produced,
            consumed: AtomicU64::new(0),
            origin,
            backpressured,
        })
    }
    pub(super) async fn ready(&self) -> Result<(), String> {
        let mut ready = self.ready.clone();
        timeout(Duration::from_secs(15), async {
            while !*ready.borrow_and_update() {
                ready
                    .changed()
                    .await
                    .map_err(|_| "Media output unavailable")?;
            }
            Ok::<_, &str>(())
        })
        .await
        .map_err(|_| "Media output timed out")?
        .map_err(str::to_owned)
    }
    pub(super) fn backpressured(&self) -> bool {
        self.backpressured.load(Ordering::Acquire)
    }
    pub(super) fn close(&self) {
        self.closed.send_replace(true);
    }
    pub(super) fn complete(&self, success: bool) {
        self.completed.send_replace(Some(success));
    }
    pub(super) fn observe(&self, position: f64) {
        if position.is_finite() {
            self.consumed.fetch_max(
                ((position - self.origin).max(0.0) * 1_000_000.0) as u64,
                Ordering::Relaxed,
            );
        }
    }
    pub(super) fn times(&self) -> (f64, f64) {
        (
            self.produced.load(Ordering::Relaxed) as f64 / 1_000_000.0,
            self.consumed.load(Ordering::Relaxed) as f64 / 1_000_000.0,
        )
    }
    pub(super) async fn serve(self: Arc<Self>, method: Method) -> Result<Response, String> {
        let builder = Response::builder()
            .header("Content-Type", "video/mp4")
            .header("Cache-Control", "no-store")
            .header("Accept-Ranges", "none");
        if method == Method::HEAD {
            return builder
                .body(Body::empty())
                .map_err(|_| "Invalid media response".into());
        }
        let mut receive = self
            .receiver
            .lock()
            .await
            .take()
            .ok_or("Media stream already opened; restart playback")?;
        let stream = async_stream::try_stream! {
            let mut closed = self.closed.subscribe();
            loop {
                if *closed.borrow() { Err(std::io::Error::other("Media access revoked"))?; }
                let next = tokio::select! { next = receive.recv() => Ok(next), _ = closed.changed() => Err(std::io::Error::other("Media access revoked")) };
                let Some(chunk) = next? else { break };
                yield chunk?;
            }
        };
        builder
            .body(Body::from_stream(futures::StreamExt::map(
                stream,
                |item: Result<Bytes, std::io::Error>| item,
            )))
            .map_err(|_| "Invalid media response".into())
    }
}
