use super::*;
use librqbit::{
    storage::{
        filesystem::{FilesystemStorage, FilesystemStorageFactory},
        BoxStorageFactory, StorageFactory, StorageFactoryExt, TorrentStorage,
    },
    AddTorrent, AddTorrentOptions, ManagedTorrentShared, Session, SessionOptions, TorrentMetadata,
};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Duration,
};

#[derive(Default)]
struct Gate {
    entered: AtomicBool,
    active: AtomicBool,
    released: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct ReleaseOnDrop(Arc<Gate>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct DelayedFactory(Arc<Gate>);
struct DelayedStorage {
    inner: FilesystemStorage,
    gate: Arc<Gate>,
}
impl StorageFactory for DelayedFactory {
    type Storage = DelayedStorage;
    fn create(
        &self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<Self::Storage> {
        Ok(DelayedStorage {
            inner: FilesystemStorageFactory::default().create(shared, metadata)?,
            gate: self.0.clone(),
        })
    }
    fn clone_box(&self) -> BoxStorageFactory {
        DelayedFactory(self.0.clone()).boxed()
    }
}
impl TorrentStorage for DelayedStorage {
    fn init(
        &mut self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        self.inner.init(shared, metadata)
    }
    fn pread_exact(&self, id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        if !self.gate.entered.swap(true, Ordering::AcqRel) {
            self.gate.active.store(true, Ordering::Release);
            let released = self.gate.released.lock().unwrap();
            // Finite escape also covers a panicking probe; this is an IO-delay
            // injection over the real filesystem, not replacement torrent logic.
            let _ = self
                .gate
                .changed
                .wait_timeout_while(released, Duration::from_secs(10), |r| !*r)
                .unwrap();
            let result = self.inner.pread_exact(id, offset, buf);
            self.gate.active.store(false, Ordering::Release);
            return result;
        }
        self.inner.pread_exact(id, offset, buf)
    }
    fn pwrite_all(&self, id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        self.inner.pwrite_all(id, offset, buf)
    }
    fn remove_file(&self, id: usize, filename: &Path) -> anyhow::Result<()> {
        self.inner.remove_file(id, filename)
    }
    fn remove_directory_if_empty(&self, path: &Path) -> anyhow::Result<()> {
        self.inner.remove_directory_if_empty(path)
    }
    fn ensure_file_length(&self, id: usize, length: u64) -> anyhow::Result<()> {
        self.inner.ensure_file_length(id, length)
    }
    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        self.inner.take()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_initialization_keeps_cache_charged_until_real_worker_join() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache");
    let folder = cache.join("torrent-owned-fixture");
    let manager = TorrentManager::new(TorrentConfig {
        enabled: true,
        cache_dir: cache.clone(),
        enable_dht: false,
        idle_timeout: Duration::ZERO,
        ..Default::default()
    })
    .unwrap();
    let lock = Arc::new(prepare_cache(&cache).unwrap());
    manager.inner.cache_lock.set(lock.clone()).unwrap();
    manager
        .inner
        .runtime
        .set(tokio::runtime::Handle::current())
        .unwrap();
    std::fs::create_dir(&folder).unwrap();
    let source = folder.join("owned.mkv");
    std::fs::write(&source, vec![7; 64_000]).unwrap();
    let bytes = librqbit::create_torrent(
        &source,
        librqbit::CreateTorrentOptions {
            piece_length: Some(16 * 1024),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .as_bytes()
    .unwrap();
    let session = Session::new_with_opts(
        cache.clone(),
        SessionOptions {
            disable_dht: true,
            disable_dht_persistence: true,
            listen_port_range: None,
            enable_upnp_port_forwarding: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let gate = Arc::new(Gate::default());
    let _release = ReleaseOnDrop(gate.clone());
    let managed = session
        .add_torrent(
            AddTorrent::from_bytes(bytes.clone()),
            Some(AddTorrentOptions {
                paused: true,
                overwrite: true,
                disable_trackers: true,
                output_folder: Some(folder.to_str().unwrap().into()),
                storage_factory: Some(DelayedFactory(gate.clone()).boxed()),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap();
    timeout(Duration::from_secs(3), async {
        while !gate.active.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    let key = ("owned-fixture".to_string(), managed.info_hash().as_string());
    let entry = Arc::new(Entry {
        _cache_lock: lock,
        session: session.clone(),
        managed: managed.clone(),
        files: vec![TorrentFile {
            index: 0,
            name: "owned.mkv".into(),
            size: 64_000,
        }],
        folder: folder.clone(),
        size: 64_000,
        users: AtomicUsize::new(0),
        readers: AtomicUsize::new(0),
        idle_since: std::sync::Mutex::new(std::time::Instant::now()),
        handles: std::sync::Mutex::new(HashMap::new()),
    });
    {
        let mut state = manager.inner.state.lock().await;
        state.scopes.insert(key.0.clone(), session.clone());
        state.entries.insert(key, entry.clone());
        state.reserved = 64_000;
    }
    let cancelling_at = tokio::time::Instant::now();
    assert_eq!(
        managed
            .stop_and_settle_until(cancelling_at + Duration::from_secs(2))
            .await,
        librqbit::Settlement::SettlementFailed
    );
    assert!(cancelling_at.elapsed() >= Duration::from_secs(2));
    assert!(gate.active.load(Ordering::Acquire));
    assert!(managed.has_failed_settlement());
    assert!(session.has_failed_settlement());
    assert!(managed.has_pending_work());
    assert!(
        !entry.is_idle(),
        "initialization work cannot be acknowledged as idle"
    );
    manager.reap().await.unwrap();
    assert_eq!(manager.inner.state.lock().await.reserved, 64_000);
    assert!(folder.exists());
    // Refusing new admission must not depend on deleting the old cache entry.
    assert_eq!(
        manager
            .start_selected_metainfo("new-fixture", bytes.to_vec(), 0)
            .await
            .unwrap_err(),
        "Torrent settlement failed"
    );
    gate.release();
    assert!(
        managed
            .stop_and_wait_until(tokio::time::Instant::now() + Duration::from_secs(2))
            .await
    );
    assert!(!managed.has_pending_work());
    manager.reap().await.unwrap();
    assert_eq!(manager.inner.state.lock().await.reserved, 0);
    assert!(!folder.exists());
    assert_eq!(
        manager
            .start_selected_metainfo("new-fixture", bytes.to_vec(), 0)
            .await
            .unwrap_err(),
        "Torrent settlement failed"
    );
    assert!(
        session
            .shutdown_and_wait_until(tokio::time::Instant::now() + Duration::from_secs(2))
            .await
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dht_payload_cancellation_joins_queries_without_retiring_sibling_or_root() {
    // An owned, deliberately silent bootstrap socket keeps the real DHT root
    // alive until cancellation. An empty inventory makes bootstrap fail and
    // drops the socket before this test reaches its shutdown assertion.
    let bootstrap = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let dht = librqbit::dht::DhtBuilder::with_config(librqbit::dht::DhtConfig {
        listen_addr: Some("127.0.0.1:0".parse().unwrap()),
        bootstrap_addrs: Some(vec![bootstrap.local_addr().unwrap().to_string()]),
        ..Default::default()
    })
    .await
    .unwrap();
    let root = librqbit::OwnedTasks::new(Default::default());
    let outgoing = root.child();
    let candidate = root.child();
    let outgoing_stream =
        dht.get_peers_owned(librqbit::dht::Id20::new([1; 20]), None, outgoing.clone());
    let candidate_stream =
        dht.get_peers_owned(librqbit::dht::Id20::new([2; 20]), None, candidate.clone());
    // Keep both streams borrowed: cancellation must be observed by the worker,
    // rather than relying on an embedder dropping the stream first.
    let started = tokio::time::Instant::now();
    assert_eq!(
        candidate
            .cancel_and_settle_until(started + Duration::from_secs(2))
            .await,
        librqbit::Settlement::Settled
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(!candidate.has_pending_work());
    assert!(outgoing.has_pending_work());
    assert!(!outgoing.cancellation_token().is_cancelled());
    assert!(
        outgoing_stream
            .cancel_and_wait_until(tokio::time::Instant::now() + Duration::from_secs(2))
            .await
    );
    drop((candidate_stream, outgoing_stream));
    assert!(
        root.cancel_and_wait_until(tokio::time::Instant::now() + Duration::from_secs(2))
            .await
    );
    let bound_addr = dht.listen_addr();
    #[cfg(target_os = "linux")]
    let socket_inode = {
        let endpoint = format!("0100007F:{:04X}", bound_addr.port());
        std::fs::read_to_string("/proc/net/udp")
            .unwrap()
            .lines()
            .find_map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                (fields.get(1) == Some(&endpoint.as_str())).then(|| fields[9].to_owned())
            })
            .expect("owned DHT socket must exist before shutdown")
    };
    assert!(
        dht.shutdown_and_wait_until(tokio::time::Instant::now() + Duration::from_secs(2))
            .await
    );
    // The Arc remains alive; only joined worker disposal can free this socket.
    // Check this process's exact socket descriptor on Linux. The workspace's
    // parallel child-process fixtures can transiently inherit a descriptor
    // between fork and exec, so the network-namespace table/rebinding measures
    // those unrelated children as well as this owner.
    #[cfg(target_os = "linux")]
    assert!(!std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_link(entry.path()).ok())
        .any(|target| target.to_string_lossy() == format!("socket:[{socket_inode}]")));
    #[cfg(not(target_os = "linux"))]
    {
        let rebound = tokio::net::UdpSocket::bind(bound_addr).await.unwrap();
        drop(rebound);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_shutdown_cancels_unreturned_metadata_admission() {
    let tmp = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let session = Session::new_with_opts(
        tmp.path().to_owned(),
        SessionOptions {
            disable_dht: true,
            disable_dht_persistence: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let waiter = tokio::spawn({
        let session = session.clone();
        let peer = listener.local_addr().unwrap();
        async move {
            session
                .add_torrent(
                    AddTorrent::from_url(
                        "magnet:?xt=urn:btih:1111111111111111111111111111111111111111",
                    ),
                    Some(AddTorrentOptions {
                        initial_peers: Some(vec![peer]),
                        disable_trackers: true,
                        ..Default::default()
                    }),
                )
                .await
        }
    });
    let (_socket, _) = timeout(Duration::from_secs(2), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let started = tokio::time::Instant::now();
    assert_eq!(
        session
            .shutdown_and_settle_until(started + Duration::from_secs(2))
            .await,
        librqbit::Settlement::Settled
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(waiter.await.unwrap().is_err());
    assert!(!session.has_pending_work());
    assert!(!session.has_failed_settlement());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_live_payload_keeps_another_real_filesystem_payload_readable() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::new_with_opts(
        tmp.path().to_owned(),
        SessionOptions {
            disable_dht: true,
            disable_dht_persistence: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let mut payloads = Vec::new();
    for index in 0..2 {
        let folder = tmp.path().join(format!("owned-{index}"));
        std::fs::create_dir(&folder).unwrap();
        let source = folder.join("owned.mkv");
        std::fs::write(&source, vec![index as u8 + 7; 64_000]).unwrap();
        let bytes = librqbit::create_torrent(
            &source,
            librqbit::CreateTorrentOptions {
                piece_length: Some(16 * 1024),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .as_bytes()
        .unwrap();
        let managed = session
            .add_torrent(
                AddTorrent::from_bytes(bytes),
                Some(AddTorrentOptions {
                    overwrite: true,
                    disable_trackers: true,
                    output_folder: Some(folder.to_str().unwrap().into()),
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();
        timeout(Duration::from_secs(2), managed.wait_until_initialized())
            .await
            .unwrap()
            .unwrap();
        payloads.push(managed);
    }
    assert!(payloads[0].live().is_some());
    assert!(payloads[1].live().is_some());
    let started = tokio::time::Instant::now();
    assert_eq!(
        payloads[0]
            .stop_and_settle_until(started + Duration::from_secs(2))
            .await,
        librqbit::Settlement::Settled
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(payloads[1].live().is_some());
    let mut reader = payloads[1].clone().stream(0).unwrap();
    let mut bytes = vec![0; 64_000];
    timeout(Duration::from_secs(2), reader.read_exact(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bytes, vec![8; 64_000]);
    drop(reader);
    assert_eq!(
        session
            .shutdown_and_settle_until(tokio::time::Instant::now() + Duration::from_secs(2))
            .await,
        librqbit::Settlement::Settled
    );
}

#[cfg(feature = "test-network-policy")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_metadata_peer_is_retained_for_payload_without_second_discovery() {
    let seed = tempfile::tempdir().unwrap();
    let bytes: Vec<u8> = (0..65536).map(|n| (n % 251) as u8).collect();
    let source = seed.path().join("video.mkv");
    std::fs::write(&source, &bytes).unwrap();
    let metainfo = librqbit::create_torrent(&source, Default::default())
        .await
        .unwrap()
        .as_bytes()
        .unwrap()
        .to_vec();
    let metadata = vet_native_metainfo(&metainfo, None, 0, Some(bytes.len() as u64)).unwrap();
    let hash = metadata.info_hash_hex();
    let selection =
        librqbit::native_metainfo::NativeSelection::from_hex(&hash, 0, bytes.len() as u64).unwrap();
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    let seeder = Session::new_with_opts(
        seed.path().to_owned(),
        SessionOptions {
            disable_dht: true,
            disable_dht_persistence: true,
            disable_trackers: true,
            enable_upnp_port_forwarding: false,
            listen_port_range: Some(port..port + 1),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    seeder
        .add_torrent(
            AddTorrent::from_bytes(metainfo),
            Some(AddTorrentOptions {
                output_folder: Some(seed.path().to_str().unwrap().into()),
                overwrite: true,
                disable_trackers: true,
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap()
        .wait_until_initialized()
        .await
        .unwrap();
    let cache = tempfile::tempdir().unwrap();
    let peer: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let consumer = Session::new_with_opts(
        cache.path().to_owned(),
        SessionOptions {
            network_policy: NetworkPolicy::OwnedPrivateTcp(vec![peer]),
            disable_dht: true,
            disable_dht_persistence: true,
            disable_trackers: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let listed = consumer
        .add_torrent(
            AddTorrent::from_url(format!("magnet:?xt=urn:btih:{hash}")),
            Some(AddTorrentOptions {
                list_only: true,
                native_selection: Some(selection),
                only_files: Some(vec![0]),
                initial_peers: Some(vec![peer]),
                disable_trackers: true,
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    let librqbit::AddTorrentResponse::ListOnly(listed) = listed else {
        panic!("metadata-only preparation required")
    };
    assert!(listed.seen_peers.contains(&peer));
    // Payload receives no new bootstrap peers, DHT or trackers. It must retain
    // only the metadata-stage peer proven by this session for this exact hash.
    let handle = consumer
        .add_torrent(
            AddTorrent::from_bytes(listed.torrent_bytes),
            Some(AddTorrentOptions {
                native_selection: Some(selection),
                only_files: Some(vec![0]),
                disable_trackers: true,
                initial_peers: Some(vec![]),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap();
    handle.wait_until_initialized().await.unwrap();
    let mut reader = handle.clone().stream(0).unwrap();
    let mut first = vec![0; 128];
    let result = timeout(Duration::from_secs(2), reader.read_exact(&mut first)).await;
    drop(reader);
    assert_eq!(
        consumer
            .shutdown_and_settle_until(tokio::time::Instant::now() + Duration::from_secs(2))
            .await,
        librqbit::Settlement::Settled
    );
    assert_eq!(
        seeder
            .shutdown_and_settle_until(tokio::time::Instant::now() + Duration::from_secs(2))
            .await,
        librqbit::Settlement::Settled
    );
    result
        .expect("payload discarded the working metadata peer and waited for rediscovery")
        .unwrap();
    assert_eq!(first, bytes[..128]);
}
