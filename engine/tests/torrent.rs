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
    manager.reap().await.unwrap();
    assert!(
        manager.reserve_cache(1).await.is_err(),
        "an active range reader must retain its payload reservation"
    );
    handle.stop();
    // Stop actively cancels this asynchronous missing-piece read. It may have
    // already joined before reap runs; asserting it stays pending races valid
    // cancellation. Wait for the actual receipt before testing the refund.
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), read)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
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
            // Retirement can close the socket before headers or during the body.
            // Either refusal proves the pending reader lost its byte authority.
            let response = reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .get(url)
                .header("Range", "bytes=0-7")
                .send()
                .await?;
            response.bytes().await
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

struct SelectedFixture {
    // Seeder files and cache must outlive their sessions/readers.
    _seed: tempfile::TempDir,
    _cache: tempfile::TempDir,
    session: std::sync::Arc<librqbit::Session>,
    manager: TorrentManager,
    metainfo: Vec<u8>,
    selected: u32,
    other: u32,
    data: Vec<u8>,
}
async fn selected_fixture() -> SelectedFixture {
    let seed = tempfile::tempdir().unwrap();
    let data: Vec<u8> = (0..150_000).map(|n| (n % 251) as u8).collect();
    std::fs::write(seed.path().join("a.mkv"), &data).unwrap();
    std::fs::write(seed.path().join("b.mkv"), vec![7; 90_000]).unwrap();
    let metainfo = librqbit::create_torrent(
        seed.path(),
        librqbit::CreateTorrentOptions {
            piece_length: Some(16 * 1024),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .as_bytes()
    .unwrap()
    .to_vec();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let session = librqbit::Session::new_with_opts(
        seed.path().to_owned(),
        librqbit::SessionOptions {
            disable_dht: true,
            disable_dht_persistence: true,
            enable_upnp_port_forwarding: false,
            listen_port_range: Some(port..port + 1),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let seeder = session
        .add_torrent(
            librqbit::AddTorrent::from_bytes(metainfo.clone()),
            Some(librqbit::AddTorrentOptions {
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
    // Read the owned fixture metadata, without a legacy admission or traversal-order assumption.
    let meta = librqbit::torrent_from_bytes::<librqbit::ByteBufOwned>(&metainfo).unwrap();
    let files: Vec<_> = meta
        .info
        .iter_file_details()
        .unwrap()
        .enumerate()
        .map(|(index, file)| (index as u32, file.filename.to_vec().unwrap().join("/")))
        .collect();
    let selected = files.iter().find(|(_, name)| name == "a.mkv").unwrap().0;
    let other = files.iter().find(|(_, name)| name == "b.mkv").unwrap().0;
    SelectedFixture {
        _seed: seed,
        _cache: cache,
        session,
        manager,
        metainfo,
        selected,
        other,
        data,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_file_capability_denies_unselected_get_head_range_and_native_reads() {
    let f = selected_fixture().await;
    let handle = f
        .manager
        .start_selected_metainfo("selected-fixture", f.metainfo.clone(), f.selected)
        .await
        .unwrap();
    assert_eq!(handle.files().len(), 1);
    assert_eq!(handle.files()[0].index, f.selected);
    let url = handle.stream_url(f.selected).unwrap();
    let unselected_url = url.replace(
        &format!("/{}/stream.", f.selected),
        &format!("/{}/stream.", f.other),
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap();
    let mut failures = Vec::new();
    if handle.stream_url(f.other).is_ok() {
        failures.push("unselected URL creation");
    }
    if handle.read_range(f.other, 0, 16).await.is_ok() {
        failures.push("unselected native read");
    }
    for (method, range) in [
        (reqwest::Method::GET, None),
        (reqwest::Method::HEAD, None),
        (reqwest::Method::GET, Some("bytes=0-15")),
    ] {
        let mut request = client.request(method.clone(), &unselected_url);
        if let Some(range) = range {
            request = request.header("Range", range);
        }
        let response = request.send().await.unwrap();
        if response.status() != reqwest::StatusCode::NOT_FOUND {
            failures.push("unselected HTTP status");
        }
        if response
            .headers()
            .get("Content-Length")
            .is_some_and(|v| v != "0")
            || response.headers().contains_key("Content-Range")
        {
            failures.push("unselected length exposure");
        }
        if !response.bytes().await.unwrap().is_empty() {
            failures.push("unselected HTTP bytes");
        }
    }
    assert_eq!(
        handle.read_range(f.selected, 80_000, 32).await.unwrap(),
        f.data[80_000..80_032]
    );
    let head = client.head(&url).send().await.unwrap();
    assert_eq!(head.status(), reqwest::StatusCode::OK);
    assert_eq!(head.headers()["Content-Length"], "150000");
    assert!(head.bytes().await.unwrap().is_empty());
    let response = client.get(&url).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().as_ref(), f.data.as_slice());
    handle.stop();
    f.session.stop().await;
    assert!(
        failures.is_empty(),
        "selected-file capability violations: {failures:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_file_starts_have_separate_tokens_and_preserve_legacy_aliases() {
    let f = selected_fixture().await;
    let legacy = f
        .manager
        .start_metainfo("selected-fixture", f.metainfo.clone(), Some(f.selected))
        .await
        .unwrap();
    let first = f
        .manager
        .start_selected_metainfo("selected-fixture", f.metainfo.clone(), f.selected)
        .await
        .unwrap();
    let second = f
        .manager
        .start_selected_metainfo("selected-fixture", f.metainfo.clone(), f.selected)
        .await
        .unwrap();
    let legacy_alias = f
        .manager
        .start_metainfo("selected-fixture", f.metainfo.clone(), Some(f.selected))
        .await
        .unwrap();
    assert!(legacy.stream_url(f.selected).unwrap() == legacy_alias.stream_url(f.selected).unwrap());
    let first_url = first.stream_url(f.selected).unwrap();
    let second_url = second.stream_url(f.selected).unwrap();
    assert!(
        first_url != second_url,
        "independent lease endpoints must differ"
    );
    let first_token = first_url.split('/').nth(3).unwrap();
    let second_token = second_url.split('/').nth(3).unwrap();
    assert!(
        first_token != second_token,
        "independent lease tokens must differ"
    );
    let swapped_token_url = second_url.replace(second_token, first_token);
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client.get(swapped_token_url).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    assert!(response.bytes().await.unwrap().is_empty());
    assert!(first.stream_url(f.selected).unwrap() != legacy.stream_url(f.selected).unwrap());
    assert!(
        legacy.stream_url(f.other).is_ok(),
        "legacy archive access remains unrestricted"
    );
    first.stop();
    second.stop();
    legacy.stop();
    f.session.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_file_stop_retires_clones_not_an_independent_same_file_lease() {
    let f = selected_fixture().await;
    let first = f
        .manager
        .start_selected_metainfo("selected-fixture", f.metainfo.clone(), f.selected)
        .await
        .unwrap();
    let alias = first.clone();
    let hash = librqbit::torrent_from_bytes::<librqbit::ByteBufOwned>(&f.metainfo)
        .unwrap()
        .info_hash
        .as_string();
    let second = f
        .manager
        .start_selected_magnet(
            "selected-fixture",
            &format!("magnet:?xt=urn:btih:{hash}"),
            f.selected,
        )
        .await
        .unwrap();
    let old_url = first.stream_url(f.selected).unwrap();
    let valid_url = second.stream_url(f.selected).unwrap();
    assert_eq!(
        first.read_range(f.selected, 0, 16).await.unwrap(),
        f.data[..16]
    );
    first.stop();
    assert!(alias.status().stopped);
    assert!(alias.stream_url(f.selected).is_err());
    assert!(alias.read_range(f.selected, 0, 16).await.is_err());
    assert!(
        !second.status().stopped,
        "stopping one lease must preserve independent same-file work"
    );
    f.manager.reap().await.unwrap();
    assert_eq!(
        second.read_range(f.selected, 120_000, 32).await.unwrap(),
        f.data[120_000..120_032]
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    let response = client
        .get(&valid_url)
        .header("Range", "bytes=120000-120031")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        &f.data[120_000..120_032]
    );
    // A retired listener may reject a connection or return an empty refusal,
    // but cannot serve bytes under its old capability.
    if let Ok(response) = client.get(&old_url).send().await {
        assert!(!response.status().is_success());
        assert!(response.bytes().await.unwrap().is_empty());
    }
    second.stop();
    f.session.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_file_nonzero_index_and_last_owner_drop_preserve_other_authority() {
    let f = selected_fixture().await;
    // Directory traversal order is not metadata identity. Exercise the file that
    // actually has a nonzero index, whichever owned filename appears first.
    let (selected, unselected, expected) = if f.other != 0 {
        (f.other, f.selected, vec![7; 90_000])
    } else {
        (f.selected, f.other, f.data.clone())
    };
    assert_ne!(
        selected, 0,
        "fixture must exercise a nonzero metadata index"
    );
    let hash = librqbit::torrent_from_bytes::<librqbit::ByteBufOwned>(&f.metainfo)
        .unwrap()
        .info_hash
        .as_string();
    let first = f
        .manager
        .start_selected_magnet(
            "selected-fixture",
            &format!("magnet:?xt=urn:btih:{hash}"),
            selected,
        )
        .await
        .unwrap();
    let second = f
        .manager
        .start_selected_metainfo("selected-fixture", f.metainfo.clone(), selected)
        .await
        .unwrap();
    let alias = first.clone();
    assert_eq!(first.files().len(), 1);
    assert_eq!(first.files()[0].index, selected);
    assert_eq!(first.selected_file().index, selected);
    assert!(first.stream_url(unselected).is_err());
    assert!(first.read_range(unselected, 0, 16).await.is_err());
    assert_eq!(
        first.read_range(selected, 0, 16).await.unwrap(),
        expected[..16]
    );
    assert!(
        !first.close_if_last_owner(),
        "explicit clones still alias one lease"
    );
    drop(first);
    assert!(!alias.status().stopped);
    drop(alias);
    f.manager.reap().await.unwrap();
    assert!(!second.status().stopped);
    assert_eq!(
        second.read_range(selected, 80_000, 32).await.unwrap(),
        expected[80_000..80_032]
    );
    assert!(f
        .manager
        .start_selected_metainfo("selected-fixture", f.metainfo.clone(), 999)
        .await
        .is_err());
    assert!(
        !second.status().stopped,
        "invalid selection cannot retire active authority"
    );
    second.stop();
    f.session.stop().await;
}
