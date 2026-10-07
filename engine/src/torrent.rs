//! Opt-in BitTorrent byte transport. HTTP trackers and webseeds are disabled:
//! the library does not provide the gateway's vetted HTTP egress seam.
//! A process manager owns independent per-scope sessions and a shared cache budget.
use crate::torrent_stream::LoopbackStream;
mod acquisition;
pub use acquisition::{AcquisitionStatus, TorrentAcquisition};
use acquisition::{PendingLease, PendingPayload};
pub use librqbit::native_metainfo::{NativeMetainfo, NativeSelection};
use librqbit::network_policy::NetworkPolicy;
use librqbit::OwnedTasks;
pub use librqbit::Settlement;
/// Vet upstream metainfo and preserve exact info bytes for an explicit selected file.
pub fn vet_native_metainfo(
    bytes: &[u8],
    expected_hash: Option<&str>,
    index: u32,
    expected_size: Option<u64>,
) -> Result<NativeMetainfo, String> {
    let vetted = librqbit::native_metainfo::vet_native_metainfo(bytes)
        .map_err(|_| "Invalid native metainfo")?;
    vetted
        .verify_selection(expected_hash, index, expected_size)
        .map_err(|_| "Native exact selection denied")?;
    Ok(vetted)
}

use librqbit::{
    torrent_from_bytes, torrent_from_bytes_ext, AddTorrent, AddTorrentOptions, AddTorrentResponse,
    ByteBufOwned, Session, SessionOptions, TorrentMetaV1Info,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Weak,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    sync::{Mutex, Notify, OnceCell},
    time::timeout,
};
use uuid::Uuid;

pub const MIN_CACHE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_METAINFO_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_RANGE_BYTES: u32 = 8 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Operator configuration. Constructing a disabled manager does no IO.
#[derive(Clone)]
pub struct TorrentConfig {
    pub enabled: bool,
    pub cache_dir: PathBuf,
    pub max_bytes: u64,
    pub listen_port: Option<u16>,
    /// Peer/DHT connections are an explicit opt-in exception to HTTP egress policy.
    pub enable_dht: bool,
    /// Operator-supplied bootstrap peers (not source URI fields).
    pub initial_peers: Vec<SocketAddr>,
    pub idle_timeout: Duration,
}
impl Default for TorrentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cache_dir: PathBuf::from("data/torrent-cache"),
            max_bytes: 2 * 1024 * 1024 * 1024,
            listen_port: None,
            enable_dht: true,
            initial_peers: Vec::new(),
            idle_timeout: Duration::from_secs(60),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TorrentFile {
    pub index: u32,
    pub name: String,
    pub size: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TorrentStatus {
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub finished: bool,
    pub stopped: bool,
}

#[derive(Clone)]
pub struct TorrentManager {
    inner: Arc<ManagerInner>,
}
struct ManagerInner {
    runtime: OnceCell<tokio::runtime::Handle>,
    config: TorrentConfig,
    network_policy: NetworkPolicy,
    cache_lock: OnceCell<Arc<std::fs::File>>,
    state: Mutex<ManagerState>,
    settlement_failed: AtomicBool,
    acquisition_owners: std::sync::Mutex<Vec<OwnedTasks>>,
    #[cfg(all(test, feature = "test-network-policy"))]
    native_storage_factory: std::sync::Mutex<Option<librqbit::storage::BoxStorageFactory>>,
    #[cfg(all(test, feature = "test-network-policy"))]
    native_available_bytes: std::sync::atomic::AtomicU64,
}
#[derive(Default)]
struct ManagerState {
    scopes: HashMap<String, Arc<Session>>,
    entries: HashMap<(String, String), Arc<Entry>>,
    reserved: u64,
    pending: HashMap<(String, String), Arc<PendingPayload>>,
}
struct Entry {
    _cache_lock: Arc<std::fs::File>,
    session: Arc<Session>,
    managed: Arc<librqbit::ManagedTorrent>,
    files: Vec<TorrentFile>,
    folder: PathBuf,
    size: u64,
    users: AtomicUsize,
    readers: AtomicUsize,
    idle_since: std::sync::Mutex<Instant>,
    handles: std::sync::Mutex<HashMap<u32, Weak<HandleState>>>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum LeaseMode {
    // Repeated legacy starts share a lease and can read any torrent file.
    Shared,
    // Repeated starts share payload only, never byte authority.
    SelectedFile,
}
/// Clones share one lease. The last clone closes that lease's listener.
/// Payload becomes idle only after every lease and active reader releases it.
pub struct TorrentHandle {
    pub(crate) state: Arc<HandleState>,
}
pub(crate) struct HandleState {
    entry: Arc<Entry>,
    manager: Arc<ManagerInner>,
    selected: u32,
    mode: LeaseMode,
    owners: std::sync::Mutex<usize>,
    reads: Arc<TorrentReads>,
    pub(crate) stopped: AtomicBool,
    server: OnceCell<LoopbackStream>,
    io_tasks: OwnedTasks,
    native_lease: Option<Arc<PendingLease>>,
}
impl std::fmt::Debug for TorrentHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TorrentHandle(<redacted>)")
    }
}
impl std::fmt::Debug for TorrentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TorrentManager(<redacted>)")
    }
}

