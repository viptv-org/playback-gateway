//! Shared-runtime torrent acquisition; private source bytes never become diagnostics.
use crate::protocol::Input;
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Mutex as AsyncMutex;
use torrent_runtime_host::Worker;

pub struct RuntimeSources {
    executable: PathBuf,
    cache: PathBuf,
    capacity: u64,
    worker: Mutex<Option<Arc<Worker>>>,
    retirements: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
}
pub struct RuntimeSource {
    pub url: String,
    pub file_index: u32,
    pub size: u64,
    worker: Arc<Worker>,
    handle: String,
    closed: std::sync::atomic::AtomicBool,
    settled: AsyncMutex<bool>,
    renewal: Mutex<Option<tokio::task::JoinHandle<()>>>,
    retirements: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
    // Admission survives cancellation until the owned handle actually settles.
    pending: Option<Arc<dyn Send + Sync>>,
}
impl RuntimeSources {
    pub fn new(executable: PathBuf, cache: PathBuf, capacity: u64) -> Self {
        Self {
            executable,
            cache,
            capacity,
            worker: Mutex::new(None),
            retirements: Arc::new(Mutex::new(Vec::new())),
        }
    }
    pub async fn initialize(self: &Arc<Self>) -> Result<(), &'static str> {
        self.worker().await.map(|_| ())
    }
    async fn worker(self: &Arc<Self>) -> Result<Arc<Worker>, &'static str> {
        let owner = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = owner
                .worker
                .lock()
                .map_err(|_| "torrent_worker_unavailable")?;
            if let Some(worker) = guard.as_ref().filter(|worker| worker.is_alive()) {
                return Ok(worker.clone());
            }
            if let Some(worker) = guard.as_ref() {
                worker.terminate().map_err(|_| "torrent_worker_unsettled")?;
            }
            let worker = Arc::new(
                Worker::start(&owner.executable, &owner.cache, owner.capacity)
                    .map_err(|_| "torrent_worker_unavailable")?,
            );
            *guard = Some(worker.clone());
            Ok(worker)
        })
        .await
        .map_err(|_| "torrent_worker_unavailable")?
    }
    pub async fn resolve(
        self: &Arc<Self>,
        input: &Input,
        metainfo: Option<Vec<u8>>,
        pending: Option<Arc<dyn Send + Sync>>,
    ) -> Result<Arc<RuntimeSource>, &'static str> {
        let worker = self.worker().await?;
        let mut source = json!({});
        if let Some(bytes) = metainfo {
            source["metainfo"] = json!(base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                bytes
            ));
        } else {
            source["magnet"] = json!(input.url);
        }
        if let Some(index) = input.file_index {
            source["file_index"] = json!(index);
        }
        if let Some(index) = input.archive_index {
            source["archive_index"] = json!(index);
        }
        if let Some(size) = input.expected_file_size {
            source["expected_file_size"] = json!(size);
        }
        if !input.trackers.is_empty() {
            source["trackers"] = json!(input.trackers);
        }
        let (send, receive) = tokio::sync::oneshot::channel();
        let acquiring = worker.clone();
        let retirements = self.retirements.clone();
        // The control reply may arrive after the requesting future is cancelled.
        // This task owns admission and installs RAII before it delivers the handle.
        tokio::spawn(async move {
            let result = async {
                let reply = call(
                    acquiring.clone(),
                    json!({"op":"prepare","source":source,"authority_ms":60000}),
                    Duration::from_secs(3),
                )
                .await?;
                let handle = reply["handle"]
                    .as_str()
                    .ok_or("source_preparation_failed")?
                    .to_owned();
                Ok::<_, &'static str>(Arc::new(RuntimeSource {
                    url: String::new(),
                    file_index: 0,
                    size: 0,
                    worker: acquiring,
                    handle,
                    closed: std::sync::atomic::AtomicBool::new(false),
                    settled: AsyncMutex::new(false),
                    renewal: Mutex::new(None),
                    retirements,
                    pending,
                }))
            }
            .await;
            let _ = send.send(result);
        });
        let pending = receive.await.map_err(|_| "source_preparation_failed")??;
        pending.start_renewal();
        loop {
            let reply = call(
                worker.clone(),
                json!({"op":"observe","handle":pending.handle}),
                Duration::from_secs(3),
            )
            .await?;
            let progress = &reply["progress"];
            if let Some(code) = progress["error"].as_str() {
                return Err(if matches!(code, "startup_stalled" | "startup_deadline") {
                    match progress["stage"].as_str() {
                        Some("finding_peers") => "torrent_no_peers",
                        Some("fetching_metadata") => "torrent_metadata_timeout",
                        Some("opening_archive") => "torrent_archive_timeout",
                        _ => "torrent_buffering_timeout",
                    }
                } else {
                    runtime_failure(code)
                });
            }
            if progress["ready"] == true {
                let url = reply["media_url"]
                    .as_str()
                    .ok_or("source_preparation_failed")?
                    .to_owned();
                if !valid_media_url(&url) {
                    return Err("source_preparation_failed");
                }
                let index = u32::try_from(
                    progress["file_index"]
                        .as_u64()
                        .ok_or("source_preparation_failed")?,
                )
                .map_err(|_| "source_preparation_failed")?;
                let size = progress["length"]
                    .as_u64()
                    .filter(|size| *size > 0)
                    .ok_or("source_preparation_failed")?;
                let task = pending
                    .renewal
                    .lock()
                    .map_err(|_| "torrent_worker_unavailable")?
                    .take();
                if let Some(task) = task {
                    task.abort();
                    let _ = task.await;
                }
                let mut owned =
                    Arc::try_unwrap(pending).map_err(|_| "source_preparation_failed")?;
                owned.url = url;
                owned.file_index = index;
                owned.size = size;
                let owned = Arc::new(owned);
                owned.start_renewal();
                return Ok(owned);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    pub async fn reap(&self) -> Result<(), &'static str> {
        let (retired, active) = {
            let mut guard = self
                .retirements
                .lock()
                .map_err(|_| "torrent_worker_unsettled")?;
            let (done, active): (Vec<_>, Vec<_>) = std::mem::take(&mut *guard)
                .into_iter()
                .partition(|thread| thread.is_finished());
            let unsettled = !active.is_empty();
            *guard = active;
            (done, unsettled)
        };
        tokio::task::spawn_blocking(move || {
            for thread in retired {
                thread.join().map_err(|_| "torrent_worker_unsettled")?;
            }
            if active {
                Err("input_cleanup_pending")
            } else {
                Ok(())
            }
        })
        .await
        .map_err(|_| "torrent_worker_unsettled")?
    }

    pub async fn shutdown(&self) -> Result<(), &'static str> {
        let worker = self
            .worker
            .lock()
            .map_err(|_| "torrent_worker_unavailable")?
            .take();
        if let Some(worker) = worker {
            tokio::task::spawn_blocking(move || worker.shutdown())
                .await
                .map_err(|_| "torrent_worker_unsettled")?
                .map_err(|_| "torrent_worker_unsettled")?;
        }
        self.reap().await
    }
}
impl RuntimeSource {
    fn start_renewal(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(20)).await;
                let Some(owner) = weak.upgrade() else {
                    break;
                };
                if owner.closed.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                if call(
                    owner.worker.clone(),
                    json!({"op":"renew","handle":owner.handle,"authority_ms":60000}),
                    Duration::from_secs(3),
                )
                .await
                .is_err()
                {
                    // The private byte grant expires independently even if the host disappears.
                    break;
                }
            }
        });
        if let Ok(mut renewal) = self.renewal.lock() {
            *renewal = Some(task);
        }
    }
    #[cfg(test)]
    pub(crate) async fn fixture_stats(&self) -> Value {
        call(
            self.worker.clone(),
            json!({"op":"fixture_stats"}),
            Duration::from_secs(3),
        )
        .await
        .unwrap()
    }
    pub async fn output_ready(&self) -> Result<(), &'static str> {
        // Gateway FFmpeg has opened this verified source and produced its first output.
        // Public clients retain their own deadline through actual presentation.
        call(
            self.worker.clone(),
            json!({"op":"first_frame","handle":self.handle}),
            Duration::from_secs(3),
        )
        .await
        .map(|_| ())
    }
    pub async fn close(&self) -> Result<(), &'static str> {
        let mut settled = self.settled.lock().await;
        if *settled {
            return Ok(());
        }
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(task) = self
            .renewal
            .lock()
            .map_err(|_| "torrent_worker_unavailable")?
            .take()
        {
            task.abort();
        }
        let worker = self.worker.clone();
        let handle = self.handle.clone();
        tokio::task::spawn_blocking(move || close_owned(&worker, &handle))
            .await
            .map_err(|_| "torrent_worker_unsettled")??;
        *settled = true;
        Ok(())
    }
}
impl Drop for RuntimeSource {
    fn drop(&mut self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        if let Ok(mut task) = self.renewal.lock() {
            if let Some(task) = task.take() {
                task.abort();
            }
        }
        if *self.settled.get_mut() {
            return;
        }
        let worker = self.worker.clone();
        let handle = self.handle.clone();
        let pending = self.pending.take();
        let thread = std::thread::spawn(move || {
            // Do not release admission before the child and its private endpoint settle.
            let _pending = pending;
            while close_owned(&worker, &handle).is_err() {
                std::thread::sleep(Duration::from_secs(1));
            }
        });
        if let Ok(mut retired) = self.retirements.lock() {
            retired.push(thread);
        }
    }
}
fn close_owned(worker: &Worker, handle: &str) -> Result<(), &'static str> {
    match worker.call(
        json!({"op":"close","handle":handle}),
        Duration::from_millis(1750),
    ) {
        Ok(reply) if reply["ok"] == true => Ok(()),
        _ => worker.terminate().map_err(|_| "torrent_worker_unsettled"),
    }
}
pub(crate) fn valid_media_url(value: &str) -> bool {
    let Ok(parsed) = url::Url::parse(value) else {
        return false;
    };
    let token = parsed.path().strip_prefix("/media/").unwrap_or("");
    parsed.scheme() == "http"
        && parsed.host_str() == Some("127.0.0.1")
        && parsed.port().is_some_and(|port| port > 0)
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none()
        && token.len() == 64
        && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}
