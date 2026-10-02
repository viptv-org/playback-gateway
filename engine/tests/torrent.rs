#![cfg(feature = "torrent")]
use playback_engine::{TorrentConfig, TorrentManager};

#[tokio::test]
async fn disabled_transport_never_creates_cache_or_starts_network() {
    let cache = tempfile::tempdir().unwrap().path().join("disabled");
    let manager = TorrentManager::new(TorrentConfig {
        cache_dir: cache.clone(),
        ..Default::default()
    })
    .unwrap();
    assert!(manager
        .start_magnet(
            "fixture",
            "magnet:?xt=urn:btih:1111111111111111111111111111111111111111",
            None
        )
        .await
        .is_err());
    assert!(!cache.exists());
}

#[tokio::test]
async fn magnets_refuse_embedded_credentials_and_source_supplied_peers() {
    use playback_engine::torrent::validate_magnet;
    let hash = "magnet:?xt=urn:btih:1111111111111111111111111111111111111111";
    assert!(validate_magnet(hash).is_ok());
    for suffix in [
        "&tr=http%3A%2F%2Fuser%3Asecret%40example.com%2Fa",
        "&x.pe=127.0.0.1%3A80",
        "&ws=http%3A%2F%2Fexample.com%2Fdata",
        "&as=http%3A%2F%2Fexample.com%2Fdata",
    ] {
        assert!(validate_magnet(&format!("{hash}{suffix}")).is_err());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_seeder_supports_selection_ranges_tokens_shared_leases_and_scope_isolation() {
    use librqbit::{
        create_torrent, AddTorrent, AddTorrentOptions, CreateTorrentOptions, Session,
        SessionOptions,
    };
    use playback_engine::TorrentConfig;
    let seed = tempfile::tempdir().unwrap();
    let source = seed.path().join("movie.MKV");
    let data: Vec<u8> = (0..150_000).map(|n| (n % 251) as u8).collect();
    std::fs::write(&source, &data).unwrap();
    std::fs::write(seed.path().join("readme.txt"), vec![0; 200_000]).unwrap();
    std::fs::write(seed.path().join("small.mp4"), vec![1; 20_000]).unwrap();
    let torrent = create_torrent(
        seed.path(),
        CreateTorrentOptions {
            piece_length: Some(16 * 1024),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let bytes = torrent.as_bytes().unwrap();
    let port = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let session = Session::new_with_opts(
        seed.path().to_owned(),
        SessionOptions {
            disable_dht: true,
            disable_dht_persistence: true,
            listen_port_range: Some(port..port + 1),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let seeder = session
        .add_torrent(
            AddTorrent::from_bytes(bytes.clone()),
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
        .unwrap();
    seeder.wait_until_initialized().await.unwrap();
    let cache = tempfile::tempdir().unwrap();
    let manager = TorrentManager::new(TorrentConfig {
        enabled: true,
        cache_dir: cache.path().join("cache"),
        enable_dht: false,
        initial_peers: vec![format!("127.0.0.1:{port}").parse().unwrap()],
        idle_timeout: std::time::Duration::ZERO,
        ..Default::default()
    })
    .unwrap();
    let tracker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tracker_url = format!("http://{}/tracker", tracker.local_addr().unwrap());
    let mut announced = format!("d8:announce{}:{}", tracker_url.len(), tracker_url).into_bytes();
    announced.extend_from_slice(&bytes[1..]);
    let handle = manager
        .start_metainfo("scope-a", announced, None)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), tracker.accept())
            .await
            .is_err(),
        "unvetted HTTP tracker must never be contacted"
    );
    assert_eq!(handle.selected_file().name, "movie.MKV");
    assert_eq!(handle.selected_file().size, 150_000);
    let index = handle.selected_file().index;
    assert_eq!(
        handle.read_range(index, 80_000, 32).await.unwrap(),
        &data[80_000..80_032]
    );
    let url = handle.stream_url(index).unwrap();
    assert!(url.ends_with("stream.mkv"));
    let shared = manager
        .start_metainfo("scope-a", bytes.to_vec(), None)
        .await
        .unwrap();
    assert_eq!(shared.stream_url(index).unwrap(), url);
    let other = manager
        .start_metainfo("scope-b", bytes.to_vec(), None)
        .await
        .unwrap();
    assert_ne!(other.stream_url(index).unwrap(), url);
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let range = client
        .get(&url)
        .header("Range", "bytes=1234-1244")
        .send()
        .await
        .unwrap();
    assert_eq!(range.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    assert_eq!(range.headers()["Content-Range"], "bytes 1234-1244/150000");
    assert_eq!(range.bytes().await.unwrap().as_ref(), &data[1234..1245]);
    let suffix = client
        .get(&url)
        .header("Range", "bytes=-7")
        .send()
        .await
        .unwrap();
    assert_eq!(
        suffix.bytes().await.unwrap().as_ref(),
        &data[data.len() - 7..]
    );
    let head = client.head(&url).send().await.unwrap();
    assert_eq!(head.headers()["Content-Length"], "150000");
    assert!(head.bytes().await.unwrap().is_empty());
    for invalid in [
        "bytes=0-0,2-2",
        "bytes=150000-",
        "bytes=-0",
        "bytes=20-10",
        "items=0-10",
    ] {
        let response = client
            .get(&url)
            .header("Range", invalid)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::RANGE_NOT_SATISFIABLE
        );
    }
    let mut wrong = url::Url::parse(&url).unwrap();
    let path = format!("/wrong/{index}/stream.mkv");
    wrong.set_path(&path);
    assert_eq!(
        client.get(wrong).send().await.unwrap().status(),
        reqwest::StatusCode::NOT_FOUND
    );
    drop(handle);
    assert_eq!(shared.read_range(index, 0, 8).await.unwrap(), &data[..8]);
    drop(shared);
    drop(other);
    manager.reap().await.unwrap();
    let resumed = manager
        .start_metainfo("scope-a", bytes.to_vec(), None)
        .await
        .unwrap();
    assert_ne!(resumed.stream_url(index).unwrap(), url);
    resumed.stop();
    assert!(resumed.read_range(index, 0, 8).await.is_err());
    manager.reap().await.unwrap();
    assert_eq!(
        std::fs::read_dir(cache.path().join("cache"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("torrent-"))
            .count(),
        0,
        "stopped retained handles must release their cache reservations"
    );
    session.stop().await;
}

#[tokio::test]
async fn global_cache_budget_refuses_active_spools_and_reclaims_released_spools() {
    let temp = tempfile::tempdir().unwrap();
    let manager = TorrentManager::new(TorrentConfig {
        enabled: true,
        cache_dir: temp.path().join("cache"),
        max_bytes: playback_engine::torrent::MIN_CACHE_BYTES,
        enable_dht: false,
        ..Default::default()
    })
    .unwrap();
    let reservation = manager
        .reserve_cache(playback_engine::torrent::MIN_CACHE_BYTES)
        .await
        .unwrap();
    assert!(manager.reserve_cache(1).await.is_err());
    let path = reservation.path().to_owned();
    drop(reservation);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while path.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(manager.reserve_cache(1).await.is_ok());
}

#[tokio::test]
async fn webseed_metainfo_is_rejected_explicitly_instead_of_silently_ignoring_transport() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("movie.mkv");
    std::fs::write(&source, b"fixture").unwrap();
    let torrent = librqbit::create_torrent(&source, Default::default())
        .await
        .unwrap()
        .as_bytes()
        .unwrap();
    let mut bytes = torrent[..torrent.len() - 1].to_vec();
    bytes.extend_from_slice(b"8:url-list19:http://example.org/e");
    let manager = TorrentManager::new(TorrentConfig {
        enabled: true,
        cache_dir: temp.path().join("cache"),
        enable_dht: false,
        ..Default::default()
    })
    .unwrap();
    let error = manager
        .start_metainfo("fixture", bytes, None)
        .await
        .unwrap_err();
    assert_eq!(error, "Torrent webseeds unsupported");
    assert!(!temp.path().join("cache").exists());
}

#[tokio::test]
async fn cache_lock_lasts_until_every_reservation_is_released() {
    let temp = tempfile::tempdir().unwrap();
    let config = TorrentConfig {
        enabled: true,
        cache_dir: temp.path().join("cache"),
        enable_dht: false,
        ..Default::default()
    };
    let first = TorrentManager::new(config.clone()).unwrap();
    let reservation = first.reserve_cache(1024).await.unwrap();
    let second = TorrentManager::new(config).unwrap();
    assert!(second.reserve_cache(1).await.is_err());
    drop(first);
    assert!(second.reserve_cache(1).await.is_err());
    drop(reservation);
    let reservation = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(reservation) = second.reserve_cache(1).await {
                break reservation;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(reservation.bytes(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_metadata_reaping_does_not_exhaust_namespace_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let manager = TorrentManager::new(TorrentConfig {
        enabled: true,
        cache_dir: temp.path().join("cache"),
        enable_dht: false,
        ..Default::default()
    })
    .unwrap();
    // With no DHT, trackers or initial peers metadata fails immediately without
    // public IO. Reaping empty sessions keeps subsequent scopes admissible.
    for index in 0..40 {
        let error = manager
            .start_magnet(
                &format!("scope-{index}"),
                "magnet:?xt=urn:btih:1111111111111111111111111111111111111111",
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(error, "Invalid torrent metadata");
        manager.reap().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_torrent_keeps_payload_budget_until_pending_range_read_is_cancelled() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("movie.mkv");
    std::fs::write(&source, b"known missing fixture bytes").unwrap();
    let metainfo = librqbit::create_torrent(&source, Default::default())
        .await
        .unwrap()
        .as_bytes()
        .unwrap();
    let manager = TorrentManager::new(TorrentConfig {
        enabled: true,
        cache_dir: temp.path().join("cache"),
        max_bytes: playback_engine::torrent::MIN_CACHE_BYTES,
        enable_dht: false,
        idle_timeout: std::time::Duration::ZERO,
        ..Default::default()
    })
    .unwrap();
    let handle = manager
        .start_metainfo("fixture", metainfo.to_vec(), None)
        .await
        .unwrap();
    let remaining = manager
        .reserve_cache(playback_engine::torrent::MIN_CACHE_BYTES - handle.selected_file().size)
        .await
        .unwrap();
    let (entered, entering) = tokio::sync::oneshot::channel();
    let read = tokio::spawn({
        let handle = handle.clone();
        async move {
            let reading = handle.read_range(0, 0, 8);
            tokio::pin!(reading);
            std::future::poll_fn(|cx| {
                assert!(
                    std::future::Future::poll(reading.as_mut(), cx).is_pending(),
                    "missing unseeded pieces must block"
                );
                std::task::Poll::Ready(())
            })
            .await;
            let _ = entered.send(());
            reading.await
        }
    });
    entering.await.unwrap();
    handle.stop();
    manager.reap().await.unwrap();
    assert!(
        manager.reserve_cache(1).await.is_err(),
        "stopping a lease must not unlink/refund an in-flight reader's payload"
    );
    read.abort();
    let _ = read.await;
    manager.reap().await.unwrap();
    assert!(
        manager.reserve_cache(1).await.is_ok(),
        "payload capacity must return after the actual range reader closes"
    );
    drop(remaining);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_external_handle_drop_closes_pending_http_reader_and_reclaims_budget() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("movie.mkv");
    std::fs::write(&source, b"missing torrent fixture").unwrap();
    let metainfo = librqbit::create_torrent(&source, Default::default())
        .await
        .unwrap()
        .as_bytes()
        .unwrap();
    let manager = TorrentManager::new(TorrentConfig {
        enabled: true,
        cache_dir: temp.path().join("cache"),
        max_bytes: playback_engine::torrent::MIN_CACHE_BYTES,
        enable_dht: false,
        idle_timeout: std::time::Duration::ZERO,
        ..Default::default()
    })
    .unwrap();
    let handle = manager
        .start_metainfo("fixture", metainfo.to_vec(), None)
        .await
        .unwrap();
    let remaining = manager
        .reserve_cache(playback_engine::torrent::MIN_CACHE_BYTES - handle.selected_file().size)
        .await
        .unwrap();
    let settled = handle.read_settler();
    let url = handle.stream_url(0).unwrap();
    let read = tokio::spawn({
        let url = url.clone();
        async move {
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .get(url)
                .header("Range", "bytes=0-7")
                .send()
                .await
                .unwrap()
                .bytes()
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !settled.has_pending_reads() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(handle);
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        settled.settle_closed_reads(),
    )
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), read)
            .await
            .unwrap()
            .unwrap()
            .is_err(),
        "internal HTTP references must not keep a last-dropped public handle alive"
    );
    assert!(reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .head(url)
        .send()
        .await
        .is_err());
    manager.reap().await.unwrap();
    assert!(manager.reserve_cache(1).await.is_ok());
    drop(remaining);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_owner_close_preserves_other_torrent_aliases() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("movie.mkv");
    std::fs::write(&source, b"unseeded alias fixture").unwrap();
    let metainfo = librqbit::create_torrent(&source, Default::default())
        .await
        .unwrap()
        .as_bytes()
        .unwrap();
    let manager = TorrentManager::new(TorrentConfig {
        enabled: true,
        cache_dir: temp.path().join("cache"),
        enable_dht: false,
        ..Default::default()
    })
    .unwrap();
    let first = manager
        .start_metainfo("fixture", metainfo.to_vec(), None)
        .await
        .unwrap();
    let second = manager
        .start_metainfo("fixture", metainfo.to_vec(), Some(0))
        .await
        .unwrap();
    let url = first.stream_url(0).unwrap();
    assert_eq!(url, second.stream_url(0).unwrap());
    assert!(!first.close_if_last_owner());
    assert!(reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .head(&url)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    drop(first);
    assert!(second.close_if_last_owner());
    second.settle_closed_reads().await;
    assert!(second.stream_url(0).is_err());
}
