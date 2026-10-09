// SPDX-License-Identifier: Apache-2.0
use super::*;
use crate::{AddTorrent, AddTorrentOptions, CreateTorrentOptions, Session, SessionOptions};
use librqbit_core::network_policy::NetworkPolicy;
use std::{str::FromStr, time::Duration};
use tokio::net::{TcpListener, TcpStream};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_rediscovered_peer_can_recover_after_a_dropped_handshake() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("owned.bin");
    std::fs::write(&source, vec![7; 1024 * 1024 - 1]).unwrap();
    let metainfo = crate::create_torrent(
        &source,
        CreateTorrentOptions {
            piece_length: Some(64 * 1024),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .as_bytes()
    .unwrap()
    .to_vec();
    let hash = Id20::from_str(
        &crate::native_metainfo::vet_native_metainfo(&metainfo)
            .unwrap()
            .info_hash_hex(),
    )
    .unwrap();
    let reserved = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let seed_addr = reserved.local_addr().unwrap();
    drop(reserved);
    let seed_peer_id = crate::tests::test_util::TestPeerMetadata::good().as_peer_id();
    let seed = Session::new_with_opts(
        root.path().to_owned(),
        SessionOptions {
            peer_id: Some(seed_peer_id),
            disable_dht: true,
            disable_dht_persistence: true,
            disable_trackers: true,
            enable_upnp_port_forwarding: false,
            listen_port_range: Some(seed_addr.port()..seed_addr.port() + 1),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let handle = seed
        .add_torrent(
            AddTorrent::from_bytes(metainfo),
            Some(AddTorrentOptions {
                overwrite: true,
                output_folder: Some(root.path().to_str().unwrap().to_owned()),
                disable_trackers: true,
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap();
    handle.wait_until_initialized().await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let proxy = tokio::spawn(async move {
        let (first, _) = listener.accept().await.unwrap();
        drop(first);
        let (mut incoming, _) = listener.accept().await.unwrap();
        let mut outgoing = TcpStream::connect(seed_addr).await.unwrap();
        let _ = tokio::io::copy_bidirectional(&mut incoming, &mut outgoing).await;
    });
    let peers = futures::stream::unfold(0, move |step| async move {
        match step {
            0 => Some((addr, 1)),
            1 => {
                tokio::time::sleep(Duration::from_millis(150)).await;
                Some((addr, 2))
            }
            _ => None,
        }
    })
    .boxed();
    let reader_peer_id = crate::tests::test_util::TestPeerMetadata::good().as_peer_id();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        read_metainfo_from_peer_receiver(
            reader_peer_id,
            hash,
            vec![],
            peers,
            Some(PeerConnectionOptions {
                connect_timeout: Some(Duration::from_secs(1)),
                read_write_timeout: Some(Duration::from_secs(1)),
                ..Default::default()
            }),
            Arc::new(StreamConnector::new(
                None,
                NetworkPolicy::OwnedPrivateTcp(vec![addr]),
            )),
        ),
    )
    .await;
    proxy.abort();
    let _ = proxy.await;
    assert_eq!(
        seed.shutdown_and_settle_until(tokio::time::Instant::now() + Duration::from_secs(2))
            .await,
        crate::Settlement::Settled
    );
    assert!(matches!(result, Ok(ReadMetainfoResult::Found { .. })), "The same peer was rediscovered after its transient failure, but metadata was never retried");
}
