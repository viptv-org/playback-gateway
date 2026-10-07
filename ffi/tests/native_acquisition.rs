#![cfg(feature = "test-network-policy")]
use playback_gateway_ffi::{
    TorrentClient, TorrentProgress, TorrentProgressListener, TorrentSettlement,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
struct Callback(Arc<AtomicUsize>);
impl TorrentProgressListener for Callback {
    fn on_status(&self, _: TorrentProgress) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct Seeder {
    runtime: tokio::runtime::Runtime,
    session: Arc<librqbit::Session>,
    _seed: tempfile::TempDir,
    bytes: Vec<u8>,
    hash: String,
    port: u16,
    selected: u32,
    other: u32,
    data: Vec<u8>,
}
impl Seeder {
    fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let seed = tempfile::tempdir().unwrap();
        let data: Vec<_> = (0..90_000).map(|n| (n % 241) as u8).collect();
        std::fs::write(seed.path().join("a.mkv"), &data).unwrap();
        std::fs::write(seed.path().join("b.mkv"), vec![7; 20_000]).unwrap();
        let (session, bytes, port) = runtime.block_on(async {
            let bytes = librqbit::create_torrent(
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
                    disable_trackers: true,
                    disable_dht_persistence: true,
                    enable_upnp_port_forwarding: false,
                    listen_port_range: Some(port..port + 1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            session
                .add_torrent(
                    librqbit::AddTorrent::from_bytes(bytes.clone()),
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
                .unwrap()
                .wait_until_initialized()
                .await
                .unwrap();
            (session, bytes, port)
        });
        let vetted = playback_engine::torrent::vet_native_metainfo(&bytes, None, 0, None).unwrap();
        let hash = vetted.info_hash_hex();
        let files = librqbit::torrent_from_bytes::<librqbit::ByteBufOwned>(&bytes).unwrap();
        let names: Vec<_> = files
            .info
            .iter_file_details()
            .unwrap()
            .map(|f| f.filename.to_vec().unwrap().join("/"))
            .collect();
        let selected = names.iter().position(|n| n == "a.mkv").unwrap() as u32;
        let other = names.iter().position(|n| n == "b.mkv").unwrap() as u32;
        Self {
            runtime,
            session,
            _seed: seed,
            bytes,
            hash,
            port,
            selected,
            other,
            data,
        }
    }
}
impl Drop for Seeder {
    fn drop(&mut self) {
        assert_eq!(
            self.runtime.block_on(
                self.session.shutdown_and_settle_until(
                    tokio::time::Instant::now() + Duration::from_secs(2)
                )
            ),
            librqbit::Settlement::Settled
        );
    }
}
#[test]
fn owned_ffi_acquisitions_share_payload_and_retire_independent_byte_authority() {
    let seed = Seeder::new();
    let cache = tempfile::tempdir().unwrap();
    let client = TorrentClient::new_native_owned(
        cache.path().join("cache").to_str().unwrap().into(),
        2 * 1024 * 1024 * 1024,
        vec![format!("127.0.0.1:{}", seed.port)],
    )
    .unwrap();
    let a = client
        .begin_selected_metainfo(
            seed.bytes.clone(),
            seed.hash.clone(),
            seed.selected,
            Some(90_000),
            30_000,
        )
        .unwrap();
    let b = client
        .begin_selected_metainfo(
            seed.bytes.clone(),
            seed.hash.clone(),
            seed.selected,
            Some(90_000),
            30_000,
        )
        .unwrap();
    let first = a.wait_ready().unwrap();
    let second = b.wait_ready().unwrap();
    assert_eq!(first.files().len(), 1);
    assert_eq!(first.files()[0].index, seed.selected);
    assert_eq!(first.status().total_bytes, 90_000);
    assert!(first.status().downloaded_bytes <= 90_000);
    let first_url = first.stream_url(seed.selected).unwrap();
    let second_url = second.stream_url(seed.selected).unwrap();
    assert_ne!(first_url, second_url);
    assert_eq!(first_url.split('/').nth(3).unwrap().len(), 64);
    assert!(first_url.starts_with("http://127.0.0.1:"));
    assert!(first.stream_url(seed.other).is_err());
    assert!(first.read_range(seed.other, 0, 1).is_err());
    assert_eq!(
        first.read_range(seed.selected, 40_123, 128).unwrap(),
        seed.data[40_123..40_251]
    );
    let callbacks = Arc::new(AtomicUsize::new(0));
    assert!(first
        .clone()
        .set_progress_listener(Box::new(Callback(callbacks.clone())))
        .is_err());
    assert_eq!(callbacks.load(Ordering::SeqCst), 0);
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let wrong_index = first_url.replace(
        &format!("/{}/stream", seed.selected),
        &format!("/{}/stream", seed.other),
    );
    for method in [reqwest::Method::GET, reqwest::Method::HEAD] {
        let response = http
            .request(method, &wrong_index)
            .header("Range", "bytes=0-1")
            .send()
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
        assert!(response.headers().get("Content-Range").is_none());
        assert!(response.bytes().unwrap().is_empty());
    }
    let token = first_url.split('/').nth(3).unwrap();
    let wrong_token = first_url.replace(token, &"0".repeat(64));
    assert_eq!(
        http.get(wrong_token).send().unwrap().status(),
        reqwest::StatusCode::NOT_FOUND
    );
    for range in ["bytes=0-0,2-2", "bytes=+0-1", "bytes=0--1", "bytes=a-b"] {
        let response = http.get(&first_url).header("Range", range).send().unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::RANGE_NOT_SATISFIABLE
        );
        assert!(response.bytes().unwrap().is_empty());
    }
    let duplicate = http
        .get(&first_url)
        .header("Range", "bytes=0-1")
        .header("Range", "bytes=2-3")
        .send()
        .unwrap();
    assert_eq!(
        duplicate.status(),
        reqwest::StatusCode::RANGE_NOT_SATISFIABLE
    );
    assert!(duplicate.bytes().unwrap().is_empty());
    let response = http
        .get(&first_url)
        .header("Range", "bytes=1234-1244")
        .send()
        .unwrap();
    assert_eq!(response.bytes().unwrap().as_ref(), &seed.data[1234..1245]);
    let started = Instant::now();
    assert_eq!(a.cancel_and_wait().unwrap(), TorrentSettlement::Settled);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(a.wait_ready().is_err());
    assert!(first.read_range(seed.selected, 0, 1).is_err());
    assert_eq!(
        second.read_range(seed.selected, 70_000, 64).unwrap(),
        seed.data[70_000..70_064]
    );
    assert_eq!(b.cancel_and_wait().unwrap(), TorrentSettlement::Settled);
    drop(first);
    drop(second);
    drop(a);
    drop(b);
    client.reap().unwrap();
    assert!(!client.has_failed_settlement());
}
#[test]
fn owned_ffi_metadata_wait_has_prehandle_cancel_and_one_total_budget() {
    let stalled = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let cache = tempfile::tempdir().unwrap();
    let client = TorrentClient::new_native_owned(
        cache.path().join("cache").to_str().unwrap().into(),
        2 * 1024 * 1024 * 1024,
        vec![stalled.local_addr().unwrap().to_string()],
    )
    .unwrap();
    let hash = "1111111111111111111111111111111111111111".to_owned();
    let a = client
        .begin_selected(
            format!("magnet:?xt=urn:btih:{hash}"),
            hash.clone(),
            0,
            Some(1),
            30_000,
        )
        .unwrap();
    let (socket, _) = stalled.accept().unwrap();
    let started = Instant::now();
    assert_eq!(a.cancel_and_wait().unwrap(), TorrentSettlement::Settled);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(a.wait_ready().is_err());
    drop(socket);
    client.reap().unwrap();
    let started = Instant::now();
    let b = client
        .begin_selected(format!("magnet:?xt=urn:btih:{hash}"), hash, 0, Some(1), 250)
        .unwrap();
    assert!(matches!(
        b.wait_ready(),
        Err(playback_gateway_ffi::TorrentError::StartupTimeout)
    ));
    assert!(started.elapsed() < Duration::from_millis(600));
    assert_eq!(b.cancel_and_wait().unwrap(), TorrentSettlement::Settled);
    drop(a);
    drop(b);
    client.reap().unwrap();
}
#[test]
fn missing_piece_native_and_http_body_reads_are_cancelled_and_joined() {
    let seed = Seeder::new();
    let stopped_peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let cache = tempfile::tempdir().unwrap();
    let client = TorrentClient::new_native_owned(
        cache.path().join("cache").to_str().unwrap().into(),
        2 * 1024 * 1024 * 1024,
        vec![stopped_peer.local_addr().unwrap().to_string()],
    )
    .unwrap();
    let a = client
        .begin_selected_metainfo(
            seed.bytes.clone(),
            seed.hash.clone(),
            seed.selected,
            Some(90_000),
            30_000,
        )
        .unwrap();
    let handle = a.wait_ready().unwrap();
    let url = handle.stream_url(seed.selected).unwrap();
    let reader = handle.clone();
    let selected = seed.selected;
    let native = std::thread::spawn(move || reader.read_range(selected, 0, 64));
    let http = std::thread::spawn(move || {
        reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
            .get(url)
            .send()
            .and_then(|r| r.bytes())
    });
    std::thread::sleep(Duration::from_millis(80));
    let started = Instant::now();
    assert_eq!(a.cancel_and_wait().unwrap(), TorrentSettlement::Settled);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(native.join().unwrap().is_err());
    assert!(http.join().unwrap().is_err());
    drop(handle);
    drop(a);
    client.reap().unwrap();
}

#[test]
fn late_foreign_handle_return_has_no_bytes_and_remains_charged_until_disposal() {
    let seed = Seeder::new();
    let cache = tempfile::tempdir().unwrap();
    let directory = cache.path().join("cache");
    let client = TorrentClient::new_native_owned(
        directory.to_str().unwrap().into(),
        2 * 1024 * 1024 * 1024,
        vec![format!("127.0.0.1:{}", seed.port)],
    )
    .unwrap();
    let acquisition = client
        .begin_selected_metainfo(
            seed.bytes.clone(),
            seed.hash.clone(),
            seed.selected,
            Some(90_000),
            30_000,
        )
        .unwrap();
    let foreign = acquisition.clone();
    let (ready, available) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let returning = std::thread::spawn(move || {
        let handle = foreign.wait_ready().unwrap();
        ready.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(5)).unwrap();
        handle
    });
    available.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        acquisition.cancel_and_wait().unwrap(),
        TorrentSettlement::Settled
    );
    client.reap().unwrap();
    let payloads = || {
        std::fs::read_dir(&directory)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("torrent-"))
            .count()
    };
    assert_eq!(payloads(), 1);
    release.send(()).unwrap();
    let late = returning.join().unwrap();
    assert!(late.stream_url(seed.selected).is_err());
    assert!(late.read_range(seed.selected, 0, 64).is_err());
    drop(late);
    client.reap().unwrap();
    assert_eq!(payloads(), 0);
}

