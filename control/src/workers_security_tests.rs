//! Payload demuxing must retain egress checks beyond the trusted local byte root.
use super::*;
use crate::{
    credentials::{KeySpec, Quotas},
    protocol::CreateSession,
};
use librqbit::{
    create_torrent, AddTorrent, AddTorrentOptions, CreateTorrentOptions, Session, SessionOptions,
};
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires real FFprobe; local torrent containing disguised hostile HLS"]
async fn real_torrent_disguised_playlist_cannot_contact_private_child() {
    hostile_playlist("m3u8", Some(0)).await;
    hostile_playlist("mp4", None).await;
}
async fn hostile_playlist(extension: &str, index: Option<u32>) {
    let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").expect("FFmpeg fixture path");
    let ffprobe = std::env::var("PLAYBACK_TEST_FFPROBE").expect("ffprobe fixture path");
    let root = tempfile::tempdir().unwrap();
    let seed_dir = root.path().join("seed");
    std::fs::create_dir(&seed_dir).unwrap();
    let contacts = Arc::new(AtomicUsize::new(0));
    let route = axum::Router::new().route(
        "/private-secret.ts",
        axum::routing::get({
            let contacts = contacts.clone();
            move || {
                let contacts = contacts.clone();
                async move {
                    contacts.fetch_add(1, Ordering::SeqCst);
                    axum::http::StatusCode::NOT_FOUND
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sentinel = listener.local_addr().unwrap();
    let http = tokio::spawn(async move { axum::serve(listener, route).await.unwrap() });
    let movie = seed_dir.join(format!("movie.{extension}"));
    std::fs::write(&movie,format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:6,\nhttp://{sentinel}/private-secret.ts\n#EXT-X-ENDLIST\n")).unwrap();
    let metainfo = create_torrent(
        &movie,
        CreateTorrentOptions {
            piece_length: Some(16 * 1024),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let magnet = format!("magnet:?xt=urn:btih:{}", metainfo.info_hash().as_string());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_port = listener.local_addr().unwrap().port();
    drop(listener);
    let seed = Session::new_with_opts(
        seed_dir.clone(),
        SessionOptions {
            disable_dht: true,
            disable_dht_persistence: true,
            listen_port_range: Some(peer_port..peer_port + 1),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let seeded = seed
        .add_torrent(
            AddTorrent::from_bytes(metainfo.as_bytes().unwrap()),
            Some(AddTorrentOptions {
                output_folder: Some(seed_dir.to_str().unwrap().into()),
                overwrite: true,
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap();
    seeded.wait_until_initialized().await.unwrap();
    let engine = PlaybackManager::new(playback_engine::Config {
        ffmpeg: ffmpeg.into(),
        ffprobe: ffprobe.into(),
        root: root.path().join("media"),
        max_sessions: 1,
        ttl: Duration::from_secs(60),
    });
    let credentials = Arc::new(
        Credentials::from_connection(
            rusqlite::Connection::open_in_memory().unwrap(),
            tests::BOOTSTRAP,
        )
        .unwrap(),
    );
    let key = credentials
        .issue(
            tests::BOOTSTRAP,
            KeySpec {
                label: "Nested egress fixture".into(),
                namespaces: vec!["fixture".into()],
                scopes: vec![Scope::Create, Scope::Read, Scope::Release],
                quotas: Quotas {
                    inputs: 1,
                    outputs: 1,
                    viewers: 1,
                },
                expires_at: None,
            },
            0,
        )
        .unwrap();
    let principal = credentials.authenticate(&key.secret, 0).unwrap();
    let workers = Workers::new_with_torrent(
        engine,
        credentials,
        NetworkPolicy::default(),
        playback_engine::TorrentConfig {
            enabled: true,
            cache_dir: root.path().join("torrent-cache"),
            enable_dht: false,
            initial_peers: vec![format!("127.0.0.1:{peer_port}").parse().unwrap()],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let request:CreateSession=serde_json::from_value(serde_json::json!({"namespace":"fixture","input":{"url":magnet,"live":false,"file_index":index},"output":{"protocol":"hls","conversion":"video","video_codecs":["h264"],"audio_codecs":["aac"],"max_width":640,"max_height":360}})).unwrap();
    let resolved = workers
        .sources
        .resolve("fixture", &request.input, None)
        .await
        .expect("fixture must resolve magnet metadata");
    let local = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(&resolved.url)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(
        local.starts_with(b"#EXTM3U"),
        "fixture must expose actual hostile playlist bytes"
    );
    drop(resolved);
    let result = workers
        .create(&principal, &request, "nested-egress", 0)
        .await;
    assert_eq!(
        result.err(),
        Some("source_preparation_failed"),
        "payload demux failure must expose only a static redacted failure"
    );
    assert_eq!(
        contacts.load(Ordering::SeqCst),
        0,
        "untrusted torrent payload must not fetch a private child outside the trusted byte URI"
    );
    workers.shutdown().await;
    http.abort();
    seed.cancellation_token().cancel();
}