impl TorrentManager {
    pub fn new(config: TorrentConfig) -> Result<Self, String> {
        Self::new_with_policy(config, NetworkPolicy::Gateway)
    }
    /// Production native policy has fixed discovery and no source-added peers/listener.
    pub fn new_native_public(mut config: TorrentConfig) -> Result<Self, String> {
        if !config.initial_peers.is_empty() || config.listen_port.is_some() {
            return Err("Native network configuration denied".into());
        }
        config.enable_dht = true;
        Self::new_with_policy(config, NetworkPolicy::PublicDhtTcpV1)
    }
    /// Only separately compiled fixture artifacts can admit exact owned private TCP seeders.
    #[cfg(feature = "test-network-policy")]
    pub fn new_native_owned(
        mut config: TorrentConfig,
        owned: Vec<SocketAddr>,
    ) -> Result<Self, String> {
        if config.enable_dht
            || config.listen_port.is_some()
            || !config.initial_peers.is_empty()
            || owned.is_empty()
        {
            return Err("Owned fixture configuration denied".into());
        }
        let policy = NetworkPolicy::OwnedPrivateTcp(owned.clone());
        if !owned.iter().all(|a| policy.allows(*a)) {
            return Err("Owned fixture destination denied".into());
        }
        config.initial_peers = owned;
        Self::new_with_policy(config, policy)
    }
    fn new_with_policy(
        config: TorrentConfig,
        network_policy: NetworkPolicy,
    ) -> Result<Self, String> {
        if config.max_bytes < MIN_CACHE_BYTES
            || config.listen_port == Some(u16::MAX)
            || config.listen_port == Some(0)
        {
            return Err("Invalid torrent configuration".into());
        }
        Ok(Self {
            inner: Arc::new(ManagerInner {
                runtime: OnceCell::new(),
                config,
                network_policy,
                cache_lock: OnceCell::new(),
                state: Mutex::new(ManagerState::default()),
                settlement_failed: AtomicBool::new(false),
                acquisition_owners: std::sync::Mutex::new(Vec::new()),
                #[cfg(all(test, feature = "test-network-policy"))]
                native_storage_factory: std::sync::Mutex::new(None),
                #[cfg(all(test, feature = "test-network-policy"))]
                native_available_bytes: std::sync::atomic::AtomicU64::new(u64::MAX),
            }),
        })
    }
    pub async fn start_magnet(
        &self,
        scope: &str,
        source: &str,
        index: Option<u32>,
    ) -> Result<TorrentHandle, String> {
        self.start_magnet_with_mode(scope, source, index, LeaseMode::Shared)
            .await
    }
    async fn start_magnet_with_mode(
        &self,
        scope: &str,
        source: &str,
        index: Option<u32>,
        mode: LeaseMode,
    ) -> Result<TorrentHandle, String> {
        if !self.inner.config.enabled {
            return Err("Torrent transport disabled".into());
        }
        validate_magnet(source)?;
        let mut magnet = url::Url::parse(source).map_err(|_| "Invalid magnet URI")?;
        let fields: Vec<_> = magnet
            .query_pairs()
            .filter(|(k, _)| k != "tr")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        magnet.query_pairs_mut().clear().extend_pairs(fields);
        self.start(
            scope,
            AddTorrent::from_url(magnet.as_str()),
            index,
            mode,
            None,
        )
        .await
    }
    /// The caller fetches HTTP metainfo using its vetted redirect/DNS policy.
    pub async fn start_metainfo(
        &self,
        scope: &str,
        bytes: Vec<u8>,
        index: Option<u32>,
    ) -> Result<TorrentHandle, String> {
        self.start_metainfo_with_mode(scope, bytes, index, LeaseMode::Shared)
            .await
    }
    async fn start_metainfo_with_mode(
        &self,
        scope: &str,
        bytes: Vec<u8>,
        index: Option<u32>,
        mode: LeaseMode,
    ) -> Result<TorrentHandle, String> {
        if !self.inner.config.enabled {
            return Err("Torrent transport disabled".into());
        }
        validate_metainfo(&bytes)?;
        self.start(
            scope,
            AddTorrent::from_bytes(strip_trackers(&bytes)?),
            index,
            mode,
            None,
        )
        .await
    }
    pub async fn start_native_selected_metainfo(
        &self,
        scope: &str,
        bytes: Vec<u8>,
        expected_hash: &str,
        index: u32,
        verified_size: u64,
    ) -> Result<TorrentHandle, String> {
        if !self.inner.network_policy.is_native() || !self.inner.config.enabled {
            return Err("Native transport unavailable".into());
        }
        let selection = NativeSelection::from_hex(expected_hash, index, verified_size)
            .map_err(|_| "Native exact selection denied")?;
        let vetted = vet_native_metainfo(&bytes, Some(expected_hash), index, Some(verified_size))?;
        self.start(
            scope,
            AddTorrent::from_bytes(vetted.canonical_bytes().to_vec()),
            Some(index),
            LeaseMode::SelectedFile,
            Some(selection),
        )
        .await
    }
    pub async fn start_native_selected_magnet(
        &self,
        scope: &str,
        source: &str,
        expected_hash: &str,
        index: u32,
        verified_size: u64,
    ) -> Result<TorrentHandle, String> {
        if !self.inner.network_policy.is_native() || !self.inner.config.enabled {
            return Err("Native transport unavailable".into());
        }
        let selection = NativeSelection::from_hex(expected_hash, index, verified_size)
            .map_err(|_| "Native exact selection denied")?;
        if source != format!("magnet:?xt=urn:btih:{expected_hash}") {
            return Err("Native source input denied".into());
        }
        self.start(
            scope,
            AddTorrent::from_url(source),
            Some(index),
            LeaseMode::SelectedFile,
            Some(selection),
        )
        .await
    }
    /// Start a separately stoppable lease restricted to an explicit file.
    pub async fn start_selected_magnet(
        &self,
        scope: &str,
        source: &str,
        index: u32,
    ) -> Result<TorrentHandle, String> {
        self.start_magnet_with_mode(scope, source, Some(index), LeaseMode::SelectedFile)
            .await
    }
    /// The caller fetches metainfo using its vetted redirect/DNS policy.
    /// Each call owns a separate selected-file byte capability.
    pub async fn start_selected_metainfo(
        &self,
        scope: &str,
        bytes: Vec<u8>,
        index: u32,
    ) -> Result<TorrentHandle, String> {
        self.start_metainfo_with_mode(scope, bytes, Some(index), LeaseMode::SelectedFile)
            .await
    }
    /// Reserve additional bytes for a decompressed archive spool. Keep the
    /// reservation alive until the spool and every range reader are gone.
    pub async fn reserve_cache(&self, bytes: u64) -> Result<TorrentCacheReservation, String> {
        if !self.inner.config.enabled {
            return Err("Torrent transport disabled".into());
        }
        let _ = self.inner.runtime.set(tokio::runtime::Handle::current());
        self.inner
            .cache_lock
            .get_or_try_init(|| async { prepare_cache(&self.inner.config.cache_dir).map(Arc::new) })
            .await?;
        let mut state = self.inner.state.lock().await;
        self.evict(&mut state, Some(bytes)).await?;
        let folder = self
            .inner
            .config
            .cache_dir
            .join(format!("torrent-{}", Uuid::new_v4().simple()));
        std::fs::create_dir(&folder).map_err(|_| "Torrent cache unavailable")?;
        private_dir(&folder)?;
        state.reserved += bytes;
        Ok(TorrentCacheReservation {
            _cache_lock: self
                .inner
                .cache_lock
                .get()
                .expect("cache initialized")
                .clone(),
            manager: self.inner.clone(),
            bytes,
            folder,
        })
    }
    async fn start(
        &self,
        scope: &str,
        input: AddTorrent<'_>,
        index: Option<u32>,
        mode: LeaseMode,
        native_selection: Option<NativeSelection>,
    ) -> Result<TorrentHandle, String> {
        if self.inner.network_policy.is_native() && native_selection.is_none() {
            return Err("Native exact selection required".into());
        }
        if scope.is_empty() || scope.len() > 256 {
            return Err("Invalid torrent scope".into());
        }
        let _ = self.inner.runtime.set(tokio::runtime::Handle::current());
        self.inner
            .cache_lock
            .get_or_try_init(|| async { prepare_cache(&self.inner.config.cache_dir).map(Arc::new) })
            .await?;
        let scope = format!("{:x}", Sha256::digest(scope.as_bytes()));
        // Serialization also makes reservation, deletion and admission atomic.
        let mut state = self.inner.state.lock().await;
        if state
            .scopes
            .values()
            .any(|session| session.has_failed_settlement())
        {
            self.inner.settlement_failed.store(true, Ordering::Release);
        }
        if mode == LeaseMode::SelectedFile && self.inner.settlement_failed.load(Ordering::Acquire) {
            return Err("Torrent settlement failed".into());
        }
        let known_hash = match &input {
            AddTorrent::Url(raw) => librqbit::Magnet::parse(raw)
                .ok()
                .and_then(|m| m.as_id20())
                .map(|h| h.as_string()),
            AddTorrent::TorrentFileBytes(bytes) => torrent_from_bytes::<ByteBufOwned>(bytes)
                .ok()
                .map(|m| m.info_hash.as_string()),
        };
        if let Some(entry) =
            known_hash.and_then(|hash| state.entries.get(&(scope.clone(), hash)).cloned())
        {
            let selected = select_file(&entry.files, index)?;
            if native_selection.is_some_and(|n| {
                n.verified_size.is_some_and(|size| {
                    entry.files.get(selected as usize).map(|f| f.size) != Some(size)
                })
            }) {
                return Err("Native exact selection denied".into());
            }
            return self.lease_entry(entry, selected, mode).await;
        }
        let session = match state.scopes.get(&scope) {
            Some(s) => s.clone(),
            None => {
                if state.scopes.len() >= 32 {
                    return Err("Torrent scope capacity reached".into());
                }
                let listen = if state.scopes.is_empty() {
                    self.inner.config.listen_port.unwrap_or(0)
                } else {
                    0
                };
                let session = Session::new_with_opts(
                    self.inner.config.cache_dir.clone(),
                    SessionOptions {
                        network_policy: self.inner.network_policy.clone(),
                        disable_trackers: self.inner.network_policy.is_native(),
                        disable_dht: !self.inner.config.enable_dht,
                        disable_dht_persistence: true,
                        persistence: None,
                        listen_port_range: if self.inner.network_policy.is_native() {
                            None
                        } else {
                            Some(listen..listen + 1)
                        },
                        enable_upnp_port_forwarding: false,
                        concurrent_init_limit: Some(2),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|_| "Torrent session unavailable")?;
                state.scopes.insert(scope.clone(), session.clone());
                session
            }
        };
        let listed = timeout(
            READ_TIMEOUT,
            session.add_torrent(
                input,
                Some(AddTorrentOptions {
                    list_only: true,
                    native_selection,
                    only_files: native_selection.map(|n| vec![n.file_index as usize]),
                    disable_trackers: true,
                    initial_peers: Some(self.inner.config.initial_peers.clone()),
                    ..Default::default()
                }),
            ),
        )
        .await
        .map_err(|_| "Torrent metadata timed out")?
        .map_err(|_| "Invalid torrent metadata")?;
        let (hash, files, bytes, existing) = match listed {
            AddTorrentResponse::ListOnly(l) => {
                validate_metainfo(&l.torrent_bytes)?;
                (
                    l.info_hash.as_string(),
                    describe_files(&l.info)?,
                    Some(strip_trackers(&l.torrent_bytes)?),
                    None,
                )
            }
            AddTorrentResponse::AlreadyManaged(_, h) => (
                h.info_hash().as_string(),
                h.with_metadata(|m| describe_files(&m.info))
                    .map_err(|_| "Torrent metadata unavailable")??,
                None,
                Some(h),
            ),
            AddTorrentResponse::Added(_, _) => {
                return Err("Unexpected torrent metadata state".into())
            }
        };
        let selected = select_file(&files, index)?;
        if native_selection.is_some_and(|n| {
            n.verified_size
                .is_some_and(|size| files.get(selected as usize).map(|f| f.size) != Some(size))
                || hash != hex_hash(n.info_hash)
        }) {
            return Err("Native exact selection denied".into());
        }
        let key = (scope, hash);
        if let Some(entry) = state.entries.get(&key).cloned() {
            return self.lease_entry(entry, selected, mode).await;
        }
        if existing.is_some() {
            return Err("Torrent cache state unavailable".into());
        }
        let size = files
            .iter()
            .try_fold(0u64, |acc, file| acc.checked_add(file.size))
            .ok_or("Torrent size overflow")?;
        if size > self.inner.config.max_bytes {
            return Err("Torrent exceeds cache budget".into());
        }
        self.evict(&mut state, Some(size)).await?;
        let folder = self
            .inner
            .config
            .cache_dir
            .join(format!("torrent-{}", Uuid::new_v4().simple()));
        std::fs::create_dir(&folder).map_err(|_| "Torrent cache unavailable")?;
        private_dir(&folder)?;
        let response = session
            .add_torrent(
                AddTorrent::from_bytes(bytes.ok_or("Torrent metadata unavailable")?),
                Some(AddTorrentOptions {
                    output_folder: Some(
                        folder
                            .to_str()
                            .ok_or("Torrent cache path unsupported")?
                            .to_owned(),
                    ),
                    disable_trackers: true,
                    native_selection,
                    only_files: native_selection.map(|n| vec![n.file_index as usize]),
                    overwrite: false,
                    initial_peers: Some(self.inner.config.initial_peers.clone()),
                    ..Default::default()
                }),
            )
            .await
            .map_err(|_| "Torrent start failed")?;
        let managed = response.into_handle().ok_or("Torrent start failed")?;
        // Insert before waiting: cancellation leaves a reclaimable idle reservation.
        let entry = Arc::new(Entry {
            _cache_lock: self
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
        });
        state.reserved += size;
        state.entries.insert(key, entry.clone());
        timeout(READ_TIMEOUT, entry.managed.wait_until_initialized())
            .await
            .map_err(|_| "Torrent initialization timed out")?
            .map_err(|_| "Torrent initialization failed")?;
        self.make_handle(entry, selected, mode).await
    }
    async fn lease_entry(
        &self,
        entry: Arc<Entry>,
        selected: u32,
        mode: LeaseMode,
    ) -> Result<TorrentHandle, String> {
        if mode == LeaseMode::Shared {
            let existing = entry
                .handles
                .lock()
                .map_err(|_| "Torrent unavailable")?
                .get(&selected)
                .and_then(Weak::upgrade);
            if let Some(handle) = existing.filter(|h| !h.stopped.load(Ordering::Acquire)) {
                if let Some(handle) = TorrentHandle::acquire(handle) {
                    return Ok(handle);
                }
            }
        }
        if entry.users.load(Ordering::Acquire) == 0 && entry.managed.is_paused() {
            entry
                .session
                .unpause(&entry.managed)
                .await
                .map_err(|_| "Torrent resume failed")?;
        }
        self.make_handle(entry, selected, mode).await
    }
    async fn make_handle(
        &self,
        entry: Arc<Entry>,
        selected: u32,
        mode: LeaseMode,
    ) -> Result<TorrentHandle, String> {
        entry.users.fetch_add(1, Ordering::AcqRel);
        let state = Arc::new(HandleState {
            entry: entry.clone(),
            manager: self.inner.clone(),
            selected,
            mode,
            owners: std::sync::Mutex::new(1),
            reads: Arc::new(TorrentReads::default()),
            stopped: AtomicBool::new(false),
            server: OnceCell::new(),
            io_tasks: OwnedTasks::new(Default::default()),
            native_lease: None,
        });
        let server = LoopbackStream::start(Arc::downgrade(&state)).await?;
        state
            .server
            .set(server)
            .map_err(|_| "Torrent stream unavailable")?;
        if mode == LeaseMode::Shared {
            entry
                .handles
                .lock()
                .map_err(|_| "Torrent unavailable")?
                .insert(selected, Arc::downgrade(&state));
        }
        Ok(TorrentHandle { state })
    }
    async fn make_native_handle(
        &self,
        entry: Arc<Entry>,
        selected: u32,
        lease: Arc<PendingLease>,
        io_tasks: OwnedTasks,
    ) -> Result<TorrentHandle, String> {
        entry.users.fetch_add(1, Ordering::AcqRel);
        let state = Arc::new(HandleState {
            entry,
            manager: self.inner.clone(),
            selected,
            mode: LeaseMode::SelectedFile,
            owners: std::sync::Mutex::new(1),
            reads: Arc::new(TorrentReads::default()),
            stopped: AtomicBool::new(false),
            server: OnceCell::new(),
            io_tasks,
            native_lease: Some(lease),
        });
        let server =
            LoopbackStream::start_owned(Arc::downgrade(&state), state.io_tasks.child()).await?;
        state
            .server
            .set(server)
            .map_err(|_| "Torrent stream unavailable")?;
        Ok(TorrentHandle { state })
    }
    pub fn has_failed_settlement(&self) -> bool {
        self.inner.settlement_failed.load(Ordering::Acquire)
    }
    /// Pause idle torrents and remove expired entries. Under pressure only idle
    /// entries may be evicted; active inputs retain their reserved bytes.
    pub async fn reap(&self) -> Result<(), String> {
        self.reap_native().await?;
        let mut state = self.inner.state.lock().await;
        self.evict(&mut state, None).await
    }
    async fn evict(&self, state: &mut ManagerState, needed: Option<u64>) -> Result<(), String> {
        let mut idle: Vec<_> = state
            .entries
            .iter()
            .filter(|(_, e)| e.is_unused())
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect();
        idle.sort_by_key(|(_, e)| *e.idle_since.lock().unwrap_or_else(|e| e.into_inner()));
        for (key, entry) in idle {
            if entry.managed.has_failed_settlement() {
                self.inner.settlement_failed.store(true, Ordering::Release);
            }
            let _ = entry.session.pause(&entry.managed).await;
            if entry.managed.is_paused() {
                let _ = entry
                    .managed
                    .wait_for_paused_workers_until(
                        tokio::time::Instant::now() + Duration::from_secs(2),
                    )
                    .await;
            }
            if !entry.is_idle() {
                continue;
            }
            let expired = entry
                .idle_since
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .elapsed()
                >= self.inner.config.idle_timeout;
            let pressure = needed
                .is_some_and(|n| state.reserved.saturating_add(n) > self.inner.config.max_bytes);
            if expired || pressure {
                entry
                    .session
                    .delete(librqbit::api::TorrentIdOrHash::Id(entry.managed.id()), true)
                    .await
                    .map_err(|_| "Torrent eviction failed")?;
                if entry.folder.exists() {
                    std::fs::remove_dir_all(&entry.folder)
                        .map_err(|_| "Torrent eviction failed")?;
                }
                state.reserved -= entry.size;
                state.entries.remove(&key);
            }
        }
        let scopes: Vec<_> = state
            .scopes
            .keys()
            .filter(|scope| !state.entries.keys().any(|(s, _)| s == *scope))
            .cloned()
            .collect();
        // Do not remove the new scope while making its first reservation.
        if needed.is_none() {
            for scope in scopes {
                if let Some(session) = state.scopes.get(&scope) {
                    if session.has_failed_settlement() {
                        self.inner.settlement_failed.store(true, Ordering::Release);
                    }
                    if session
                        .shutdown_and_wait_until(
                            tokio::time::Instant::now() + Duration::from_secs(2),
                        )
                        .await
                    {
                        state.scopes.remove(&scope);
                    } else {
                        self.inner.settlement_failed.store(true, Ordering::Release);
                    }
                }
            }
        }
        if needed.is_some_and(|n| state.reserved.saturating_add(n) > self.inner.config.max_bytes) {
            return Err("Torrent cache capacity reached".into());
        }
        Ok(())
    }
}
impl Clone for TorrentHandle {
    fn clone(&self) -> Self {
        *self
            .state
            .owners
            .lock()
            .unwrap_or_else(|error| error.into_inner()) += 1;
        Self {
            state: self.state.clone(),
        }
    }
}
impl Drop for TorrentHandle {
    fn drop(&mut self) {
        let mut owners = self
            .state
            .owners
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *owners -= 1;
        if *owners == 0 {
            self.state.release();
        }
    }
}
/// Read-lifetime observation without retaining a public torrent owner. It lets
/// the control reaper wait for closed IO without keeping its listener alive.
#[derive(Clone)]
pub struct TorrentReadSettler(Arc<TorrentReads>);
#[derive(Default)]
struct TorrentReads {
    closed: AtomicBool,
    active: AtomicUsize,
    changed: Notify,
}
impl TorrentReadSettler {
    pub fn has_pending_reads(&self) -> bool {
        self.0.active.load(Ordering::Acquire) > 0
    }
    pub fn closed_reads_pending(&self) -> bool {
        self.0.closed.load(Ordering::Acquire) && self.has_pending_reads()
    }
    pub async fn settle_closed_reads(&self) {
        if !self.0.closed.load(Ordering::Acquire) {
            return;
        }
        loop {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.has_pending_reads() {
                return;
            }
            changed.await;
        }
    }
}
impl TorrentHandle {
    fn acquire(state: Arc<HandleState>) -> Option<Self> {
        let mut owners = state
            .owners
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.stopped.load(Ordering::Acquire) {
            return None;
        }
        *owners += 1;
        drop(owners);
        Some(Self { state })
    }
    pub fn read_settler(&self) -> TorrentReadSettler {
        TorrentReadSettler(self.state.reads.clone())
    }
    pub async fn settle_closed_reads(&self) {
        self.read_settler().settle_closed_reads().await;
    }
    /// Aliases can share this lease. Stop only if the caller is its final public
    /// owner; internal HTTP request references do not count as public owners.
    pub fn close_if_last_owner(&self) -> bool {
        let owners = self
            .state
            .owners
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *owners != 1 {
            return false;
        }
        self.state.release();
        true
    }
    pub fn files(&self) -> &[TorrentFile] {
        if self.state.mode == LeaseMode::SelectedFile {
            std::slice::from_ref(self.selected_file())
        } else {
            &self.state.entry.files
        }
    }
    pub fn selected_file(&self) -> &TorrentFile {
        &self.state.entry.files[self.state.selected as usize]
    }
    /// Vetted metadata cardinality, without exposing unselected names or lengths.
    pub fn metadata_file_count(&self) -> u32 {
        u32::try_from(self.state.entry.files.len()).unwrap_or(u32::MAX)
    }
    /// Private capability URL. Never send it to public clients or logs.
    pub fn stream_url(&self, index: u32) -> Result<String, String> {
        if self.state.stopped.load(Ordering::Acquire) || self.state.file(index).is_none() {
            return Err("Torrent stream unavailable".into());
        }
        self.state
            .server
            .get()
            .map(|s| s.url(index))
            .ok_or("Torrent stream unavailable".into())
    }
    pub async fn read_range(
        &self,
        index: u32,
        offset: u64,
        length: u32,
    ) -> Result<Vec<u8>, String> {
        self.state.read_range(index, offset, length).await
    }
    pub fn status(&self) -> TorrentStatus {
        let stats = self.state.entry.managed.stats();
        if self.is_strict() {
            let total_bytes = self.selected_file().size;
            let downloaded_bytes = stats
                .file_progress
                .get(self.state.selected as usize)
                .copied()
                .unwrap_or_default()
                .min(total_bytes);
            return TorrentStatus {
                downloaded_bytes,
                total_bytes,
                finished: downloaded_bytes == total_bytes,
                stopped: self.state.stopped.load(Ordering::Acquire),
            };
        }
        TorrentStatus {
            downloaded_bytes: stats.progress_bytes,
            total_bytes: stats.total_bytes,
            finished: stats.finished,
            stopped: self.state.stopped.load(Ordering::Acquire),
        }
    }
    /// Invalidates this lease and every explicit clone. Independent leases,
    /// including selected-only leases for the same file, remain active.
    /// Use stop_and_settle_until for a joined completion receipt.
    pub fn stop(&self) {
        self.state.release();
    }
    pub fn is_strict(&self) -> bool {
        self.state.native_lease.is_some()
    }
    pub async fn stop_and_settle_until(&self, deadline: tokio::time::Instant) -> Settlement {
        self.stop();
        let mut joined = self.state.io_tasks.cancel_and_wait_until(deadline).await;
        if let Some(server) = self.state.server.get() {
            joined &= server.stop_and_wait_until(deadline).await;
        }
        joined &= tokio::time::timeout_at(deadline, self.settle_closed_reads())
            .await
            .is_ok();
        if let Some(lease) = &self.state.native_lease {
            joined &= lease.settle_until(deadline).await;
        }
        joined &= !self.state.io_tasks.has_failed_settlement();
        if joined {
            Settlement::Settled
        } else {
            self.state
                .manager
                .settlement_failed
                .store(true, Ordering::Release);
            Settlement::SettlementFailed
        }
    }
}
impl HandleState {
    fn release(&self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        self.reads.closed.store(true, Ordering::Release);
        self.io_tasks.cancel();
        if let Some(server) = self.server.get() {
            server.stop();
        }
        if let Some(lease) = &self.native_lease {
            lease.release();
        }
        if self.entry.users.fetch_sub(1, Ordering::AcqRel) == 1 && self.native_lease.is_none() {
            mark_idle(&self.entry, &self.manager);
        }
    }