#[test]
fn native_ffi_retains_full_payload_limit_including_unselected_files() {
    let cache = tempfile::tempdir().unwrap();
    let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TorrentClient::new_native_owned(
        cache.path().join("cache").to_str().unwrap().into(),
        256 * 1024 * 1024,
        vec![peer.local_addr().unwrap().to_string()],
    )
    .unwrap();
    // A tiny selected file cannot authorize the unselected full-torrent reservation.
    let mut bytes = b"d4:infod5:filesld6:lengthi1e4:pathl5:a.mkveed6:lengthi268435456e4:pathl5:b.mkveee4:name5:owned12:piece lengthi16777216e6:pieces340:".to_vec();
    bytes.extend([7; 340]);
    bytes.extend_from_slice(b"ee");
    let hash = playback_engine::torrent::vet_native_metainfo(&bytes, None, 0, Some(1))
        .unwrap()
        .info_hash_hex();
    let acquisition = client
        .begin_selected_metainfo(bytes, hash, 0, Some(1), 30_000)
        .unwrap();
    assert!(matches!(
        acquisition.wait_ready(),
        Err(playback_gateway_ffi::TorrentError::PayloadLimit)
    ));
    assert_eq!(
        acquisition.cancel_and_wait().unwrap(),
        TorrentSettlement::Settled
    );
    client.reap().unwrap();
}
