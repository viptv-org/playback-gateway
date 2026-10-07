#![cfg(feature = "torrent")]
use librqbit::{
    create_torrent, AddTorrent, AddTorrentOptions, CreateTorrentOptions, Session, SessionOptions,
};
use playback_engine::{TorrentConfig, TorrentManager};
use std::time::Duration;

/// Real TCP seeder: payload is 32 times larger than the retained piece cache.
/// Seeks force eviction/re-download; simultaneous readers share the same input.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rolling_cache_reads_large_file_seeks_back_and_reclaims() {
    rolling_read_case(false).await;
}
#[cfg(feature = "test-network-policy")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_rolling_cross_file_readers_keep_independent_grants() {
    rolling_read_case(true).await;
}
async fn rolling_read_case(native: bool) {
    let seed = tempfile::tempdir().unwrap();
    let data: Vec<u8> = (0..2 * 1024 * 1024)
        .map(|i| ((i * 31 + i / 16384) % 251) as u8)
        .collect();
    std::fs::write(seed.path().join("movie.mkv"), &data).unwrap();
    // A non-media file precedes the video and makes its first piece cross a file boundary.
    std::fs::write(seed.path().join("a-not-selected.bin"), vec![42; 65_673]).unwrap();
    let torrent = create_torrent(
        seed.path(),
        CreateTorrentOptions {
            piece_length: Some(16384),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let bytes = torrent.as_bytes().unwrap();
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    let seeder = Session::new_with_opts(
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
    let seeded = seeder
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
    seeded.wait_until_initialized().await.unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = cache.path().join("cache");
    let budget = 65536;
    let address = format!("127.0.0.1:{port}").parse().unwrap();
    let config = TorrentConfig {
        enabled: true,
        cache_dir: root.clone(),
        enable_dht: false,
        initial_peers: vec![address],
        max_bytes: playback_engine::torrent::MIN_CACHE_BYTES,
        piece_cache_bytes: Some(budget),
        idle_timeout: Duration::ZERO,
        ..Default::default()
    };
    let manager = if native {
        #[cfg(feature = "test-network-policy")]
        {
            let mut config = config;
            config.initial_peers.clear();
            TorrentManager::new_native_owned(config, vec![address]).unwrap()
        }
        #[cfg(not(feature = "test-network-policy"))]
        {
            unreachable!()
        }
    } else {
        TorrentManager::new(config).unwrap()
    };
    let index = 1;
    let acquisitions = if native {
        let hash = playback_engine::torrent::vet_native_metainfo(
            &bytes,
            None,
            index,
            Some(data.len() as u64),
        )
        .unwrap()
        .info_hash_hex();
        Some((
            manager
                .begin_native_selected_metainfo(
                    "rolling".into(),
                    bytes.to_vec(),
                    hash.clone(),
                    index,
                    Some(data.len() as u64),
                    Duration::from_secs(30),
                )
                .unwrap(),
            manager
                .begin_native_selected_metainfo(
                    "rolling".into(),
                    bytes.to_vec(),
                    hash,
                    index,
                    Some(data.len() as u64),
                    Duration::from_secs(30),
                )
                .unwrap(),
        ))
    } else {
        None
    };
    let (handle, shared) = if let Some((a, b)) = &acquisitions {
        (a.wait_ready().await.unwrap(), b.wait_ready().await.unwrap())
    } else {
        (
            manager
                .start_metainfo("rolling", bytes.to_vec(), None)
                .await
                .unwrap(),
            manager
                .start_metainfo("rolling", bytes.to_vec(), None)
                .await
                .unwrap(),
        )
    };
    assert_eq!(handle.selected_file().index, index);
    let remaining = manager
        .reserve_cache(playback_engine::torrent::MIN_CACHE_BYTES - budget)
        .await
        .unwrap();
    assert!(
        manager.reserve_cache(1).await.is_err(),
        "active rolling reservations count against the global budget"
    );
    if !native {
        assert_eq!(
            handle.stream_url(index).unwrap(),
            shared.stream_url(index).unwrap()
        );
    } else {
        assert_ne!(
            handle.stream_url(index).unwrap(),
            shared.stream_url(index).unwrap()
        );
        assert!(handle.read_range(0, 0, 1).await.is_err());
    }
    let payloads: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path().join("pieces.cache");
            p.exists().then_some(p)
        })
        .collect();
    assert_eq!(payloads.len(), 1);
    let check_bound = || {
        assert!(std::fs::metadata(&payloads[0]).unwrap().len() <= budget);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert!(std::fs::metadata(&payloads[0]).unwrap().blocks() * 512 <= budget);
        }
    };
    // Each range crosses piece boundaries and each jump replaces the old window.
    for offset in [0, 900_000, 1_900_000, 0, 700_000, 0] {
        let got = tokio::time::timeout(
            Duration::from_secs(15),
            handle.read_range(index, offset, 33000),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(got, data[offset as usize..offset as usize + 33000]);
        check_bound();
    }
    let (a, b) = tokio::join!(
        handle.read_range(index, 100_000, 20000),
        shared.read_range(index, 1_500_000, 20000)
    );
    assert_eq!(a.unwrap(), data[100_000..120_000]);
    assert_eq!(b.unwrap(), data[1_500_000..1_520_000]);
    check_bound();
    let entire = tokio::time::timeout(
        Duration::from_secs(20),
        handle.read_range(index, 0, data.len() as u32),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        entire, data,
        "sequential playback must reach EOF through a cache smaller than the video"
    );
    check_bound();
    if let Some((a, b)) = acquisitions {
        assert_eq!(a.cancel_and_wait().await, librqbit::Settlement::Settled);
        assert!(handle.read_range(index, 0, 1).await.is_err());
        assert_eq!(shared.read_range(index, 0, 100).await.unwrap(), data[..100]);
        assert_eq!(b.cancel_and_wait().await, librqbit::Settlement::Settled);
    } else {
        handle.stop();
        shared.stop();
    }
    drop(remaining);
    if native {
        // Strict acquisition keeps its reservation until returned handles retire.
        drop(handle);
        drop(shared);
    }
    manager.reap().await.unwrap();
    assert!(!payloads[0].exists());
    #[cfg(target_os = "linux")]
    for fd in std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(Result::ok)
    {
        if let Ok(path) = std::fs::read_link(fd.path()) {
            assert!(
                !path
                    .to_string_lossy()
                    .starts_with(payloads[0].to_str().unwrap()),
                "retirement must close deleted cache FDs even while stopped handles exist"
            );
        }
    }

    seeder.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rolling_stop_cancels_missing_piece_read_and_releases_budget() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("movie.mkv");
    std::fs::write(&source, vec![11; 256 * 1024]).unwrap();
    let bytes = create_torrent(
        &source,
        CreateTorrentOptions {
            piece_length: Some(16384),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .as_bytes()
    .unwrap();
    let root = temp.path().join("cache");
    let manager = TorrentManager::new(TorrentConfig {
        enabled: true,
        cache_dir: root.clone(),
        enable_dht: false,
        max_bytes: playback_engine::torrent::MIN_CACHE_BYTES,
        piece_cache_bytes: Some(65536),
        idle_timeout: Duration::ZERO,
        ..Default::default()
    })
    .unwrap();
    let handle = manager
        .start_metainfo("missing", bytes.to_vec(), None)
        .await
        .unwrap();
    let (entered, entering) = tokio::sync::oneshot::channel();
    let read = tokio::spawn({
        let handle = handle.clone();
        async move {
            let reading = handle.read_range(0, 0, 16);
            tokio::pin!(reading);
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(reading.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            let _ = entered.send(());
            reading.await
        }
    });
    entering.await.unwrap();
    assert_eq!(
        handle
            .stop_and_settle_until(tokio::time::Instant::now() + Duration::from_secs(3))
            .await,
        librqbit::Settlement::Settled
    );
    assert!(read.await.unwrap().is_err());
    manager.reap().await.unwrap();
    let full_budget = manager
        .reserve_cache(playback_engine::torrent::MIN_CACHE_BYTES)
        .await
        .unwrap();
    drop(full_budget);
}

/// An actual peer disconnects after writing half of a piece; its replacement
/// must retry the missing blocks rather than leaving a permanently pinned slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rolling_peer_disconnect_retries_partial_piece_without_advertising_cache() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("movie.mkv");
    let data: Vec<u8> = (0..1024 * 1024).map(|n| (n % 251) as u8).collect();
    std::fs::write(&source, &data).unwrap();
    let bytes = create_torrent(
        &source,
        CreateTorrentOptions {
            piece_length: Some(32768),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .as_bytes()
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (disconnected, disconnecting) = tokio::sync::oneshot::channel();
    let seed = tokio::spawn({
        let data = data.clone();
        async move {
            let mut disconnected = Some(disconnected);
            for attempt in 0..2 {
                // A downloader may retire an accepted connection before its
                // handshake. Keep the replacement seeder available for retry.
                let (mut peer, mut handshake) = loop {
                    let (mut peer, _) = listener.accept().await.unwrap();
                    let mut handshake = [0; 68];
                    if matches!(
                        tokio::time::timeout(
                            Duration::from_secs(2),
                            peer.read_exact(&mut handshake)
                        )
                        .await,
                        Ok(Ok(_))
                    ) {
                        break (peer, handshake);
                    }
                };
                handshake[20..28].fill(0);
                for byte in &mut handshake[48..] {
                    *byte ^= 0x5a;
                }
                peer.write_all(&handshake).await.unwrap();
                peer.write_all(&[0, 0, 0, 5, 5, 255, 255, 255, 255])
                    .await
                    .unwrap();
                peer.write_all(&[0, 0, 0, 1, 1]).await.unwrap();
                loop {
                    let Ok(size) = peer.read_u32().await else {
                        break;
                    };
                    if size == 0 {
                        continue;
                    }
                    assert!(size <= 65536);
                    let mut message = vec![0; size as usize];
                    if peer.read_exact(&mut message).await.is_err() {
                        break;
                    }
                    assert!(
                        !matches!(message[0], 4 | 5),
                        "rolling cache must never advertise evictable availability"
                    );
                    if message[0] != 6 {
                        continue;
                    }
                    assert_eq!(message.len(), 13);
                    let piece = u32::from_be_bytes(message[1..5].try_into().unwrap()) as usize;
                    let offset = u32::from_be_bytes(message[5..9].try_into().unwrap()) as usize;
                    let len = u32::from_be_bytes(message[9..13].try_into().unwrap()) as usize;
                    let start = piece * 32768 + offset;
                    peer.write_u32((len + 9) as u32).await.unwrap();
                    peer.write_u8(7).await.unwrap();
                    peer.write_all(&message[1..9]).await.unwrap();
                    peer.write_all(&data[start..start + len]).await.unwrap();
                    if attempt == 0 {
                        let _ = disconnected.take().unwrap().send(());
                        break;
                    }
                }
            }
        }
    });
    let manager = TorrentManager::new(TorrentConfig {
        enabled: true,
        cache_dir: temp.path().join("cache"),
        enable_dht: false,
        initial_peers: vec![address],
        piece_cache_bytes: Some(65536),
        idle_timeout: Duration::ZERO,
        ..Default::default()
    })
    .unwrap();
    let handle = manager
        .start_metainfo("disconnect", bytes.to_vec(), None)
        .await
        .unwrap();
    let read = tokio::spawn({
        let handle = handle.clone();
        async move { handle.read_range(0, 0, 40000).await }
    });
    tokio::time::timeout(Duration::from_secs(5), disconnecting)
        .await
        .unwrap()
        .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(25), read)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(got, data[..40000]);
    assert_eq!(
        handle.read_range(0, 800000, 40000).await.unwrap(),
        data[800000..840000]
    );
    handle
        .stop_and_settle_until(tokio::time::Instant::now() + Duration::from_secs(3))
        .await;
    manager.reap().await.unwrap();
    seed.abort();
    let _ = seed.await;
}

/// A valid 100 GiB virtual torrent served lazily over TCP. Only requested pieces
/// are materialized; the downloader must admit it into a 4 MiB cache and use
/// 64-bit offsets correctly, including re-downloading an evicted first piece.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hundred_gib_torrent_seeks_with_four_mib_cache() {
    hundred_gib_case(false).await;
}

#[cfg(feature = "test-network-policy")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_hundred_gib_exact_file_seeks_with_four_mib_cache() {
    hundred_gib_case(true).await;
}

async fn hundred_gib_case(native: bool) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let temp = tempfile::tempdir().unwrap();
    let piece_len = 2 * 1024 * 1024usize;
    let file_size = 100 * 1024 * 1024 * 1024u64;
    let count = (file_size / piece_len as u64) as usize;
    let sample = temp.path().join("sample.mkv");
    let mut hashes = Vec::new();
    for value in [17u8, 93] {
        std::fs::write(&sample, vec![value; piece_len]).unwrap();
        let bytes = create_torrent(
            &sample,
            CreateTorrentOptions {
                piece_length: Some(piece_len as u32),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .as_bytes()
        .unwrap();
        let metadata = librqbit::torrent_from_bytes::<bencode::ByteBufOwned>(&bytes).unwrap();
        hashes.push(metadata.info.pieces.0[..20].to_vec());
    }
    let mut metainfo = format!(
        "d4:infod6:lengthi{file_size}e4:name9:large.mkv12:piece lengthi{piece_len}e6:pieces{}:",
        count * 20
    )
    .into_bytes();
    for index in 0..count {
        metainfo.extend_from_slice(&hashes[index % 2]);
    }
    metainfo.extend_from_slice(b"ee");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let peer = tokio::spawn({
        let requests = requests.clone();
        async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            let mut handshake = [0; 68];
            peer.read_exact(&mut handshake).await.unwrap();
            handshake[20..28].fill(0);
            for byte in &mut handshake[48..] {
                *byte ^= 0x5a;
            }
            peer.write_all(&handshake).await.unwrap();
            peer.write_u32((1 + count / 8) as u32).await.unwrap();
            peer.write_u8(5).await.unwrap();
            peer.write_all(&vec![255; count / 8]).await.unwrap();
            peer.write_all(&[0, 0, 0, 1, 1]).await.unwrap();
            while let Ok(size) = peer.read_u32().await {
                if size == 0 {
                    continue;
                }
                assert!(size <= 65536);
                let mut message = vec![0; size as usize];
                if peer.read_exact(&mut message).await.is_err() {
                    break;
                }
                assert!(!matches!(message[0], 4 | 5));
                if message[0] != 6 {
                    continue;
                }
                let index = u32::from_be_bytes(message[1..5].try_into().unwrap()) as usize;
                let offset = u32::from_be_bytes(message[5..9].try_into().unwrap()) as usize;
                let length = u32::from_be_bytes(message[9..13].try_into().unwrap()) as usize;
                assert!(index < count && offset + length <= piece_len && length <= 16384);
                requests.lock().unwrap().push((index, offset));
                peer.write_u32((length + 9) as u32).await.unwrap();
                peer.write_u8(7).await.unwrap();
                peer.write_all(&message[1..9]).await.unwrap();
                peer.write_all(&vec![[17, 93][index % 2]; length])
                    .await
                    .unwrap();
            }
        }
    });
    let root = temp.path().join("cache");
    let budget = 4 * 1024 * 1024;
    let config = TorrentConfig {
        enabled: true,
        cache_dir: root.clone(),
        enable_dht: false,
        initial_peers: vec![address],
        max_bytes: playback_engine::torrent::MIN_CACHE_BYTES,
        piece_cache_bytes: Some(budget),
        idle_timeout: Duration::ZERO,
        ..Default::default()
    };
    let manager = if native {
        #[cfg(feature = "test-network-policy")]
        {
            let mut config = config;
            config.initial_peers.clear();
            TorrentManager::new_native_owned(config, vec![address]).unwrap()
        }
        #[cfg(not(feature = "test-network-policy"))]
        {
            unreachable!()
        }
    } else {
        TorrentManager::new(config).unwrap()
    };
    let acquisition = if native {
        let hash =
            playback_engine::torrent::vet_native_metainfo(&metainfo, None, 0, Some(file_size))
                .unwrap()
                .info_hash_hex();
        Some(
            manager
                .begin_native_selected_metainfo(
                    "huge".into(),
                    metainfo.clone(),
                    hash,
                    0,
                    Some(file_size),
                    Duration::from_secs(30),
                )
                .unwrap(),
        )
    } else {
        None
    };
    let handle = if let Some(a) = &acquisition {
        a.wait_ready().await.unwrap()
    } else {
        manager
            .start_metainfo("huge", metainfo, None)
            .await
            .unwrap()
    };
    assert_eq!(handle.selected_file().size, file_size);
    for index in [0, count / 2 + 1, count - 1, 0] {
        let offset = index as u64 * piece_len as u64 + 123;
        let got = handle.read_range(0, offset, 32768).await.unwrap();
        assert_eq!(got, vec![[17, 93][index % 2]; 32768]);
    }
    let requested = requests.lock().unwrap();
    assert!(requested.iter().any(|(i, _)| *i == count - 1));
    assert!(
        requested.iter().filter(|(i, o)| *i == 0 && *o == 0).count() >= 2,
        "evicted data must be fetched again"
    );
    drop(requested);
    let payload = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path().join("pieces.cache"))
        .find(|p| p.exists())
        .unwrap();
    assert_eq!(std::fs::metadata(&payload).unwrap().len(), budget);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert!(std::fs::metadata(&payload).unwrap().blocks() * 512 <= budget);
    }
    if let Some(a) = acquisition {
        assert!(handle.read_range(1, 0, 1).await.is_err());
        assert_eq!(a.cancel_and_wait().await, librqbit::Settlement::Settled);
        assert!(handle.read_range(0, 0, 1).await.is_err());
    } else {
        assert_eq!(
            handle
                .stop_and_settle_until(tokio::time::Instant::now() + Duration::from_secs(3))
                .await,
            librqbit::Settlement::Settled
        );
    }
    drop(handle);
    manager.reap().await.unwrap();
    assert!(!payload.exists());
    peer.abort();
    let _ = peer.await;
}
