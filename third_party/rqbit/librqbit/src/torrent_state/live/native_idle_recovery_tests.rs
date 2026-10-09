// SPDX-License-Identifier: Apache-2.0
// Real peer/requester regression with an owned stalled seeder.
use crate::network_policy::NetworkPolicy;
use crate::{
    storage::{
        filesystem::FilesystemStorageFactory, BoxStorageFactory, StorageFactory, StorageFactoryExt,
        TorrentStorage,
    },
    AddTorrent, AddTorrentOptions, ManagedTorrentShared, Session, SessionOptions, TorrentMetadata,
};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Duration,
};
use tokio::{io::AsyncReadExt, time::timeout};

#[derive(Default)]
struct Gate {
    armed: AtomicBool,
    block_from: AtomicU64,
    paced: AtomicBool,
    reads: Mutex<Vec<u64>>,
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
    inner: Box<dyn TorrentStorage>,
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
            inner: Box::new(FilesystemStorageFactory::default().create(shared, metadata)?),
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
        if self.gate.armed.load(Ordering::Acquire) {
            self.gate.reads.lock().unwrap().push(offset);
        }
        if self.gate.armed.load(Ordering::Acquire)
            && self.gate.paced.load(Ordering::Acquire)
            && offset < 64 * 1024
        {
            self.gate.active.store(true, Ordering::Release);
            std::thread::sleep(Duration::from_millis(900));
            return self.inner.pread_exact(id, offset, buf);
        }
        if self.gate.armed.load(Ordering::Acquire)
            && offset >= self.gate.block_from.load(Ordering::Acquire)
            && offset < 64 * 1024
        {
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
        Ok(Box::new(Self {
            inner: self.inner.take()?,
            gate: self.gate.clone(),
        }))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_idle_peer_recovers_decoder_piece_before_socket_expiry() {
    recover_from_owned_peer(false, false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_takeover_requests_only_missing_chunks_before_hash_verification() {
    recover_from_owned_peer(true, false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_retired_peer_cannot_reset_successor_partial_chunks() {
    recover_from_owned_peer(true, true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_progressing_owner_is_not_replaced_on_total_piece_age() {
    recover_from_owned_peer(false, false, true).await;
}

async fn recover_from_owned_peer(partial: bool, retire_previous: bool, progressing: bool) {
    let seed = tempfile::tempdir().unwrap();
    let source = seed.path().join("owned.bin");
    let bytes: Vec<u8> = (0..2 * 1024 * 1024 - 1).map(|n| (n % 251) as u8).collect();
    std::fs::write(&source, &bytes).unwrap();
    let metainfo = crate::create_torrent(
        &source,
        crate::CreateTorrentOptions {
            piece_length: Some(64 * 1024),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .as_bytes()
    .unwrap()
    .to_vec();
    let vetted = crate::native_metainfo::vet_native_metainfo(&metainfo).unwrap();
    let selection = crate::native_metainfo::NativeSelection::from_hex(
        &vetted.info_hash_hex(),
        0,
        bytes.len() as u64,
    )
    .unwrap();
    let mut sessions = Vec::new();
    let mut peers = Vec::new();
    let gate = Arc::new(Gate::default());
    let fast_gate = Arc::new(Gate::default());
    gate.paced.store(progressing, Ordering::Release);
    fast_gate.block_from.store(
        if retire_previous { 48 * 1024 } else { u64::MAX },
        Ordering::Release,
    );
    let _release_fast = ReleaseOnDrop(fast_gate.clone());
    if partial {
        gate.block_from.store(32 * 1024, Ordering::Release);
    }
    let _release = ReleaseOnDrop(gate.clone());
    for slow in [true, false] {
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = socket.local_addr().unwrap();
        drop(socket);
        let session = Session::new_with_opts(
            seed.path().to_owned(),
            SessionOptions {
                peer_id: Some(crate::tests::test_util::TestPeerMetadata::good().as_peer_id()),
                disable_dht: true,
                disable_dht_persistence: true,
                disable_trackers: true,
                enable_upnp_port_forwarding: false,
                listen_port_range: Some(peer.port()..peer.port() + 1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let handle = session
            .add_torrent(
                AddTorrent::from_bytes(metainfo.clone()),
                Some(AddTorrentOptions {
                    overwrite: true,
                    output_folder: Some(seed.path().to_str().unwrap().into()),
                    disable_trackers: true,
                    storage_factory: Some(
                        DelayedFactory(if slow {
                            gate.clone()
                        } else {
                            fast_gate.clone()
                        })
                        .boxed(),
                    ),
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();
        handle.wait_until_initialized().await.unwrap();
        // An incomplete fixture peer stays connected through interest negotiation.
        // Only the first piece is read; the final advertised piece is withheld.
        let live = handle.live().unwrap();
        live.lock_write("owned_incomplete_peer")
            .get_chunks_mut()
            .unwrap()
            .evict_piece(live.lengths.last_piece_id(), &live.metadata.file_infos);
        sessions.push(session);
        peers.push(peer);
    }
    gate.armed.store(true, Ordering::Release);
    fast_gate.armed.store(true, Ordering::Release);
    let cache = tempfile::tempdir().unwrap();
    let consumer = Session::new_with_opts(
        cache.path().to_owned(),
        SessionOptions {
            network_policy: NetworkPolicy::OwnedPrivateTcp(peers.clone()),
            disable_dht: true,
            disable_dht_persistence: true,
            disable_trackers: true,
            listen_port_range: None,
            enable_upnp_port_forwarding: false,
            peer_opts: Some(crate::PeerConnectionOptions {
                connect_timeout: Some(Duration::from_secs(2)),
                read_write_timeout: Some(Duration::from_secs(5)),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let handle = consumer
        .add_torrent(
            AddTorrent::from_bytes(metainfo),
            Some(AddTorrentOptions {
                native_selection: Some(selection),
                only_files: Some(vec![0]),
                disable_trackers: true,
                initial_peers: Some(vec![peers[0]]),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap();
    handle.wait_until_initialized().await.unwrap();
    let reader_handle = handle.clone();
    let reading = tokio::spawn(async move {
        let mut reader = reader_handle.stream(0).unwrap();
        let mut first = [0; 16];
        reader.read_exact(&mut first).await.map(|_| first)
    });
    timeout(Duration::from_secs(2), async {
        while !gate.active.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("slow seeder never received the decoder piece request");
    timeout(Duration::from_secs(2), async {
        loop {
            let live = handle.live().unwrap();
            let g = live.lock_read("owned_work_reserved");
            let all_reserved = live.lengths.iter_piece_infos().take(31).all(|piece| {
                g.inflight_pieces.contains_key(&piece.piece_index)
                    || g.get_chunks().unwrap().is_piece_have(piece.piece_index)
            });
            drop(g);
            if all_reserved {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("slow peer did not reserve the fixture before recovery");
    if partial {
        timeout(Duration::from_secs(2), async {
            loop {
                let live = handle.live().unwrap();
                let chunks: Vec<_> = live
                    .lengths
                    .iter_chunk_infos(live.lengths.validate_piece_index(0).unwrap())
                    .take(2)
                    .collect();
                if chunks.iter().all(|chunk| {
                    live.lock_read("owned_received_prefix")
                        .get_chunks()
                        .unwrap()
                        .is_chunk_received(chunk)
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("slow peer prefix never reached the actual chunk tracker");
    }
    assert!(
        !reading.is_finished(),
        "unverified cached chunks cannot reach a reader"
    );
    // All available work is reserved or complete before introducing the fast
    // peer, so recovery cannot rely on a new lookahead transfer completing.
    handle
        .live()
        .unwrap()
        .add_peer_if_not_seen(peers[1])
        .unwrap();
    let successor_kept_prefix = if retire_previous {
        timeout(Duration::from_millis(3800), async {
            while !fast_gate.active.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            loop {
                let live = handle.live().unwrap();
                let chunks: Vec<_> = live
                    .lengths
                    .iter_chunk_infos(live.lengths.validate_piece_index(0).unwrap())
                    .take(2)
                    .collect();
                if chunks.iter().all(|chunk| {
                    live.lock_read("owned_successor_prefix")
                        .get_chunks()
                        .unwrap()
                        .is_chunk_received(chunk)
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("successor did not retain its received prefix before the forced old-peer close");
        let live = handle.live().unwrap();
        live.peers
            .with_live_mut(peers[0], "owned_retire_previous", |peer| {
                peer.tx
                    .send(super::WriterRequest::Disconnect(Ok(())))
                    .unwrap();
            })
            .expect("previous peer disappeared before the fixture retired it");
        timeout(Duration::from_secs(1), async {
            while live
                .peers
                .with_live_mut(peers[0], "owned_previous_closed", |_| ())
                .is_some()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("previous peer did not close");
        let chunks: Vec<_> = live
            .lengths
            .iter_chunk_infos(live.lengths.validate_piece_index(0).unwrap())
            .take(2)
            .collect();
        let kept = chunks.iter().all(|chunk| {
            live.lock_read("owned_retained_prefix")
                .get_chunks()
                .unwrap()
                .is_chunk_received(chunk)
        });
        fast_gate.release();
        kept
    } else {
        true
    };
    let result = timeout(Duration::from_millis(3800), reading).await;
    gate.release();
    let steals = handle
        .live()
        .unwrap()
        .peers
        .stats
        .steals
        .load(Ordering::Relaxed);
    assert_eq!(
        consumer
            .shutdown_and_settle_until(tokio::time::Instant::now() + Duration::from_secs(2))
            .await,
        crate::Settlement::Settled
    );
    for session in sessions {
        assert_eq!(
            session
                .shutdown_and_settle_until(tokio::time::Instant::now() + Duration::from_secs(2))
                .await,
            crate::Settlement::Settled
        );
    }
    let first = result
        .expect("idle peer waited until its socket expired instead of recovering the decoder piece")
        .unwrap()
        .unwrap();
    assert!(
        successor_kept_prefix,
        "retired peer invalidated chunks now held by its successor"
    );
    assert_eq!(first.as_slice(), &bytes[..16]);
    if progressing {
        assert_eq!(steals, 0, "a peer delivering new chunks was replaced solely because the whole piece took two seconds");
    }
    if partial {
        let mut reads: Vec<_> = fast_gate
            .reads
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|offset| *offset < 64 * 1024)
            .collect();
        reads.sort_unstable();
        assert_eq!(
            reads,
            vec![32 * 1024, 48 * 1024],
            "takeover must not request the prefix already on disk"
        );
    }
}