async fn call(
    worker: Arc<Worker>,
    command: Value,
    budget: Duration,
) -> Result<Value, &'static str> {
    let reply = tokio::task::spawn_blocking(move || worker.call(command, budget))
        .await
        .map_err(|_| "torrent_worker_unavailable")?
        .map_err(|_| "torrent_worker_unavailable")?;
    if reply["ok"] != true {
        return Err(runtime_failure(
            reply["error"]
                .as_str()
                .unwrap_or("source_preparation_failed"),
        ));
    }
    Ok(reply)
}
fn runtime_failure(code: &str) -> &'static str {
    match code {
        "compressed_archive_unsupported" => "compressed_archive_unsupported",
        "encrypted_archive_unsupported" => "encrypted_archive_unsupported",
        "archive_volume_missing" => "archive_volume_missing",
        "archive_metadata_invalid" | "invalid_archive_volume" | "archive_member_unsupported" => {
            "archive_metadata_invalid"
        }
        "startup_stalled" | "startup_deadline" => "startup_timeout",
        "cache_unavailable" | "metadata_cache_unavailable" => "torrent_cache_capacity",
        "input_limit" | "handle_limit" => "input_capacity",
        "private_torrent_unsupported"
        | "invalid_metadata"
        | "file_index_unavailable"
        | "empty_file" => "unsupported_output",
        _ => "source_preparation_failed",
    }
}