    pub(crate) fn file(&self, index: u32) -> Option<&TorrentFile> {
        if self.mode == LeaseMode::SelectedFile && index != self.selected {
            return None;
        }
        self.entry.files.get(index as usize)
    }
    pub(crate) async fn read_range(
        &self,
        index: u32,
        offset: u64,
        length: u32,
    ) -> Result<Vec<u8>, String> {
        let _operation = self.io_tasks.register().ok_or("Torrent stream stopped")?;
        if self.stopped.load(Ordering::Acquire) || length > MAX_RANGE_BYTES {
            return Err("Torrent stream unavailable".into());
        }
        self.entry.readers.fetch_add(1, Ordering::AcqRel);
        self.reads.active.fetch_add(1, Ordering::AcqRel);
        let _reader = TorrentReadGuard {
            reads: self.reads.clone(),
            entry: self.entry.clone(),
            manager: self.manager.clone(),
        };
        // Stop can race reader admission. No file is opened after that race;
        // an already-open reader keeps the reservation through cancellation.
        if self.stopped.load(Ordering::Acquire) {
            return Err("Torrent stream stopped".into());
        }
        let file = self.file(index).ok_or("Invalid torrent file index")?;
        if offset > file.size {
            return Err("Invalid torrent range".into());
        }
        let count = u64::from(length).min(file.size - offset) as usize;
        let work = self
            .io_tasks
            .track_future(async {
                let mut reader = self
                    .entry
                    .managed
                    .clone()
                    .stream(index as usize)
                    .map_err(|_| "Torrent stream unavailable")?;
                reader
                    .seek(std::io::SeekFrom::Start(offset))
                    .await
                    .map_err(|_| "Torrent seek failed")?;
                let mut bytes = vec![0; count];
                timeout(READ_TIMEOUT, reader.read_exact(&mut bytes))
                    .await
                    .map_err(|_| "Torrent read timed out")?
                    .map_err(|_| "Torrent read failed")?;
                if self.stopped.load(Ordering::Acquire) {
                    return Err("Torrent stream stopped".into());
                }
                Ok(bytes)
            })
            .ok_or("Torrent stream stopped")?;
        tokio::select! {
            biased;
            _ = self.io_tasks.cancellation_token().cancelled() => Err("Torrent stream stopped".into()),
            result = work => result,
        }
    }
}
impl Entry {
    fn is_unused(&self) -> bool {
        self.users.load(Ordering::Acquire) == 0 && self.readers.load(Ordering::Acquire) == 0
    }
    fn is_idle(&self) -> bool {
        self.is_unused() && !self.managed.has_pending_work()
    }
}
struct TorrentReadGuard {
    reads: Arc<TorrentReads>,
    entry: Arc<Entry>,
    manager: Arc<ManagerInner>,
}
impl Drop for TorrentReadGuard {
    fn drop(&mut self) {
        if self.entry.readers.fetch_sub(1, Ordering::AcqRel) == 1 {
            mark_idle(&self.entry, &self.manager);
        }
        if self.reads.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.reads.changed.notify_waiters();
        }
    }
}
fn mark_idle(entry: &Arc<Entry>, manager: &Arc<ManagerInner>) {
    if manager.network_policy.is_native() {
        return;
    }
    if !entry.is_unused() {
        return;
    }
    *entry
        .idle_since
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Instant::now();
    if let Some(runtime) = manager.runtime.get() {
        let entry = entry.clone();
        let manager = manager.clone();
        runtime.spawn(async move {
            let _guard = manager.state.lock().await;
            if entry.is_unused() {
                let _ = entry.session.pause(&entry.managed).await;
            }
        });
    }
}
impl Drop for HandleState {
    fn drop(&mut self) {
        self.release();
    }
}
/// Validate URI structure without resolving or fetching any URLs. HTTP tracker
/// fetching and source-provided peers/alternate webseed sources are disabled.
pub fn validate_magnet(source: &str) -> Result<(), String> {
    if source.len() > 16 * 1024 {
        return Err("Invalid magnet URI".into());
    }
    let url = url::Url::parse(source).map_err(|_| "Invalid magnet URI")?;
    if url.scheme() != "magnet"
        || !url.path().is_empty()
        || url.fragment().is_some()
        || url.host_str().is_some()
    {
        return Err("Invalid magnet URI".into());
    }
    let mut hash = false;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "xt" => {
                if hash || !value.starts_with("urn:btih:") {
                    return Err("Unsupported magnet hash".into());
                }
                let h = &value[9..];
                if !(h.len() == 40 && h.chars().all(|c| c.is_ascii_hexdigit())
                    || h.len() == 32
                        && h.chars()
                            .all(|c| c.is_ascii_alphabetic() || ('2'..='7').contains(&c)))
                {
                    return Err("Unsupported magnet hash".into());
                }
                hash = true;
            }
            "tr" => {
                let u = url::Url::parse(&value).map_err(|_| "Invalid magnet tracker")?;
                if !matches!(u.scheme(), "http" | "https" | "udp")
                    || u.host_str().is_none()
                    || !u.username().is_empty()
                    || u.password().is_some()
                    || u.fragment().is_some()
                {
                    return Err("Invalid magnet tracker".into());
                }
            }
            "dn" => {}
            _ => return Err("Unsupported magnet parameter".into()),
        }
    }
    if !hash {
        return Err("Unsupported magnet hash".into());
    }
    librqbit::Magnet::parse(source).map_err(|_| "Invalid magnet URI")?;
    Ok(())
}
fn validate_metainfo(bytes: &[u8]) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() > MAX_METAINFO_BYTES {
        return Err("Invalid torrent metadata".into());
    }
    #[derive(Deserialize)]
    struct Extras {
        #[serde(rename = "url-list")]
        webseed: Option<serde::de::IgnoredAny>,
        #[serde(rename = "httpseeds")]
        legacy_webseed: Option<serde::de::IgnoredAny>,
    }
    let extras: Extras = bencode::from_bytes(bytes).map_err(|_| "Invalid torrent metadata")?;
    if extras.webseed.is_some() || extras.legacy_webseed.is_some() {
        return Err("Torrent webseeds unsupported".into());
    }
    let meta = torrent_from_bytes::<ByteBufOwned>(bytes).map_err(|_| "Invalid torrent metadata")?;
    if meta.info.private {
        return Err("Private tracker torrents unsupported".into());
    }
    describe_files(&meta.info)?;
    Ok(())
}
fn describe_files(info: &TorrentMetaV1Info<ByteBufOwned>) -> Result<Vec<TorrentFile>, String> {
    let details = info
        .iter_file_details()
        .map_err(|_| "Invalid torrent file list")?;
    let mut files = Vec::new();
    for (index, file) in details.enumerate() {
        if index >= 4096 || file.attrs().symlink || file.symlink_path.is_some() {
            return Err("Unsupported torrent file list".into());
        }
        let components = file
            .filename
            .to_vec()
            .map_err(|_| "Invalid torrent filename")?;
        if components.iter().any(|c| {
            c.is_empty()
                || c == "."
                || c == ".."
                || c.contains(':')
                || c.chars().any(char::is_control)
        }) {
            return Err("Invalid torrent filename".into());
        }
        files.push(TorrentFile {
            index: index as u32,
            name: components.join("/"),
            size: file.len,
        });
    }
    if files.is_empty() {
        return Err("Invalid torrent file list".into());
    }
    Ok(files)
}
fn select_file(files: &[TorrentFile], index: Option<u32>) -> Result<u32, String> {
    if let Some(index) = index {
        return files
            .get(index as usize)
            .map(|f| f.index)
            .ok_or("Invalid torrent file index".into());
    }
    files
        .iter()
        .filter(|f| is_media_name(&f.name))
        .max_by_key(|f| f.size)
        .map(|f| f.index)
        .ok_or("No supported media file in torrent".into())
}
pub fn is_media_name(name: &str) -> bool {
    name.rsplit('.').next().is_some_and(|ext| {
        matches!(
            ext.to_ascii_lowercase().as_str(),
            "mkv"
                | "mp4"
                | "m4v"
                | "avi"
                | "mov"
                | "webm"
                | "ts"
                | "m2ts"
                | "mpg"
                | "mpeg"
                | "flv"
                | "wmv"
                | "mp3"
                | "flac"
                | "m4a"
                | "aac"
                | "ogg"
                | "wav"
                | "rar"
        )
    })
}
fn private_dir(path: &std::path::Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| "Torrent cache permissions unavailable")?;
    }
    Ok(())
}
fn prepare_cache(path: &std::path::Path) -> Result<std::fs::File, String> {
    if path.exists() {
        let m = std::fs::symlink_metadata(path).map_err(|_| "Torrent cache unavailable")?;
        if !m.is_dir() || m.file_type().is_symlink() {
            return Err("Unsafe torrent cache directory".into());
        }
    }
    std::fs::create_dir_all(path).map_err(|_| "Torrent cache unavailable")?;
    private_dir(path)?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options
        .open(path.join(".gateway-lock"))
        .map_err(|_| "Torrent cache lock unavailable")?;
    lock_cache_file(&lock)?;
    // Only gateway-owned cache folders may be stale after a previous crash.
    for e in std::fs::read_dir(path).map_err(|_| "Torrent cache unavailable")? {
        let e = e.map_err(|_| "Torrent cache unavailable")?;
        if e.file_name().to_string_lossy().starts_with("torrent-") {
            let m = e.file_type().map_err(|_| "Torrent cache unavailable")?;
            if m.is_symlink() || !m.is_dir() {
                return Err("Unsafe torrent cache entry".into());
            }
            std::fs::remove_dir_all(e.path()).map_err(|_| "Torrent cache cleanup failed")?;
        }
    }
    Ok(lock)
}

