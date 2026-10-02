use playback_gateway_ffi::{
    default_torrent_options, TorrentClient, TorrentProgress, TorrentProgressListener,
};
use std::{sync::mpsc, time::Duration};
struct Listener(mpsc::Sender<TorrentProgress>);
impl TorrentProgressListener for Listener {
    fn on_status(&self, status: TorrentProgress) {
        let _ = self.0.send(status);
    }
}

#[test]
fn native_handle_streams_local_seed_ranges_reports_progress_and_stops() {
    let fixture = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let seed = tempfile::tempdir().unwrap();
    let path = seed.path().join("movie.mkv");
    let data: Vec<u8> = (0..90_000).map(|i| (i % 241) as u8).collect();
    std::fs::write(&path, &data).unwrap();
    let (session, bytes, port) = fixture.block_on(async {
        let meta = librqbit::create_torrent(
            &path,
            librqbit::CreateTorrentOptions {
                piece_length: Some(16 * 1024),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .as_bytes()
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let session = librqbit::Session::new_with_opts(
            seed.path().to_owned(),
            librqbit::SessionOptions {
                disable_dht: true,
                disable_dht_persistence: true,
                listen_port_range: Some(port..port + 1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let handle = session
            .add_torrent(
                librqbit::AddTorrent::from_bytes(meta.clone()),
                Some(librqbit::AddTorrentOptions {
                    output_folder: Some(seed.path().to_str().unwrap().into()),
                    overwrite: true,
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();
        handle.wait_until_initialized().await.unwrap();
        (session, meta.to_vec(), port)
    });
    let cache = tempfile::tempdir().unwrap();
    let mut options = default_torrent_options();
    options.enable_dht = false;
    options.initial_peers = vec![format!("127.0.0.1:{port}")];
    let client =
        TorrentClient::new(cache.path().join("cache").to_str().unwrap().into(), options).unwrap();
    let handle = client.start_metainfo(bytes, None).unwrap();
    let files = handle.files();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].name, "movie.mkv");
    assert_eq!(files[0].size, 90_000);
    assert_eq!(
        handle.read_range(0, 40_123, 128).unwrap(),
        data[40_123..40_251]
    );
    assert!(handle
        .stream_url(0)
        .unwrap()
        .starts_with("http://127.0.0.1:"));
    let (sender, receiver) = mpsc::channel();
    handle
        .clone()
        .set_progress_listener(Box::new(Listener(sender)))
        .unwrap();
    let status = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(status.total_bytes, 90_000);
    assert!(!status.stopped);
    handle.stop();
    assert!(handle.status().stopped);
    assert!(handle.stream_url(0).is_err());
    assert!(handle.read_range(0, 0, 1).is_err());
    drop(handle);
    client.reap().unwrap();
    drop(client);
    fixture.block_on(session.stop());
}
