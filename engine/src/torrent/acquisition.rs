//! Selected acquisitions own cancellation before any IO is queued. Pending
//! payloads own dependency receipts and reservations independently of waiters.
use super::*;
use librqbit::{OwnedTasks, Settlement};
use tokio::time::{timeout_at, Instant as Deadline};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcquisitionStatus {
    Preparing,
    Ready,
    Cancelled,
    Failed,
    SettlementFailed,
}
/// Closed observable facts; dependency diagnostics and source data never cross this boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcquisitionFailure {
    StartupTimeout,
    SessionTimeout,
    MetadataTimeout,
    CachePreparationTimeout,
    InitializationTimeout,
    LoopbackTimeout,
    SessionUnavailable,
    InitializationFailed,
    LoopbackUnavailable,
    RetirementPending,
    PayloadLimit,
    StorageUnavailable,
    CacheUnavailable,
    MetadataInvalid,
    FileUnavailable,
    OperationFailed,
}
impl AcquisitionFailure {
    pub fn from_safe_error(error: &str) -> Self {
        match error {
            "Native startup timed out" => Self::StartupTimeout,
            "Torrent session unavailable" => Self::SessionUnavailable,
            "Torrent start failed" | "Torrent initialization failed" => Self::InitializationFailed,
            "Torrent stream unavailable" => Self::LoopbackUnavailable,
            "Native acquisition retiring" | "Native payload retiring" => Self::RetirementPending,
            "Torrent exceeds cache budget" => Self::PayloadLimit,
            "Torrent disk capacity reached" | "Torrent disk capacity unavailable" => {
                Self::StorageUnavailable
            }
            "Torrent settlement failed"
            | "Torrent cache unavailable"
            | "Torrent cache lock unavailable"
            | "Torrent cache already in use"
            | "Torrent cache cleanup failed"
            | "Torrent cache permissions unavailable" => Self::CacheUnavailable,
            "Invalid native metadata" | "Invalid native metainfo" => Self::MetadataInvalid,
            "Native exact selection denied" => Self::FileUnavailable,
            _ => Self::OperationFailed,
        }
    }
}
pub struct TorrentAcquisition {
    state: Arc<AcquisitionState>,
}
struct AcquisitionState {
    manager: TorrentManager,
    deadline: Deadline,
    tasks: OwnedTasks,
    control: std::sync::Mutex<AcquisitionControl>,
    changed: Notify,
}
struct AcquisitionControl {
    status: AcquisitionStatus,
    lease: Option<Arc<PendingLease>>,
    handle: Option<TorrentHandle>,
    io_tasks: Option<OwnedTasks>,
    failure: Option<AcquisitionFailure>,
    phase: AcquisitionPhase,
}
#[derive(Clone, Copy)]
enum AcquisitionPhase {
    CachePreparation,
    Session,
    Metadata,
    Initialization,
    Loopback,
}
impl AcquisitionPhase {
    fn timeout(self) -> AcquisitionFailure {
        match self {
            Self::CachePreparation => AcquisitionFailure::CachePreparationTimeout,
            Self::Metadata => AcquisitionFailure::MetadataTimeout,
            Self::Initialization => AcquisitionFailure::InitializationTimeout,
            Self::Loopback => AcquisitionFailure::LoopbackTimeout,
            Self::Session => AcquisitionFailure::SessionTimeout,
        }
    }
}
type PendingKey = (String, String);
pub(super) struct PendingPayload {
    selection: NativeSelection,
    tasks: OwnedTasks,
    consumers: AtomicUsize,
    admission: std::sync::Mutex<()>,
    result: std::sync::Mutex<Option<Result<Arc<Entry>, String>>>,
    changed: Notify,
    folder: std::sync::Mutex<Option<PathBuf>>,
    reserved: std::sync::atomic::AtomicU64,
    // Reaping claims this fence under the registry lock before deleting files.
    reaping: AtomicBool,
    phase: std::sync::Mutex<AcquisitionPhase>,
}
pub(super) struct PendingLease {
    pending: Arc<PendingPayload>,
    held: AtomicBool,
}
impl PendingLease {
    pub(super) fn release(&self) {
        let _admission = self
            .pending
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.held.swap(false, Ordering::AcqRel)
            && self.pending.consumers.fetch_sub(1, Ordering::AcqRel) == 1
        {
            self.pending.tasks.cancel();
            self.pending.changed.notify_waiters();
        }
    }
    pub(super) async fn settle_until(&self, deadline: Deadline) -> bool {
        self.release();
        if self.pending.consumers.load(Ordering::Acquire) != 0 {
            return true;
        }
        let joined = self.pending.tasks.cancel_and_wait_until(deadline).await;
        joined && !self.pending.tasks.has_failed_settlement()
    }
}
impl Drop for PendingLease {
    fn drop(&mut self) {
        self.release();
    }
}
impl std::fmt::Debug for TorrentAcquisition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TorrentAcquisition(<redacted>)")
    }
}
impl TorrentManager {
    /// Capture the caller's remaining monotonic budget before validation,
    /// queueing or registry waits. No later stage or renewal resets it.
    pub fn begin_native_selected_magnet(
        &self,
        scope: String,
        source: String,
        expected_hash: String,
        index: u32,
        verified_size: impl Into<Option<u64>>,
        budget: Duration,
    ) -> Result<TorrentAcquisition, String> {
        self.begin_native(
            scope,
            NativeInput::Magnet(source),
            expected_hash,
            index,
            verified_size.into(),
            budget,
        )
    }
    pub fn begin_native_selected_metainfo(
        &self,
        scope: String,
        bytes: Vec<u8>,
        expected_hash: String,
        index: u32,
        verified_size: impl Into<Option<u64>>,
        budget: Duration,
    ) -> Result<TorrentAcquisition, String> {
        self.begin_native(
            scope,
            NativeInput::Metainfo(bytes),
            expected_hash,
            index,
            verified_size.into(),
            budget,
        )
    }
    fn begin_native(
        &self,
        scope: String,
        input: NativeInput,
        expected_hash: String,
        index: u32,
        verified_size: Option<u64>,
        budget: Duration,
    ) -> Result<TorrentAcquisition, String> {
        let deadline = Deadline::now() + budget.min(READ_TIMEOUT);
        if budget.is_zero()
            || !self.inner.config.enabled
            || !self.inner.network_policy.is_native()
            || scope.is_empty()
            || scope.len() > 256
            || self.inner.settlement_failed.load(Ordering::Acquire)
        {
            return Err("Native acquisition unavailable".into());
        }
        let selection = NativeSelection::from_hex(&expected_hash, index, verified_size)
            .map_err(|_| "Native exact selection denied")?;
        let input = match input {
            NativeInput::Magnet(source) => {
                if source != format!("magnet:?xt=urn:btih:{expected_hash}") {
                    return Err("Native source input denied".into());
                }
                NativeInput::Magnet(source)
            }
            NativeInput::Metainfo(bytes) => NativeInput::Metainfo(
                vet_native_metainfo(&bytes, Some(&expected_hash), index, verified_size)?
                    .canonical_bytes()
                    .to_vec(),
            ),
        };
        let _ = self.inner.runtime.set(tokio::runtime::Handle::current());
        let state = Arc::new(AcquisitionState {
            manager: self.clone(),
            deadline,
            tasks: OwnedTasks::new(Default::default()),
            control: std::sync::Mutex::new(AcquisitionControl {
                status: AcquisitionStatus::Preparing,
                lease: None,
                handle: None,
                io_tasks: None,
                failure: None,
                phase: AcquisitionPhase::CachePreparation,
            }),
            changed: Notify::new(),
        });
        {
            let mut owners = self
                .inner
                .acquisition_owners
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if owners.iter().any(|o| o.has_failed_settlement()) {
                self.inner.settlement_failed.store(true, Ordering::Release);
                return Err("Torrent settlement failed".into());
            }
            if owners
                .iter()
                .any(|o| o.cancellation_token().is_cancelled() && o.has_pending_work())
            {
                return Err("Native acquisition retiring".into());
            }
            owners.retain(|o| !o.cancellation_token().is_cancelled() || o.has_pending_work());
            if owners.len() >= 32 {
                return Err("Native acquisition capacity reached".into());
            }
            owners.push(state.tasks.clone());
        }
        let worker = state.clone();
        state
            .tasks
            .spawn(tracing::Span::none(), async move {
                let result = timeout_at(deadline, worker.prepare(scope, input, selection)).await;
                match result {
                    Ok(Ok(handle)) => {
                        let mut control = worker.control.lock().unwrap_or_else(|e| e.into_inner());
                        if worker.tasks.cancellation_token().is_cancelled()
                            || Deadline::now() >= deadline
                        {
                            if Deadline::now() >= deadline {
                                control
                                    .failure
                                    .get_or_insert(AcquisitionFailure::LoopbackTimeout);
                            }
                            handle.stop();
                            control.status = AcquisitionStatus::Cancelled;
                        } else {
                            control.handle = Some(handle);
                            control.status = AcquisitionStatus::Ready;
                        }
                    }
                    failed => {
                        let failure = match failed {
                            Err(_) => worker.timeout_failure(),
                            Ok(Err(error)) => AcquisitionFailure::from_safe_error(&error),
                            Ok(Ok(_)) => unreachable!(),
                        };
                        // Leave ownership in the acquisition until joined cleanup.
                        let mut control = worker.control.lock().unwrap_or_else(|e| e.into_inner());
                        if control.status == AcquisitionStatus::Preparing {
                            control.failure.get_or_insert(failure);
                            control.status = AcquisitionStatus::Failed;
                        }
                        if let Some(lease) = &control.lease {
                            lease.release();
                        }
                    }
                }
                worker.changed.notify_waiters();
                Ok(())
            })
            .ok_or("Native acquisition unavailable")?;
        Ok(TorrentAcquisition { state })
    }
    /// Only a closed, joined owner with no sticky failure can release storage.
    pub(super) async fn reap_native(&self) -> Result<(), String> {
        if !self.inner.network_policy.is_native() {
            return Ok(());
        }
        let tasks = OwnedTasks::new(Default::default());
        let _cancel_on_drop = CancelOwner(tasks.clone());
        {
            let mut owners = self
                .inner
                .acquisition_owners
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if owners
                .iter()
                .any(|o| o.cancellation_token().is_cancelled() && o.has_pending_work())
            {
                return Err("Native cleanup retiring".into());
            }
            owners.retain(|o| {
                o.has_failed_settlement()
                    || !o.cancellation_token().is_cancelled()
                    || o.has_pending_work()
            });
            owners.push(tasks.clone());
        }
        let result = self.reap_native_owned(&tasks).await;
        if !tasks
            .cancel_and_wait_until(Deadline::now() + Duration::from_secs(2))
            .await
        {
            self.inner.settlement_failed.store(true, Ordering::Release);
            return Err("Torrent settlement failed".into());
        }
        result
    }
    async fn reap_native_owned(&self, tasks: &OwnedTasks) -> Result<(), String> {
        let claimed = {
            let state = self.inner.state.lock().await;
            let mut claimed = Vec::new();
            for (key, pending) in &state.pending {
                if pending.tasks.has_failed_settlement() {
                    self.inner.settlement_failed.store(true, Ordering::Release);
                }
                if pending.consumers.load(Ordering::Acquire) == 0
                    && pending.tasks.cancellation_token().is_cancelled()
                    && !pending.tasks.has_pending_work()
                    && !pending.tasks.has_failed_settlement()
                    && pending
                        .result
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                        .is_none_or(|r| {
                            r.as_ref()
                                .map_or(true, |entry| Arc::strong_count(entry) == 1)
                        })
                    && !pending.reaping.swap(true, Ordering::AcqRel)
                {
                    claimed.push((key.clone(), ReapingClaim(pending.clone())));
                }
            }
            claimed
        };
        for (key, _claim) in claimed {
            let pending = _claim.0.clone();
            let folder = pending
                .folder
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(folder) = folder {
                let retained_manager = self.inner.clone();
                owned_fs(tasks, move || {
                    let _retained_manager = retained_manager;
                    if folder.exists() {
                        std::fs::remove_dir_all(folder)
                            .map_err(|_| "Torrent eviction failed".to_owned())?;
                    }
                    Ok(())
                })
                .await?;
            }
            // Dispose managed storage and the joined session before releasing
            // its physical allocation from the registry.
            let result = pending
                .result
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            drop(result);
            let mut state = self.inner.state.lock().await;
            state.reserved -= pending.reserved.load(Ordering::Acquire);
            state.pending.remove(&key);
        }
        Ok(())
    }
}
struct CancelOwner(OwnedTasks);
impl Drop for CancelOwner {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
struct ReapingClaim(Arc<PendingPayload>);
impl Drop for ReapingClaim {
    fn drop(&mut self) {
        self.0.reaping.store(false, Ordering::Release);
    }
}
struct BlockingJob(tokio::task::JoinHandle<()>);
impl Drop for BlockingJob {
    fn drop(&mut self) {
        // Tokio can remove an unstarted blocking job. A started syscall still
        // retains its OwnedWork receipt until the closure actually returns.
        self.0.abort();
    }
}
async fn owned_fs<T: Send + Sync + 'static>(
    tasks: &OwnedTasks,
    operation: impl FnOnce() -> Result<T, String> + Send + Sync + 'static,
) -> Result<T, String> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let cancellation = tasks.cancellation_token().clone();
    let call = tasks
        .wrap_blocking(move || {
            let result = if cancellation.is_cancelled() {
                Err("Native acquisition cancelled".into())
            } else {
                operation()
            };
            let _ = sender.send(result);
        })
        .ok_or("Native acquisition cancelled")?;
    let _job = BlockingJob(tokio::task::spawn_blocking(call));
    receiver
        .await
        .map_err(|_| "Native acquisition cancelled".to_owned())?
}
enum NativeInput {
    Magnet(String),
    Metainfo(Vec<u8>),
}
impl AcquisitionState {
    fn timeout_failure(&self) -> AcquisitionFailure {
        let control = self.control.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(lease) = &control.lease {
            if let Some(Err(error)) = lease
                .pending
                .result
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
                return AcquisitionFailure::from_safe_error(error);
            }
        }
        let phase = if matches!(control.phase, AcquisitionPhase::Loopback) {
            control.phase
        } else {
            control
                .lease
                .as_ref()
                .map(|lease| {
                    *lease
                        .pending
                        .phase
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                })
                .unwrap_or(control.phase)
        };
        phase.timeout()
    }
    async fn prepare(
        self: &Arc<Self>,
        scope: String,
        input: NativeInput,
        selection: NativeSelection,
    ) -> Result<TorrentHandle, String> {
        if Deadline::now() >= self.deadline {
            return Err("Native startup timed out".into());
        }
        self.manager.reap_native_owned(&self.tasks).await?;
        self.manager
            .inner
            .cache_lock
            .get_or_try_init(|| async {
                let manager = self.manager.inner.clone();
                owned_fs(&self.tasks, move || {
                    prepare_cache(&manager.config.cache_dir).map(Arc::new)
                })
                .await
            })
            .await?;
        let key: PendingKey = (
            format!("{:x}", Sha256::digest(scope.as_bytes())),
            format!("{}:{}", hex_hash(selection.info_hash), selection.file_index),
        );
        let pending =
            {
                let mut registry = self.manager.inner.state.lock().await;
                if registry.pending.values().any(|p| {
                    p.tasks.cancellation_token().is_cancelled() && p.tasks.has_pending_work()
                }) {
                    return Err("Native payload retiring".into());
                }
                if self.manager.inner.settlement_failed.load(Ordering::Acquire) {
                    return Err("Torrent settlement failed".into());
                }
                let pending = match registry.pending.get(&key) {
                    Some(pending)
                        if pending.selection.file_index == selection.file_index
                            && !pending
                                .selection
                                .verified_size
                                .zip(selection.verified_size)
                                .is_some_and(|(existing, requested)| existing != requested)
                            && !pending.tasks.cancellation_token().is_cancelled()
                            && !pending.reaping.load(Ordering::Acquire) =>
                    {
                        pending.clone()
                    }
                    Some(_) => return Err("Native payload unavailable".into()),
                    None => {
                        // Metadata is capped at 4 MiB per job; bound the pending
                        // control working set independently of full payload charge.
                        if registry.pending.len() >= 16 {
                            return Err("Native pending capacity reached".into());
                        }
                        let pending = Arc::new(PendingPayload {
                            selection,
                            tasks: OwnedTasks::new(Default::default()),
                            consumers: AtomicUsize::new(0),
                            admission: std::sync::Mutex::new(()),
                            result: std::sync::Mutex::new(None),
                            changed: Notify::new(),
                            folder: std::sync::Mutex::new(None),
                            reserved: std::sync::atomic::AtomicU64::new(0),
                            reaping: AtomicBool::new(false),
                            phase: std::sync::Mutex::new(AcquisitionPhase::Session),
                        });
                        registry.pending.insert(key, pending.clone());
                        let producer = pending.clone();
                        let manager = self.manager.clone();
                        pending
                            .tasks
                            .spawn(tracing::Span::none(), async move {
                                let result = producer.produce(&manager, input).await;
                                *producer.result.lock().unwrap_or_else(|e| e.into_inner()) =
                                    Some(result);
                                producer.changed.notify_waiters();
                                Ok(())
                            })
                            .ok_or("Native payload unavailable")?;
                        pending
                    }
                };
                // Registration and cancellation of the final consumer use this
                // mutex as the publication fence, with no IO inside the transaction.
                let mut control = self.control.lock().unwrap_or_else(|e| e.into_inner());
                if self.tasks.cancellation_token().is_cancelled() {
                    if pending.consumers.load(Ordering::Acquire) == 0 {
                        pending.tasks.cancel();
                    }
                    return Err("Native acquisition cancelled".into());
                }
                let admission = pending.admission.lock().unwrap_or_else(|e| e.into_inner());
                if pending.tasks.cancellation_token().is_cancelled() {
                    return Err("Native payload cancelled".into());
                }
                pending.consumers.fetch_add(1, Ordering::AcqRel);
                let lease = Arc::new(PendingLease {
                    pending: pending.clone(),
                    held: AtomicBool::new(true),
                });
                control.lease = Some(lease);
                drop(admission);
                pending
            };
        let entry = loop {
            let changed = pending.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(result) = pending
                .result
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                break result?;
            }
            tokio::select! {
                _ = changed => {},
                _ = pending.tasks.cancellation_token().cancelled() => return Err("Native payload cancelled".into()),
            }
        };
        let selected_size = entry
            .files
            .get(selection.file_index as usize)
            .map(|file| file.size)
            .filter(|size| *size > 0)
            .ok_or("Native exact selection denied")?;
        if selection
            .verified_size
            .is_some_and(|size| size != selected_size)
        {
            return Err("Native exact selection denied".into());
        }
        let lease = self
            .control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .lease
            .clone()
            .ok_or("Native acquisition cancelled")?;
        let io_tasks = lease.pending.tasks.child();
        {
            let mut control = self.control.lock().unwrap_or_else(|e| e.into_inner());
            if self.tasks.cancellation_token().is_cancelled() {
                return Err("Native acquisition cancelled".into());
            }
            control.io_tasks = Some(io_tasks.clone());
            control.phase = AcquisitionPhase::Loopback;
        }
        self.manager
            .make_native_handle(entry, self.selection_index(&lease), lease, io_tasks)
            .await
            .map_err(|_| "Torrent stream unavailable".to_owned())
    }
    fn selection_index(&self, lease: &PendingLease) -> u32 {
        lease.pending.selection.file_index
    }
}
impl PendingPayload {
    async fn produce(
        self: &Arc<Self>,
        manager: &TorrentManager,
        input: NativeInput,
    ) -> Result<Arc<Entry>, String> {
        let session = Session::new_with_opts(
            manager.inner.config.cache_dir.clone(),
            SessionOptions {
                task_owner: Some(self.tasks.clone()),
                network_policy: manager.inner.network_policy.clone(),
                disable_trackers: true,
                disable_dht: !manager.inner.config.enable_dht,
                disable_dht_persistence: true,
                persistence: None,
                listen_port_range: None,
                enable_upnp_port_forwarding: false,
                concurrent_init_limit: Some(2),
                peer_opts: Some(librqbit::PeerConnectionOptions {
                    connect_timeout: Some(Duration::from_secs(2)),
                    read_write_timeout: Some(Duration::from_secs(5)),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "Torrent session unavailable")?;
        *self.phase.lock().unwrap_or_else(|e| e.into_inner()) = AcquisitionPhase::Metadata;
        let input = match input {
            NativeInput::Magnet(s) => AddTorrent::from_url(s),
            NativeInput::Metainfo(b) => AddTorrent::from_bytes(b),
        };
        let listed = session
            .add_torrent(
                input,
                Some(AddTorrentOptions {
                    list_only: true,
                    native_selection: Some(self.selection),
                    only_files: Some(vec![self.selection.file_index as usize]),
                    disable_trackers: true,
                    initial_peers: Some(manager.inner.config.initial_peers.clone()),
                    ..Default::default()
                }),
            )
            .await
            .map_err(|_| "Invalid native metadata")?;
        let AddTorrentResponse::ListOnly(listed) = listed else {
            return Err("Native payload unavailable".into());
        };
        *self.phase.lock().unwrap_or_else(|e| e.into_inner()) = AcquisitionPhase::CachePreparation;
        let files = describe_files(&listed.info)?;
        let size = files
            .iter()
            .try_fold(0u64, |n, f| n.checked_add(f.size))
            .ok_or("Torrent size overflow")?;
        let rolling = manager
            .inner
            .config
            .piece_cache_bytes
            .filter(|budget| size > *budget);
        if let Some(budget) = rolling {
            if u64::from(listed.info.piece_length).saturating_mul(2) > budget {
                return Err("Torrent exceeds cache budget".into());
            }
        }
        let size = rolling.unwrap_or(size);
        let disk_manager = manager.inner.clone();
        let available = owned_fs(&self.tasks, move || {
            native_available_disk_bytes(&disk_manager.config.cache_dir)
        })
        .await?;
        #[cfg(all(test, feature = "test-network-policy"))]
        let available = available.min(manager.inner.native_available_bytes.load(Ordering::Acquire));
        {
            let mut registry = manager.inner.state.lock().await;
            if size > manager.inner.config.max_bytes
                || registry.reserved.saturating_add(size) > manager.inner.config.max_bytes
            {
                return Err("Torrent exceeds cache budget".into());
            }
            // Reservations include each input's bounded slots or small full
            // payload and retiring work. Charging materialized bytes again is
            // conservative: filesystem free space alone cannot prove room for
            // every previously admitted writer's remaining allocation.
            if registry
                .reserved
                .checked_add(size)
                .is_none_or(|sum| sum > available)
            {
                return Err("Torrent disk capacity reached".into());
            }
            if self.tasks.cancellation_token().is_cancelled() {
                return Err("Native payload cancelled".into());
            }
            registry.reserved += size;
            self.reserved.store(size, Ordering::Release);
        }
        let folder = manager
            .inner
            .config
            .cache_dir
            .join(format!("torrent-{}", Uuid::new_v4().simple()));
        *self.folder.lock().unwrap_or_else(|e| e.into_inner()) = Some(folder.clone());
        let prepare_folder = folder.clone();
        let retained_manager = manager.inner.clone();
        owned_fs(&self.tasks, move || {
            let _retained_manager = retained_manager;
            std::fs::create_dir(&prepare_folder)
                .map_err(|_| "Torrent cache unavailable".to_owned())?;
            private_dir(&prepare_folder)
        })
        .await?;
        let storage_factory = rolling.map(|capacity| {
            use librqbit::storage::StorageFactoryExt;
            librqbit::storage::rolling::RollingStorageFactory { capacity }.boxed()
        });
        #[cfg(all(test, feature = "test-network-policy"))]
        let storage_factory = manager
            .inner
            .native_storage_factory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|f| f.clone_box())
            .or(storage_factory);
        *self.phase.lock().unwrap_or_else(|e| e.into_inner()) = AcquisitionPhase::Initialization;
        let response = session
            .add_torrent(
                AddTorrent::from_bytes(listed.torrent_bytes.to_vec()),
                Some(AddTorrentOptions {
                    output_folder: Some(
                        folder
                            .to_str()
                            .ok_or("Torrent cache path unsupported")?
                            .to_owned(),
                    ),
                    disable_trackers: true,
                    native_selection: Some(self.selection),
                    only_files: Some(vec![self.selection.file_index as usize]),
                    initial_peers: Some(manager.inner.config.initial_peers.clone()),
                    overwrite: false,
                    storage_factory,
                    ..Default::default()
                }),
            )
            .await
            .map_err(|_| "Torrent start failed")?;
        let managed = response.into_handle().ok_or("Torrent start failed")?;
        managed
            .wait_until_initialized()
            .await
            .map_err(|_| "Torrent initialization failed")?;
        Ok(Arc::new(Entry {
            _cache_lock: manager
                .inner
                .cache_lock
                .get()
                .expect("cache initialized")
                .clone(),
            session,
            managed,
            files,
            folder,
            size,
            users: AtomicUsize::new(0),
            readers: AtomicUsize::new(0),
            idle_since: std::sync::Mutex::new(Instant::now()),
            handles: std::sync::Mutex::new(HashMap::new()),
        }))
    }
}

#[cfg(unix)]
fn native_available_disk_bytes(path: &std::path::Path) -> Result<u64, String> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| "Torrent disk capacity unavailable")?;
    let mut facts = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // statvfs writes the complete output structure on success; the private
    // cache path is a nul-terminated CString retained for the syscall.
    if unsafe { libc::statvfs(path.as_ptr(), facts.as_mut_ptr()) } != 0 {
        return Err("Torrent disk capacity unavailable".into());
    }
    let facts = unsafe { facts.assume_init() };
    (facts.f_bavail as u64)
        .checked_mul(facts.f_frsize as u64)
        .ok_or_else(|| "Torrent disk capacity unavailable".into())
}

