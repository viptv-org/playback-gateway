//! Native, identity-free torrent byte transport. No FFmpeg/control service.
#![cfg(feature = "torrent")]
use playback_engine::{TorrentConfig, TorrentManager};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::runtime::Runtime;

uniffi::setup_scaffolding!();

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum TorrentError {
    #[error("Invalid torrent options")]
    InvalidOptions,
    #[error("Torrent operation failed")]
    OperationFailed,
    #[error("Torrent handle stopped")]
    Stopped,
}
#[derive(Clone, uniffi::Record)]
pub struct TorrentOptions {
    pub max_cache_bytes: u64,
    pub listen_port: Option<u16>,
    pub enable_dht: bool,
    /// Operator-supplied bootstrap peers. Never include these in diagnostics.
    pub initial_peers: Vec<String>,
}
#[uniffi::export]
pub fn default_torrent_options() -> TorrentOptions {
    TorrentOptions {
        max_cache_bytes: 2 * 1024 * 1024 * 1024,
        listen_port: None,
        enable_dht: true,
        initial_peers: Vec::new(),
    }
}
#[derive(Clone, Debug, uniffi::Record)]
pub struct TorrentFile {
    pub index: u32,
    pub name: String,
    pub size: u64,
}
#[derive(Clone, Debug, uniffi::Record)]
pub struct TorrentProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub finished: bool,
    pub stopped: bool,
}
#[uniffi::export(callback_interface)]
pub trait TorrentProgressListener: Send + Sync {
    fn on_status(&self, status: TorrentProgress);
}

/// Clients sharing a cache must share this object; each cache has an exclusive
/// lock. Methods performing IO block and belong on the caller's IO dispatcher.
#[derive(uniffi::Object)]
pub struct TorrentClient {
    manager: TorrentManager,
}
#[derive(uniffi::Object)]
pub struct TorrentHandle {
    handle: playback_engine::TorrentHandle,
    callback: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
// A process-lifetime runtime avoids dropping a runtime from a foreign callback
// on one of its own workers. Managers still cancel all session IO on final drop.
fn runtime() -> Result<&'static Runtime, TorrentError> {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    if let Some(runtime) = RUNTIME.get() {
        return Ok(runtime);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("torrent-runtime")
        .build()
        .map_err(|_| TorrentError::OperationFailed)?;
    let _ = RUNTIME.set(runtime);
    RUNTIME.get().ok_or(TorrentError::OperationFailed)
}
#[uniffi::export]
impl TorrentClient {
    #[uniffi::constructor]
    pub fn new(cache_dir: String, options: TorrentOptions) -> Result<Arc<Self>, TorrentError> {
        if cache_dir.is_empty() {
            return Err(TorrentError::InvalidOptions);
        }
        let peers = options
            .initial_peers
            .iter()
            .map(|p| p.parse())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| TorrentError::InvalidOptions)?;
        let manager = TorrentManager::new(TorrentConfig {
            enabled: true,
            cache_dir: PathBuf::from(cache_dir),
            max_bytes: options.max_cache_bytes,
            listen_port: options.listen_port,
            enable_dht: options.enable_dht,
            initial_peers: peers,
            ..Default::default()
        })
        .map_err(|_| TorrentError::InvalidOptions)?;
        runtime()?;
        Ok(Arc::new(Self { manager }))
    }
    pub fn start(
        &self,
        magnet: String,
        file_index: Option<u32>,
    ) -> Result<Arc<TorrentHandle>, TorrentError> {
        let handle = runtime()?
            .block_on(self.manager.start_magnet("native", &magnet, file_index))
            .map_err(|_| TorrentError::OperationFailed)?;
        Ok(Arc::new(TorrentHandle {
            handle,
            callback: Mutex::new(None),
        }))
    }
    /// HTTP metainfo must be fetched with the application's vetted URL policy.
    pub fn start_metainfo(
        &self,
        metainfo: Vec<u8>,
        file_index: Option<u32>,
    ) -> Result<Arc<TorrentHandle>, TorrentError> {
        let handle = runtime()?
            .block_on(self.manager.start_metainfo("native", metainfo, file_index))
            .map_err(|_| TorrentError::OperationFailed)?;
        Ok(Arc::new(TorrentHandle {
            handle,
            callback: Mutex::new(None),
        }))
    }
    pub fn reap(&self) -> Result<(), TorrentError> {
        runtime()?
            .block_on(self.manager.reap())
            .map_err(|_| TorrentError::OperationFailed)
    }
}
#[uniffi::export]
impl TorrentHandle {
    pub fn files(&self) -> Vec<TorrentFile> {
        self.handle
            .files()
            .iter()
            .map(|f| TorrentFile {
                index: f.index,
                name: f.name.clone(),
                size: f.size,
            })
            .collect()
    }
    /// Internally generated capability token; callers cannot weaken the token.
    pub fn stream_url(&self, index: u32) -> Result<String, TorrentError> {
        self.handle
            .stream_url(index)
            .map_err(|_| TorrentError::Stopped)
    }
    pub fn read_range(
        &self,
        index: u32,
        offset: u64,
        length: u32,
    ) -> Result<Vec<u8>, TorrentError> {
        runtime()?
            .block_on(self.handle.read_range(index, offset, length))
            .map_err(|_| TorrentError::OperationFailed)
    }
    pub fn status(&self) -> TorrentProgress {
        let status = self.handle.status();
        TorrentProgress {
            downloaded_bytes: status.downloaded_bytes,
            total_bytes: status.total_bytes,
            finished: status.finished,
            stopped: status.stopped,
        }
    }
    /// Replaces any previous listener. Callbacks run on a native blocking
    /// worker so foreign code may safely call synchronous methods again.
    pub fn set_progress_listener(
        self: Arc<Self>,
        listener: Box<dyn TorrentProgressListener>,
    ) -> Result<(), TorrentError> {
        let mut slot = self
            .callback
            .lock()
            .map_err(|_| TorrentError::OperationFailed)?;
        if let Some(task) = slot.take() {
            task.abort();
        }
        let weak = Arc::downgrade(&self);
        let listener: Arc<dyn TorrentProgressListener> = listener.into();
        *slot = Some(runtime()?.spawn(async move {
            loop {
                let Some(handle) = weak.upgrade() else { break };
                let status = handle.status();
                let stopped = status.stopped;
                drop(handle);
                let listener = listener.clone();
                let _ = tokio::task::spawn_blocking(move || listener.on_status(status)).await;
                if stopped {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }));
        Ok(())
    }
    pub fn stop(&self) {
        self.handle.stop();
        if let Ok(mut callback) = self.callback.lock() {
            if let Some(task) = callback.take() {
                task.abort();
            }
        }
    }
}
impl Drop for TorrentHandle {
    fn drop(&mut self) {
        if let Ok(callback) = self.callback.get_mut() {
            if let Some(task) = callback.take() {
                task.abort();
            }
        }
    }
}
