//! Source refusal is verified before the cache or payload files exist.
use playback_engine::torrent::{vet_native_metainfo, TorrentConfig, TorrentManager};
fn metadata() -> Vec<u8> {
    let mut bytes =
        b"d4:infod6:lengthi1e4:name9:owned.mp412:piece lengthi16384e6:pieces20:".to_vec();
    bytes.extend([7; 20]);
    bytes.extend_from_slice(b"ee");
    bytes
}
#[tokio::test]
async fn native_denied_identity_and_hints_do_no_cache_io() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("not-created");
    let manager = TorrentManager::new_native_public(TorrentConfig {
        enabled: true,
        cache_dir: cache.clone(),
        ..Default::default()
    })
    .unwrap();
    let meta = metadata();
    let hash = vet_native_metainfo(&meta, None, 0, Some(1))
        .unwrap()
        .info_hash_hex();
    for (expected, index, size) in [
        ("0000000000000000000000000000000000000000", 0, 1),
        (hash.as_str(), 1, 1),
        (hash.as_str(), 0, 2),
    ] {
        assert!(manager
            .start_native_selected_metainfo("scope", meta.clone(), expected, index, size)
            .await
            .is_err());
        assert!(!cache.exists());
    }
    assert!(manager
        .start_native_selected_magnet(
            "scope",
            &format!("magnet:?xt=urn:btih:{hash}&x.pe=127.0.0.1:1"),
            &hash,
            0,
            1
        )
        .await
        .is_err());
    assert!(!cache.exists());
    assert!(manager
        .start_selected_metainfo("scope", meta, 0)
        .await
        .is_err());
    assert!(!cache.exists());
}
#[test]
fn native_configuration_cannot_add_source_peers_or_peer_listener() {
    assert!(TorrentManager::new_native_public(TorrentConfig {
        initial_peers: vec!["127.0.0.1:6881".parse().unwrap()],
        ..Default::default()
    })
    .is_err());
    assert!(TorrentManager::new_native_public(TorrentConfig {
        listen_port: Some(6881),
        ..Default::default()
    })
    .is_err());
}
#[cfg(feature = "test-network-policy")]
#[test]
fn private_fixture_policy_requires_dht_disabled_exact_owned_tcp() {
    let owned = "127.0.0.1:6881".parse().unwrap();
    assert!(TorrentManager::new_native_owned(TorrentConfig::default(), vec![owned]).is_err());
    assert!(TorrentManager::new_native_owned(
        TorrentConfig {
            enable_dht: false,
            ..Default::default()
        },
        vec![owned]
    )
    .is_ok());
    assert!(TorrentManager::new_native_owned(
        TorrentConfig {
            enable_dht: false,
            ..Default::default()
        },
        vec!["8.8.8.8:6881".parse().unwrap()]
    )
    .is_err());
}