fn lock_cache_file(file: &std::fs::File) -> Result<(), String> {
    // Android's std File::try_lock returns Unsupported. Keep its native
    // nonblocking flock tied to this file descriptor's existing owner lifetime.
    #[cfg(target_os = "android")]
    let result = lock_android_cache_file(file).map_err(|_| ());
    #[cfg(not(target_os = "android"))]
    let result = file.try_lock().map_err(|_| ());
    result.map_err(|_| "Torrent cache already in use".to_owned())
}

#[cfg(any(target_os = "android", all(test, unix)))]
fn lock_android_cache_file(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: file owns a valid descriptor for the entire call. flock neither
    // consumes it nor accesses Rust memory; closing its last alias releases it.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(all(test, unix))]
mod android_cache_lock_tests {
    use super::lock_android_cache_file;
    use std::fs::OpenOptions;

    #[test]
    fn android_cache_lock_is_exclusive_nonblocking_and_released_with_owner() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("cache-lock");
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let competing = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        lock_android_cache_file(&owner).unwrap();
        assert_eq!(
            lock_android_cache_file(&competing).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let alias = owner.try_clone().unwrap();
        drop(owner);
        assert_eq!(
            lock_android_cache_file(&competing).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(alias);
        lock_android_cache_file(&competing).unwrap();
    }
}

/// A bounded spool reservation in the same budget as torrent payloads.
pub struct TorrentCacheReservation {
    _cache_lock: Arc<std::fs::File>,
    manager: Arc<ManagerInner>,
    bytes: u64,
    folder: PathBuf,
}
impl TorrentCacheReservation {
    pub fn path(&self) -> &std::path::Path {
        &self.folder
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}
impl Drop for TorrentCacheReservation {
    fn drop(&mut self) {
        let manager = self.manager.clone();
        let folder = self.folder.clone();
        let bytes = self.bytes;
        if let Some(runtime) = manager.runtime.get().cloned() {
            runtime.spawn(async move {
                if std::fs::remove_dir_all(folder).is_ok() {
                    {
                        let mut state = manager.state.lock().await;
                        state.reserved = state.reserved.saturating_sub(bytes);
                    }
                }
            });
        }
    }
}
fn strip_trackers(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let parsed =
        torrent_from_bytes_ext::<ByteBufOwned>(bytes).map_err(|_| "Invalid torrent metadata")?;
    let mut out = b"d4:info".to_vec();
    out.extend_from_slice(parsed.info_bytes.as_ref());
    out.push(b'e');
    Ok(out)
}

impl std::fmt::Debug for TorrentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TorrentConfig(<redacted>)")
    }
}

impl Drop for ManagerInner {
    fn drop(&mut self) {
        for session in self.state.get_mut().scopes.values() {
            session.cancellation_token().cancel();
        }
    }
}

#[cfg(test)]
mod lifetime_tests;

fn hex_hash(hash: [u8; 20]) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}