#[cfg(not(unix))]
fn native_available_disk_bytes(_path: &std::path::Path) -> Result<u64, String> {
    // Native admission fails closed when this platform has no measured disk
    // adapter. Legacy generic torrent paths keep their existing behavior.
    Err("Torrent disk capacity unavailable".into())
}
impl TorrentAcquisition {
    pub fn failure_reason(&self) -> Option<AcquisitionFailure> {
        self.state
            .control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failure
    }
    pub fn status(&self) -> AcquisitionStatus {
        if self.state.manager.has_failed_settlement() {
            return AcquisitionStatus::SettlementFailed;
        }
        self.state
            .control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status
    }
    pub async fn wait_ready(&self) -> Result<TorrentHandle, String> {
        loop {
            let changed = self.state.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let control = self.state.control.lock().unwrap_or_else(|e| e.into_inner());
                match control.status {
                    AcquisitionStatus::Ready => {
                        return control
                            .handle
                            .as_ref()
                            .filter(|h| !h.status().stopped)
                            .cloned()
                            .ok_or("Native acquisition cancelled".into())
                    }
                    AcquisitionStatus::Preparing => {}
                    _ => return Err("Native acquisition unavailable".into()),
                }
            }
            if timeout_at(self.state.deadline, changed).await.is_err() {
                let failure = self.state.timeout_failure();
                self.state
                    .control
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .failure
                    .get_or_insert(failure);
                self.cancel();
                return Err("Native startup timed out".into());
            }
        }
    }
    pub fn cancel(&self) {
        // Serialize signal and publication; a late ready cannot escape cancel.
        let mut control = self.state.control.lock().unwrap_or_else(|e| e.into_inner());
        self.state.tasks.cancel();
        if control.status != AcquisitionStatus::SettlementFailed {
            control.status = AcquisitionStatus::Cancelled;
        }
        if let Some(tasks) = &control.io_tasks {
            tasks.cancel();
        }
        if let Some(handle) = &control.handle {
            handle.stop();
        }
        if let Some(lease) = &control.lease {
            lease.release();
        }
        self.state.changed.notify_waiters();
    }
    pub async fn cancel_and_wait(&self) -> Settlement {
        let deadline = Deadline::now() + Duration::from_secs(2);
        self.cancel();
        let (handle, lease, io_tasks) = {
            let control = self.state.control.lock().unwrap_or_else(|e| e.into_inner());
            (
                control.handle.clone(),
                control.lease.clone(),
                control.io_tasks.clone(),
            )
        };
        let mut joined = self.state.tasks.cancel_and_wait_until(deadline).await;
        if let Some(tasks) = io_tasks {
            joined &= tasks.cancel_and_wait_until(deadline).await && !tasks.has_failed_settlement();
        }
        if let Some(handle) = handle {
            joined &= handle.stop_and_settle_until(deadline).await == Settlement::Settled;
        }
        if let Some(lease) = lease {
            joined &= lease.settle_until(deadline).await;
        }
        joined &= !self.state.tasks.has_failed_settlement();
        if joined {
            let mut control = self.state.control.lock().unwrap_or_else(|e| e.into_inner());
            control.handle.take();
            control.io_tasks.take();
            control.lease.take();
            Settlement::Settled
        } else {
            self.state
                .manager
                .inner
                .settlement_failed
                .store(true, Ordering::Release);
            self.state
                .control
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .status = AcquisitionStatus::SettlementFailed;
            Settlement::SettlementFailed
        }
    }
}
impl Drop for TorrentAcquisition {
    fn drop(&mut self) {
        self.cancel();
    }
}
#[cfg(all(test, feature = "test-network-policy"))]
mod tests {
    use super::*;
    use librqbit::storage::{
        filesystem::{FilesystemStorage, FilesystemStorageFactory},
        BoxStorageFactory, StorageFactory, StorageFactoryExt, TorrentStorage,
    };
    #[derive(Default)]
    struct StorageGate {
        active: AtomicBool,
        release: std::sync::Mutex<bool>,
        changed: std::sync::Condvar,
    }
    impl StorageGate {
        fn release(&self) {
            *self.release.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }
    struct ReleaseStorage(Arc<StorageGate>);
    impl Drop for ReleaseStorage {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    struct Factory(Arc<StorageGate>);
    struct DelayedStorage {
        inner: FilesystemStorage,
        gate: Arc<StorageGate>,
    }
    impl StorageFactory for Factory {
        type Storage = DelayedStorage;
        fn create(
            &self,
            shared: &librqbit::ManagedTorrentShared,
            metadata: &librqbit::TorrentMetadata,
        ) -> anyhow::Result<Self::Storage> {
            Ok(DelayedStorage {
                inner: FilesystemStorageFactory::default().create(shared, metadata)?,
                gate: self.0.clone(),
            })
        }
        fn clone_box(&self) -> BoxStorageFactory {
            Factory(self.0.clone()).boxed()
        }
    }
    impl TorrentStorage for DelayedStorage {
        fn init(
            &mut self,
            shared: &librqbit::ManagedTorrentShared,
            metadata: &librqbit::TorrentMetadata,
        ) -> anyhow::Result<()> {
            self.inner.init(shared, metadata)
        }
        fn pread_exact(&self, id: usize, offset: u64, bytes: &mut [u8]) -> anyhow::Result<()> {
            self.gate.active.store(true, Ordering::Release);
            let release = self.gate.release.lock().unwrap();
            let _released = self
                .gate
                .changed
                .wait_timeout_while(release, Duration::from_secs(10), |r| !*r)
                .unwrap();
            let result = self.inner.pread_exact(id, offset, bytes);
            self.gate.active.store(false, Ordering::Release);
            result
        }
        fn pwrite_all(&self, id: usize, offset: u64, bytes: &[u8]) -> anyhow::Result<()> {
            self.inner.pwrite_all(id, offset, bytes)
        }
        fn remove_file(&self, id: usize, filename: &std::path::Path) -> anyhow::Result<()> {
            self.inner.remove_file(id, filename)
        }
        fn remove_directory_if_empty(&self, path: &std::path::Path) -> anyhow::Result<()> {
            self.inner.remove_directory_if_empty(path)
        }
        fn ensure_file_length(&self, id: usize, length: u64) -> anyhow::Result<()> {
            self.inner.ensure_file_length(id, length)
        }
        fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
            self.inner.take()
        }
    }
    fn metainfo() -> Vec<u8> {
        let mut b =
            b"d4:infod6:lengthi1e4:name9:owned.mp412:piece lengthi16384e6:pieces20:".to_vec();
        b.extend([7; 20]);
        b.extend_from_slice(b"ee");
        b
    }
    fn manager(cache: PathBuf, owned: SocketAddr) -> TorrentManager {
        TorrentManager::new_native_owned(
            TorrentConfig {
                enabled: true,
                cache_dir: cache,
                enable_dht: false,
                ..Default::default()
            },
            vec![owned],
        )
        .unwrap()
    }
    fn multi_file_metainfo(name: &str, unselected_size: u64) -> Vec<u8> {
        let total = 1 + unselected_size;
        let pieces = total.div_ceil(16_777_216);
        let mut bytes = format!(
            "d4:infod5:filesld6:lengthi1e4:pathl9:owned.mp4eed6:lengthi{unselected_size}e4:pathl10:unused.bineee4:name{}:{name}12:piece lengthi16777216e6:pieces{}:",
            name.len(), pieces * 20
        ).into_bytes();
        bytes.extend(std::iter::repeat_n(7, (pieces * 20) as usize));
        bytes.extend_from_slice(b"ee");
        bytes
    }
    fn begin_fixture(manager: &TorrentManager, bytes: Vec<u8>) -> TorrentAcquisition {
        let hash = vet_native_metainfo(&bytes, None, 0, Some(1))
            .unwrap()
            .info_hash_hex();
        manager
            .begin_native_selected_metainfo("owned".into(), bytes, hash, 0, 1, READ_TIMEOUT)
            .unwrap()
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_selected_size_resolves_exact_metadata_and_charges_all_files() {
        let temp = tempfile::tempdir().unwrap();
        let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let manager = manager(temp.path().join("cache"), peer.local_addr().unwrap());
        let bytes = multi_file_metainfo("owned", 17);
        let hash = vet_native_metainfo(&bytes, None, 0, None)
            .unwrap()
            .info_hash_hex();
        assert!(manager
            .begin_native_selected_metainfo(
                "owned".into(),
                bytes.clone(),
                hash.clone(),
                0,
                Some(2),
                READ_TIMEOUT
            )
            .is_err());
        let unknown = manager
            .begin_native_selected_metainfo(
                "owned".into(),
                bytes.clone(),
                hash.clone(),
                0,
                None,
                READ_TIMEOUT,
            )
            .unwrap();
        let first = unknown.wait_ready().await.unwrap();
        assert_eq!(first.files()[0].size, 1);
        assert_eq!(first.metadata_file_count(), 2);
        assert_eq!(manager.inner.state.lock().await.reserved, 18);
        let known = manager
            .begin_native_selected_metainfo("owned".into(), bytes, hash, 0, Some(1), READ_TIMEOUT)
            .unwrap();
        let second = known.wait_ready().await.unwrap();
        assert_eq!(manager.inner.state.lock().await.reserved, 18);
        assert_eq!(unknown.cancel_and_wait().await, Settlement::Settled);
        assert!(first.stream_url(0).is_err());
        assert!(second.stream_url(0).is_ok());
        assert_eq!(known.cancel_and_wait().await, Settlement::Settled);
        drop(first);
        drop(second);
        drop(unknown);
        drop(known);
        manager.reap().await.unwrap();
        assert_eq!(manager.inner.state.lock().await.reserved, 0);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_size_nonzero_index_retains_real_metadata_count_without_unselected_lengths() {
        let temp = tempfile::tempdir().unwrap();
        let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let manager = manager(temp.path().join("cache"), peer.local_addr().unwrap());
        let bytes = multi_file_metainfo("owned", 17);
        let hash = vet_native_metainfo(&bytes, None, 1, None)
            .unwrap()
            .info_hash_hex();
        let acquisition = manager
            .begin_native_selected_metainfo("owned".into(), bytes, hash, 1, None, READ_TIMEOUT)
            .unwrap();
        let handle = acquisition.wait_ready().await.unwrap();
        assert_eq!(handle.files().len(), 1);
        assert_eq!(handle.files()[0].index, 1);
        assert_eq!(handle.files()[0].size, 17);
        assert_eq!(handle.metadata_file_count(), 2);
        assert!(handle.stream_url(0).is_err());
        assert_eq!(manager.inner.state.lock().await.reserved, 18);
        assert_eq!(acquisition.cancel_and_wait().await, Settlement::Settled);
        drop(handle);
        drop(acquisition);
        manager.reap().await.unwrap();
        assert_eq!(manager.inner.state.lock().await.reserved, 0);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_disk_fact_and_full_unselected_payload_refuse_before_storage_allocation() {
        let temp = tempfile::tempdir().unwrap();
        let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let manager = manager(temp.path().join("cache"), peer.local_addr().unwrap());
        manager
            .inner
            .native_available_bytes
            .store(1, Ordering::Release);
        let acquisition = begin_fixture(&manager, multi_file_metainfo("owned", 1));
        assert!(acquisition.wait_ready().await.is_err());
        assert_eq!(acquisition.cancel_and_wait().await, Settlement::Settled);
        assert_eq!(manager.inner.state.lock().await.reserved, 0);
        assert!(native_available_disk_bytes(&manager.inner.config.cache_dir).unwrap() > 0);
        manager.reap().await.unwrap();
        let oversized = begin_fixture(
            &manager,
            multi_file_metainfo("oversized", manager.inner.config.max_bytes),
        );
        assert!(oversized.wait_ready().await.is_err());
        assert_eq!(oversized.cancel_and_wait().await, Settlement::Settled);
        assert_eq!(manager.inner.state.lock().await.reserved, 0);
        manager.reap().await.unwrap();
        assert!(std::fs::read_dir(&manager.inner.config.cache_dir)
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("torrent-")));
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_shared_payload_charges_once_and_candidate_disk_failure_preserves_outgoing() {
        let temp = tempfile::tempdir().unwrap();
        let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let manager = manager(temp.path().join("cache"), peer.local_addr().unwrap());
        manager
            .inner
            .native_available_bytes
            .store(3, Ordering::Release);
        let outgoing = begin_fixture(&manager, multi_file_metainfo("shared", 1));
        let outgoing_handle = outgoing.wait_ready().await.unwrap();
        assert_eq!(manager.inner.state.lock().await.reserved, 2);
        let same = begin_fixture(&manager, multi_file_metainfo("shared", 1));
        let same_handle = same.wait_ready().await.unwrap();
        assert_eq!(manager.inner.state.lock().await.reserved, 2);
        let candidate = begin_fixture(&manager, multi_file_metainfo("candidate", 1));
        assert!(candidate.wait_ready().await.is_err());
        assert_eq!(candidate.cancel_and_wait().await, Settlement::Settled);
        assert!(!outgoing_handle.status().stopped);
        assert!(!same_handle.status().stopped);
        assert_eq!(manager.inner.state.lock().await.reserved, 2);
        assert_eq!(outgoing.cancel_and_wait().await, Settlement::Settled);
        assert!(!same_handle.status().stopped);
        assert_eq!(manager.inner.state.lock().await.reserved, 2);
        assert_eq!(same.cancel_and_wait().await, Settlement::Settled);
        drop(outgoing_handle);
        drop(same_handle);
        manager.reap().await.unwrap();
        assert_eq!(manager.inner.state.lock().await.reserved, 0);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rolling_native_shared_charge_and_capacity_refusal_preserve_outgoing() {
        let temp = tempfile::tempdir().unwrap();
        let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let budget = MIN_CACHE_BYTES / 2;
        let manager = TorrentManager::new_native_owned(
            TorrentConfig {
                enabled: true,
                cache_dir: temp.path().join("cache"),
                enable_dht: false,
                max_bytes: MIN_CACHE_BYTES,
                piece_cache_bytes: Some(budget),
                ..Default::default()
            },
            vec![peer.local_addr().unwrap()],
        )
        .unwrap();
        let outgoing = begin_fixture(
            &manager,
            multi_file_metainfo("shared-large", 3 * 1024 * 1024 * 1024),
        );
        let first = outgoing.wait_ready().await.unwrap();
        let shared = begin_fixture(
            &manager,
            multi_file_metainfo("shared-large", 3 * 1024 * 1024 * 1024),
        );
        let second = shared.wait_ready().await.unwrap();
        assert_eq!(manager.inner.state.lock().await.reserved, budget);
        let candidate = begin_fixture(
            &manager,
            multi_file_metainfo("candidate-large", 3 * 1024 * 1024 * 1024),
        );
        let third = candidate.wait_ready().await.unwrap();
        assert_eq!(manager.inner.state.lock().await.reserved, MIN_CACHE_BYTES);
        let refused = begin_fixture(
            &manager,
            multi_file_metainfo("refused-large", 3 * 1024 * 1024 * 1024),
        );
        assert!(refused.wait_ready().await.is_err());
        assert_eq!(
            refused.failure_reason(),
            Some(AcquisitionFailure::PayloadLimit)
        );
        assert!(!first.status().stopped && !second.status().stopped && !third.status().stopped);
        assert_eq!(refused.cancel_and_wait().await, Settlement::Settled);
        assert_eq!(outgoing.cancel_and_wait().await, Settlement::Settled);
        assert!(!second.status().stopped);
        assert_eq!(manager.inner.state.lock().await.reserved, MIN_CACHE_BYTES);
        assert_eq!(shared.cancel_and_wait().await, Settlement::Settled);
        assert_eq!(candidate.cancel_and_wait().await, Settlement::Settled);
        drop((first, second, third));
        manager.reap().await.unwrap();
        assert_eq!(manager.inner.state.lock().await.reserved, 0);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rolling_native_piece_slot_requirement_reports_capacity_before_storage() {
        let temp = tempfile::tempdir().unwrap();
        let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let manager = TorrentManager::new_native_owned(
            TorrentConfig {
                enabled: true,
                cache_dir: temp.path().join("cache"),
                enable_dht: false,
                piece_cache_bytes: Some(16 * 1024 * 1024),
                ..Default::default()
            },
            vec![peer.local_addr().unwrap()],
        )
        .unwrap();
        let acquisition = begin_fixture(
            &manager,
            multi_file_metainfo("large-pieces", 3 * 1024 * 1024 * 1024),
        );
        assert!(acquisition.wait_ready().await.is_err());
        assert_eq!(
            acquisition.failure_reason(),
            Some(AcquisitionFailure::PayloadLimit)
        );
        assert_eq!(manager.inner.state.lock().await.reserved, 0);
        assert_eq!(acquisition.cancel_and_wait().await, Settlement::Settled);
        manager.reap().await.unwrap();
    }
    #[tokio::test]
    async fn cancel_before_first_poll_never_creates_cache_or_publishes_a_handle() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let manager = manager(cache.clone(), peer.local_addr().unwrap());
        let bytes = metainfo();
        let hash = vet_native_metainfo(&bytes, None, 0, Some(1))
            .unwrap()
            .info_hash_hex();
        let acquisition = manager
            .begin_native_selected_metainfo("owned".into(), bytes, hash, 0, 1, READ_TIMEOUT)
            .unwrap();
        acquisition.cancel();
        assert_eq!(acquisition.cancel_and_wait().await, Settlement::Settled);
        assert!(acquisition.wait_ready().await.is_err());
        assert!(!cache.exists());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn registry_queue_and_real_metadata_wait_share_one_absolute_budget() {
        let temp = tempfile::tempdir().unwrap();
        let peer = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let manager = manager(temp.path().join("cache"), peer.local_addr().unwrap());
        let lock = manager.inner.state.lock().await;
        let hash = "1111111111111111111111111111111111111111".to_owned();
        let entered = std::time::Instant::now();
        let acquisition = manager
            .begin_native_selected_magnet(
                "owned".into(),
                format!("magnet:?xt=urn:btih:{hash}"),
                hash,
                0,
                1,
                Duration::from_millis(300),
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(lock);
        let (socket, _) = tokio::time::timeout(Duration::from_millis(100), peer.accept())
            .await
            .unwrap()
            .unwrap();
        assert!(acquisition.wait_ready().await.is_err());
        assert!(entered.elapsed() < Duration::from_millis(450));
        assert_eq!(
            format!("{:?}", acquisition.failure_reason()),
            "Some(MetadataTimeout)",
            "a connected peer that never supplies metadata must retain the actual failed stage"
        );
        assert_eq!(acquisition.cancel_and_wait().await, Settlement::Settled);
        drop(socket);
        manager.reap().await.unwrap();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocked_registry_timeout_is_cache_preparation_not_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let manager = manager(temp.path().join("cache"), peer.local_addr().unwrap());
        let lock = manager.inner.state.lock().await;
        let hash = "1111111111111111111111111111111111111111".to_owned();
        let acquisition = manager
            .begin_native_selected_magnet(
                "owned".into(),
                format!("magnet:?xt=urn:btih:{hash}"),
                hash,
                0,
                1,
                Duration::from_millis(100),
            )
            .unwrap();
        assert!(acquisition.wait_ready().await.is_err());
        assert_eq!(
            acquisition.failure_reason(),
            Some(AcquisitionFailure::CachePreparationTimeout)
        );
        drop(lock);
        assert_eq!(acquisition.cancel_and_wait().await, Settlement::Settled);
        assert_eq!(
            acquisition.failure_reason(),
            Some(AcquisitionFailure::CachePreparationTimeout)
        );
        manager.reap().await.unwrap();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocked_storage_timeout_retains_initialization_stage_through_join() {
        let temp = tempfile::tempdir().unwrap();
        let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let manager = manager(temp.path().join("cache"), peer.local_addr().unwrap());
        let gate = Arc::new(StorageGate::default());
        let _release = ReleaseStorage(gate.clone());
        *manager.inner.native_storage_factory.lock().unwrap() = Some(Factory(gate.clone()).boxed());
        let bytes = metainfo();
        let hash = vet_native_metainfo(&bytes, None, 0, Some(1))
            .unwrap()
            .info_hash_hex();
        let acquisition = manager
            .begin_native_selected_metainfo(
                "owned".into(),
                bytes,
                hash,
                0,
                1,
                Duration::from_millis(300),
            )
            .unwrap();
        timeout(Duration::from_secs(2), async {
            while !gate.active.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert!(acquisition.wait_ready().await.is_err());
        assert_eq!(
            acquisition.failure_reason(),
            Some(AcquisitionFailure::InitializationTimeout)
        );
        gate.release();
        assert_eq!(acquisition.cancel_and_wait().await, Settlement::Settled);
        assert_eq!(
            acquisition.failure_reason(),
            Some(AcquisitionFailure::InitializationTimeout)
        );
        manager.reap().await.unwrap();
        assert_eq!(manager.inner.state.lock().await.reserved, 0);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unresolved_reader_receipt_poison_is_static_and_keeps_full_reservation() {
        let temp = tempfile::tempdir().unwrap();
        let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let manager = manager(temp.path().join("cache"), peer.local_addr().unwrap());
        let bytes = metainfo();
        let hash = vet_native_metainfo(&bytes, None, 0, Some(1))
            .unwrap()
            .info_hash_hex();
        let acquisition = manager
            .begin_native_selected_metainfo(
                "owned".into(),
                bytes.clone(),
                hash.clone(),
                0,
                1,
                READ_TIMEOUT,
            )
            .unwrap();
        let handle = acquisition.wait_ready().await.unwrap();
        // An external poll receipt deliberately held across the retirement
        // deadline cannot become success, including after its eventual release.
        let held = handle.state.io_tasks.register().unwrap();
        assert_eq!(
            acquisition.cancel_and_wait().await,
            Settlement::SettlementFailed
        );
        assert_eq!(acquisition.status(), AcquisitionStatus::SettlementFailed);
        assert!(manager.has_failed_settlement());
        assert!(manager
            .begin_native_selected_metainfo("other".into(), bytes, hash, 0, 1, READ_TIMEOUT)
            .is_err());
        drop(held);
        drop(handle);
        drop(acquisition);
        manager.reap().await.unwrap();
        let state = manager.inner.state.lock().await;
        assert_eq!(state.reserved, 1);
        assert_eq!(state.pending.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_prehandle_waiter_retires_without_cancelling_the_shared_metadata_peer() {
        let temp = tempfile::tempdir().unwrap();
        let peer = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let manager = manager(temp.path().join("cache"), peer.local_addr().unwrap());
        let hash = "1111111111111111111111111111111111111111".to_owned();
        let begin = || {
            manager
                .begin_native_selected_magnet(
                    "owned".into(),
                    format!("magnet:?xt=urn:btih:{hash}"),
                    hash.clone(),
                    0,
                    1,
                    READ_TIMEOUT,
                )
                .unwrap()
        };
        let first = begin();
        let second = begin();
        let (mut socket, _) = timeout(Duration::from_secs(1), peer.accept())
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(1), async {
            loop {
                if manager
                    .inner
                    .state
                    .lock()
                    .await
                    .pending
                    .values()
                    .any(|pending| pending.consumers.load(Ordering::Acquire) == 2)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut handshake = [0; 68];
        timeout(Duration::from_secs(1), socket.read_exact(&mut handshake))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.cancel_and_wait().await, Settlement::Settled);
        assert!(first.wait_ready().await.is_err());
        assert_eq!(second.status(), AcquisitionStatus::Preparing);
        assert!(timeout(Duration::from_millis(30), socket.read(&mut [0]))
            .await
            .is_err());
        assert_eq!(second.cancel_and_wait().await, Settlement::Settled);
        assert_eq!(
            timeout(Duration::from_secs(1), socket.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        manager.reap().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cache_initialization_wait_has_owned_prehandle_cancellation() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let manager = manager(cache.clone(), peer.local_addr().unwrap());
        let (entered, entry) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let owner = manager.inner.clone();
        let initializing = tokio::spawn(async move {
            owner
                .cache_lock
                .get_or_try_init(|| async {
                    entered.send(()).unwrap();
                    released.await.unwrap();
                    prepare_cache(&owner.config.cache_dir).map(Arc::new)
                })
                .await
                .unwrap();
        });
        entry.await.unwrap();
        let bytes = metainfo();
        let hash = vet_native_metainfo(&bytes, None, 0, Some(1))
            .unwrap()
            .info_hash_hex();
        let acquisition = manager
            .begin_native_selected_metainfo("owned".into(), bytes, hash, 0, 1, READ_TIMEOUT)
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(acquisition.cancel_and_wait().await, Settlement::Settled);
        assert!(acquisition.wait_ready().await.is_err());
        assert!(!cache.exists());
        release.send(()).unwrap();
        initializing.await.unwrap();
        manager.reap().await.unwrap();
    }

    async fn initialization_case(release_within_bound: bool) {
        let temp = tempfile::tempdir().unwrap();
        let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let manager = manager(temp.path().join("cache"), peer.local_addr().unwrap());
        let gate = Arc::new(StorageGate::default());
        let _release = ReleaseStorage(gate.clone());
        *manager.inner.native_storage_factory.lock().unwrap() = Some(Factory(gate.clone()).boxed());
        let bytes = metainfo();
        let hash = vet_native_metainfo(&bytes, None, 0, Some(1))
            .unwrap()
            .info_hash_hex();
        let acquisition = manager
            .begin_native_selected_metainfo("owned".into(), bytes, hash, 0, 1, READ_TIMEOUT)
            .unwrap();
        timeout(Duration::from_secs(2), async {
            while !gate.active.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(manager.inner.state.lock().await.reserved, 1);
        if release_within_bound {
            let release = gate.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                release.release();
            });
            assert_eq!(acquisition.cancel_and_wait().await, Settlement::Settled);
            assert!(!gate.active.load(Ordering::Acquire));
            assert!(acquisition.wait_ready().await.is_err());
            manager.reap().await.unwrap();
            assert_eq!(manager.inner.state.lock().await.reserved, 0);
        } else {
            assert_eq!(
                acquisition.cancel_and_wait().await,
                Settlement::SettlementFailed
            );
            assert!(gate.active.load(Ordering::Acquire));
            manager.reap().await.unwrap();
            assert_eq!(manager.inner.state.lock().await.reserved, 1);
            assert!(manager.inner.state.lock().await.pending.values().all(|p| p
                .folder
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .exists()));
            gate.release();
            let pending = manager
                .inner
                .state
                .lock()
                .await
                .pending
                .values()
                .next()
                .unwrap()
                .clone();
            assert!(
                pending
                    .tasks
                    .cancel_and_wait_until(Deadline::now() + Duration::from_secs(2))
                    .await
            );
            assert!(!gate.active.load(Ordering::Acquire));
            assert_eq!(
                acquisition.cancel_and_wait().await,
                Settlement::SettlementFailed
            );
            manager.reap().await.unwrap();
            assert_eq!(manager.inner.state.lock().await.reserved, 1);
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prehandle_initialization_cancellation_joins_actual_filesystem_reads() {
        initialization_case(true).await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_prehandle_filesystem_io_never_acknowledges_or_reaps_false_settlement() {
        initialization_case(false).await;
    }

    #[test]
    fn queued_filesystem_work_is_disposed_before_a_cancelled_operation_can_create_files() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("never-created");
        let inspected = directory.clone();
        let called = Arc::new(AtomicBool::new(false));
        let visited = called.clone();
        runtime.block_on(async move {
            let (entered, entry) = tokio::sync::oneshot::channel();
            let (release, released) = std::sync::mpsc::channel();
            let blocking = tokio::task::spawn_blocking(move || {
                entered.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(5)).unwrap();
            });
            entry.await.unwrap();
            let tasks = OwnedTasks::new(Default::default());
            let producer = tasks.clone();
            let (queued, enqueued) = tokio::sync::oneshot::channel();
            let worker = tasks
                .spawn(tracing::Span::none(), async move {
                    queued.send(()).unwrap();
                    let _ = owned_fs(&producer, move || {
                        visited.store(true, Ordering::Release);
                        std::fs::create_dir(directory)
                            .map_err(|_| "Fixture filesystem unavailable".into())
                    })
                    .await;
                    Ok(())
                })
                .unwrap();
            enqueued.await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            tasks.cancel();
            release.send(()).unwrap();
            assert_eq!(
                tasks
                    .cancel_and_settle_until(Deadline::now() + Duration::from_secs(2))
                    .await,
                Settlement::Settled
            );
            worker.await.unwrap();
            blocking.await.unwrap();
        });
        assert!(!called.load(Ordering::Acquire));
        assert!(!inspected.exists());
    }
}
