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
    #[error("Strict handles require status polling")]
    CallbacksUnsupported,
    #[error("Native startup deadline reached")]
    StartupTimeout,
    #[error("Torrent network session deadline reached")]
    SessionTimeout,
    #[error("Torrent metadata deadline reached")]
    MetadataTimeout,
    #[error("Torrent cache preparation deadline reached")]
    CachePreparationTimeout,
    #[error("Torrent initialization deadline reached")]
    InitializationTimeout,
    #[error("Local stream preparation deadline reached")]
    LoopbackTimeout,
    #[error("Torrent network session unavailable")]
    SessionUnavailable,
    #[error("Torrent initialization failed")]
    InitializationFailed,
    #[error("Local stream unavailable")]
    LoopbackUnavailable,
    #[error("Previous torrent work is retiring")]
    RetirementPending,
    #[error("Torrent exceeds payload budget")]
    PayloadLimit,
    #[error("Torrent storage unavailable")]
    StorageUnavailable,
    #[error("Torrent cache unavailable")]
    CacheUnavailable,
    #[error("Native metadata invalid")]
    MetadataInvalid,
    #[error("Selected torrent file unavailable")]
    FileUnavailable,
}
fn native_failure(reason: playback_engine::torrent::AcquisitionFailure) -> TorrentError {
    use playback_engine::torrent::AcquisitionFailure as F;
    match reason {
        F::StartupTimeout => TorrentError::StartupTimeout,
        F::SessionTimeout => TorrentError::SessionTimeout,
        F::MetadataTimeout => TorrentError::MetadataTimeout,
        F::CachePreparationTimeout => TorrentError::CachePreparationTimeout,
        F::InitializationTimeout => TorrentError::InitializationTimeout,
        F::LoopbackTimeout => TorrentError::LoopbackTimeout,
        F::SessionUnavailable => TorrentError::SessionUnavailable,
        F::InitializationFailed => TorrentError::InitializationFailed,
        F::LoopbackUnavailable => TorrentError::LoopbackUnavailable,
        F::RetirementPending => TorrentError::RetirementPending,
        F::PayloadLimit => TorrentError::PayloadLimit,
        F::StorageUnavailable => TorrentError::StorageUnavailable,
        F::CacheUnavailable => TorrentError::CacheUnavailable,
        F::MetadataInvalid => TorrentError::MetadataInvalid,
        F::FileUnavailable => TorrentError::FileUnavailable,
        F::OperationFailed => TorrentError::OperationFailed,
    }
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
#[derive(Clone, Debug, uniffi::Record)]
pub struct TorrentTransportDiagnostics {
    pub active: bool,
    pub failed: bool,
    pub fetched_bytes: u64,
    pub checked_bytes: u64,
    pub live_peers: u64,
    pub connecting_peers: u64,
    pub queued_peers: u64,
    pub dead_peers: u64,
    pub not_needed_peers: u64,
    pub current_readers: u64,
    pub blocked_pieces: u64,
    pub unreserved_pieces: u64,
    pub received_current_chunks: u64,
    pub total_current_chunks: u64,
    pub current_owners_live: u64,
    pub max_current_age_ms: u64,
    pub steals: u64,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum TorrentSettlement {
    Settled,
    SettlementFailed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum TorrentAcquisitionStatus {
    Preparing,
    Ready,
    Cancelled,
    Failed,
    SettlementFailed,
}
#[derive(uniffi::Object)]
pub struct TorrentAcquisition {
    acquisition: playback_engine::TorrentAcquisition,
}
fn settlement(value: playback_engine::Settlement) -> TorrentSettlement {
    match value {
        playback_engine::Settlement::Settled => TorrentSettlement::Settled,
        playback_engine::Settlement::SettlementFailed => TorrentSettlement::SettlementFailed,
    }
}
#[uniffi::export]
impl TorrentAcquisition {
    /// Blocks only until the captured startup deadline; call on an IO dispatcher.
    pub fn wait_ready(&self) -> Result<Arc<TorrentHandle>, TorrentError> {
        let handle = runtime()?
            .block_on(self.acquisition.wait_ready())
            .map_err(|error| {
                native_failure(self.acquisition.failure_reason().unwrap_or_else(|| {
                    playback_engine::torrent::AcquisitionFailure::from_safe_error(&error)
                }))
            })?;
        Ok(Arc::new(TorrentHandle {
            handle,
            callback: Mutex::new(None),
        }))
    }
    pub fn status(&self) -> TorrentAcquisitionStatus {
        use playback_engine::AcquisitionStatus as S;
        match self.acquisition.status() {
            S::Preparing => TorrentAcquisitionStatus::Preparing,
            S::Ready => TorrentAcquisitionStatus::Ready,
            S::Cancelled => TorrentAcquisitionStatus::Cancelled,
            S::Failed => TorrentAcquisitionStatus::Failed,
            S::SettlementFailed => TorrentAcquisitionStatus::SettlementFailed,
        }
    }
    pub fn cancel(&self) {
        self.acquisition.cancel();
    }
    pub fn cancel_and_wait(&self) -> Result<TorrentSettlement, TorrentError> {
        Ok(settlement(
            runtime()?.block_on(self.acquisition.cancel_and_wait()),
        ))
    }
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
    /// Strict production construction fixes the public discovery policy.
    #[uniffi::constructor]
    pub fn new_native_public(
        cache_dir: String,
        max_cache_bytes: u64,
    ) -> Result<Arc<Self>, TorrentError> {
        if cache_dir.is_empty() {
            return Err(TorrentError::InvalidOptions);
        }
        runtime()?;
        let manager = TorrentManager::new_native_public(TorrentConfig {
            enabled: true,
            cache_dir: PathBuf::from(cache_dir),
            max_bytes: max_cache_bytes,
            piece_cache_bytes: Some(256 * 1024 * 1024),
            ..Default::default()
        })
        .map_err(|_| TorrentError::InvalidOptions)?;
        Ok(Arc::new(Self { manager }))
    }
    /// Return owned cancellation before IO. The caller supplies its remaining
    /// monotonic startup budget, bounded to a total of thirty seconds.
    pub fn begin_selected(
        &self,
        magnet: String,
        info_hash: String,
        file_index: u32,
        verified_size: Option<u64>,
        remaining_budget_millis: u32,
    ) -> Result<Arc<TorrentAcquisition>, TorrentError> {
        let _enter = runtime()?.enter();
        let acquisition = self
            .manager
            .begin_native_selected_magnet(
                "native".into(),
                magnet,
                info_hash,
                file_index,
                verified_size,
                Duration::from_millis(remaining_budget_millis.into()),
            )
            .map_err(|error| {
                native_failure(
                    playback_engine::torrent::AcquisitionFailure::from_safe_error(&error),
                )
            })?;
        Ok(Arc::new(TorrentAcquisition { acquisition }))
    }
    pub fn begin_selected_metainfo(
        &self,
        metainfo: Vec<u8>,
        info_hash: String,
        file_index: u32,
        verified_size: Option<u64>,
        remaining_budget_millis: u32,
    ) -> Result<Arc<TorrentAcquisition>, TorrentError> {
        let _enter = runtime()?.enter();
        let acquisition = self
            .manager
            .begin_native_selected_metainfo(
                "native".into(),
                metainfo,
                info_hash,
                file_index,
                verified_size,
                Duration::from_millis(remaining_budget_millis.into()),
            )
            .map_err(|error| {
                native_failure(
                    playback_engine::torrent::AcquisitionFailure::from_safe_error(&error),
                )
            })?;
        Ok(Arc::new(TorrentAcquisition { acquisition }))
    }
    pub fn has_failed_settlement(&self) -> bool {
        self.manager.has_failed_settlement()
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
    /// Each call owns an independent lease restricted to this exact file.
    /// This does not provide cancellation during acquisition or joined stop.
    pub fn start_selected(
        &self,
        magnet: String,
        file_index: u32,
    ) -> Result<Arc<TorrentHandle>, TorrentError> {
        let handle = runtime()?
            .block_on(
                self.manager
                    .start_selected_magnet("native", &magnet, file_index),
            )
            .map_err(|_| TorrentError::OperationFailed)?;
        Ok(Arc::new(TorrentHandle {
            handle,
            callback: Mutex::new(None),
        }))
    }
    /// Caller-vetted metainfo; an explicit file index is mandatory.
    /// Repeated starts share cached payload, not stop state or byte tokens.
    pub fn start_selected_metainfo(
        &self,
        metainfo: Vec<u8>,
        file_index: u32,
    ) -> Result<Arc<TorrentHandle>, TorrentError> {
        let handle = runtime()?
            .block_on(
                self.manager
                    .start_selected_metainfo("native", metainfo, file_index),
            )
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
#[cfg(feature = "test-network-policy")]
#[uniffi::export]
impl TorrentClient {
    /// Separately compiled test artifacts only; absent from Android release bindings.
    #[uniffi::constructor]
    pub fn new_native_owned(
        cache_dir: String,
        max_cache_bytes: u64,
        owned_peers: Vec<String>,
    ) -> Result<Arc<Self>, TorrentError> {
        runtime()?;
        let peers = owned_peers
            .iter()
            .map(|p| p.parse())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| TorrentError::InvalidOptions)?;
        let manager = TorrentManager::new_native_owned(
            TorrentConfig {
                enabled: true,
                cache_dir: PathBuf::from(cache_dir),
                max_bytes: max_cache_bytes,
                piece_cache_bytes: Some(256 * 1024 * 1024),
                enable_dht: false,
                ..Default::default()
            },
            peers,
        )
        .map_err(|_| TorrentError::InvalidOptions)?;
        Ok(Arc::new(Self { manager }))
    }
}
#[uniffi::export]
impl TorrentHandle {
    pub fn metadata_file_count(&self) -> u32 {
        self.handle.metadata_file_count()
    }
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
    pub fn transport_diagnostics(&self) -> TorrentTransportDiagnostics {
        let snapshot = self.handle.diagnostics();
        TorrentTransportDiagnostics {
            active: snapshot.active,
            failed: snapshot.failed,
            fetched_bytes: snapshot.fetched_bytes,
            checked_bytes: snapshot.checked_bytes,
            live_peers: snapshot.live_peers,
            connecting_peers: snapshot.connecting_peers,
            queued_peers: snapshot.queued_peers,
            dead_peers: snapshot.dead_peers,
            not_needed_peers: snapshot.not_needed_peers,
            current_readers: snapshot.current_readers,
            blocked_pieces: snapshot.blocked_pieces,
            unreserved_pieces: snapshot.unreserved_pieces,
            received_current_chunks: snapshot.received_current_chunks,
            total_current_chunks: snapshot.total_current_chunks,
            current_owners_live: snapshot.current_owners_live,
            max_current_age_ms: snapshot.max_current_age_ms,
            steals: snapshot.steals,
        }
    }
    /// Replaces any previous listener. Callbacks run on a native blocking
    /// worker so foreign code may safely call synchronous methods again.
    pub fn set_progress_listener(
        self: Arc<Self>,
        listener: Box<dyn TorrentProgressListener>,
    ) -> Result<(), TorrentError> {
        if self.handle.is_strict() {
            return Err(TorrentError::CallbacksUnsupported);
        }
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
    /// Only strict handles can prove completion without foreign callbacks.
    pub fn stop_and_wait(&self) -> Result<TorrentSettlement, TorrentError> {
        if !self.handle.is_strict() {
            self.stop();
            return Ok(TorrentSettlement::SettlementFailed);
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        Ok(settlement(
            runtime()?.block_on(self.handle.stop_and_settle_until(deadline)),
        ))
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
