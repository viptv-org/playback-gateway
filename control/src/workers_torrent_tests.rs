use super::*;
use crate::{
    credentials::{KeySpec, Quotas},
    protocol::CreateSession,
};
use librqbit::{
    create_torrent, AddTorrent, AddTorrentOptions, CreateTorrentOptions, Session, SessionOptions,
};
use std::{path::Path, time::Duration};

#[tokio::test]
async fn pending_torrent_admission_blocks_concurrent_inputs_and_cancellation_reclaims_it() {
    use tokio::io::AsyncReadExt;
    let (root, workers, mut principal) = tests::fixture(false).await;
    principal.key.spec.quotas.inputs = 1;
    let mut workers = Arc::try_unwrap(workers).ok().unwrap();
    workers.proxy = EgressProxy::start(NetworkPolicy::loopback_fixture())
        .await
        .unwrap();
    workers.sources = Arc::new(
        Sources::new(
            playback_engine::TorrentConfig {
                enabled: true,
                cache_dir: root.path().join("torrent-cache"),
                enable_dht: false,
                ..Default::default()
            },
            &workers.proxy.url(),
        )
        .unwrap(),
    );
    let workers = Arc::new(workers);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (opened, opening) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut header = vec![0; 4096];
        let _ = socket.read(&mut header).await.unwrap();
        let _ = opened.send(());
        let _ = socket.read(&mut header).await;
    });
    let mut request = tests::request(3840);
    request.input = serde_json::from_value(
        serde_json::json!({"url":format!("http://{address}/first.torrent"),"live":false}),
    )
    .unwrap();
    let first = tokio::spawn({
        let workers = workers.clone();
        let principal = principal.clone();
        let request = request.clone();
        async move { workers.create(&principal, &request, "first", 0).await }
    });
    tokio::time::timeout(Duration::from_secs(3), opening)
        .await
        .unwrap()
        .unwrap();
    request.input.url = format!("http://{address}/other.torrent");
    assert_eq!(
        workers.create(&principal, &request, "other", 0).await.err(),
        Some("input_capacity")
    );
    assert_eq!(workers.available(&principal, 0).unwrap().inputs, 0);
    first.abort();
    let _ = first.await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while workers.available(&principal, 0).unwrap().inputs == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(workers.available(&principal, 0).unwrap().inputs, 1);
    server.abort();
}

#[tokio::test]
async fn disabled_torrent_request_has_no_media_or_peer_side_effects() {
    let (_root, workers, principal) = tests::fixture(false).await;
    let mut request = tests::request(3840);
    request.input = serde_json::from_value(serde_json::json!({"url":"magnet:?xt=urn:btih:1111111111111111111111111111111111111111","live":false})).unwrap();
    assert_eq!(
        workers
            .create(&principal, &request, "disabled", 0)
            .await
            .err(),
        Some("unsupported_output")
    );
    assert_eq!(workers.engine.available_output_slots(), 1);
    assert!(!workers.torrent_enabled());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires real FFmpeg/ffprobe; deterministic local BitTorrent fixture"]
async fn real_torrent_viewers_share_output_seek_and_reap_owned_input() {
    real_torrent_media_fixture(None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires real FFmpeg/ffprobe; deterministic local BitTorrent fixture"]
async fn rolling_torrent_viewers_decode_seek_with_bounded_input() {
    real_torrent_media_fixture(Some(64 * 1024)).await;
}

async fn real_torrent_media_fixture(piece_cache_bytes: Option<u64>) {
    let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").expect("FFmpeg fixture path");
    let ffprobe = std::env::var("PLAYBACK_TEST_FFPROBE").expect("ffprobe fixture path");
    let root = tempfile::tempdir().unwrap();
    let seed_dir = root.path().join("seed");
    std::fs::create_dir(&seed_dir).unwrap();
    let movie = seed_dir.join("clip.mkv");
    let generated = tokio::process::Command::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-nostdin",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=25",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "12",
            "-c:v",
            "libx264",
            "-threads",
            "2",
            "-preset",
            "ultrafast",
            "-g",
            "25",
            "-c:a",
            "aac",
        ])
        .arg(&movie)
        .output()
        .await
        .unwrap();
    assert!(
        generated.status.success(),
        "fixture media generation failed"
    );
    if let Some(budget) = piece_cache_bytes {
        assert!(std::fs::metadata(&movie).unwrap().len() > budget * 4);
    }
    let metainfo = create_torrent(
        &movie,
        CreateTorrentOptions {
            piece_length: Some(16 * 1024),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .as_bytes()
    .unwrap();
    let peer_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_port = peer_listener.local_addr().unwrap().port();
    drop(peer_listener);
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
            AddTorrent::from_bytes(metainfo.clone()),
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
    let route = axum::Router::new().route(
        "/clip.torrent",
        axum::routing::get(move || {
            let bytes = metainfo.clone();
            async move { bytes }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let source_url = format!("http://{}/clip.torrent", listener.local_addr().unwrap());
    let http = tokio::spawn(async move { axum::serve(listener, route).await.unwrap() });
    let engine = PlaybackManager::new(playback_engine::Config {
        ffmpeg: ffmpeg.clone().into(),
        ffprobe: ffprobe.clone().into(),
        root: root.path().join("media"),
        max_sessions: 4,
        ttl: Duration::from_secs(120),
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
                label: "Torrent fixture".into(),
                namespaces: vec!["fixture".into()],
                scopes: vec![
                    Scope::Create,
                    Scope::Read,
                    Scope::Renew,
                    Scope::Release,
                    Scope::Capabilities,
                ],
                quotas: Quotas {
                    inputs: 4,
                    outputs: 4,
                    viewers: 10,
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
        NetworkPolicy::loopback_fixture(),
        playback_engine::TorrentConfig {
            enabled: true,
            piece_cache_bytes,
            cache_dir: root.path().join("torrent-cache"),
            enable_dht: false,
            initial_peers: vec![format!("127.0.0.1:{peer_port}").parse().unwrap()],
            idle_timeout: Duration::ZERO,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let mut request: CreateSession = serde_json::from_value(serde_json::json!({"namespace":"fixture","input":{"url":source_url,"live":false},"output":{"protocol":"hls","conversion":"video","video_codecs":["h264"],"audio_codecs":["aac"],"max_width":640,"max_height":360}})).unwrap();
    let first = workers
        .create(&principal, &request, "first", 0)
        .await
        .unwrap();
    let second = workers
        .create(&principal, &request, "second", 2)
        .await
        .unwrap();
    let late = workers
        .create(&principal, &request, "late", 4)
        .await
        .unwrap();
    assert_eq!(first.job_id, second.job_id);
    assert_eq!(first.job_id, late.job_id);
    assert_eq!(workers.available(&principal, 4).unwrap().inputs, 3);
    assert_eq!(workers.input_file(&first).unwrap().name, "clip.mkv");
    let token = workers
        .sessions
        .lock()
        .unwrap()
        .ensure_media(&principal, &first.id, 4)
        .unwrap()
        .token;
    let target = workers.media_target(&first.id, &token, 4).await.unwrap();
    let path = root.path().join("media").join(&target.id);
    assert_decodable(&ffmpeg, &path, root.path(), "first").await;
    request.position_seconds = 3.0;
    let sought = workers
        .create(&principal, &request, "seek", 5)
        .await
        .unwrap();
    assert_ne!(first.job_id, sought.job_id);
    let sought_response = workers
        .active
        .lock()
        .unwrap()
        .get(&sought.job_id)
        .unwrap()
        .clone();
    let sought_path = root.path().join("media").join(&sought_response.id);
    assert_decodable(&ffmpeg, &sought_path, root.path(), "sought").await;
    assert_seek_frame(&ffmpeg, &sought_path, &movie, root.path()).await;
    let manifests: Vec<_> = std::fs::read_dir(&sought_path)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|file| {
            file.path()
                .extension()
                .is_some_and(|extension| extension == "m3u8")
        })
        .collect();
    assert!(!manifests.is_empty());
    if let Some(budget) = piece_cache_bytes {
        let caches: Vec<_> = std::fs::read_dir(root.path().join("torrent-cache"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path().join("pieces.cache"))
            .filter(|p| p.exists())
            .collect();
        assert_eq!(caches.len(), 1);
        assert!(std::fs::metadata(&caches[0]).unwrap().len() <= budget);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert!(std::fs::metadata(&caches[0]).unwrap().blocks() * 512 <= budget);
        }
    }
    for viewer in [&first, &second, &late, &sought] {
        workers
            .sessions
            .lock()
            .unwrap()
            .release(&principal, &viewer.id, 6)
            .unwrap();
    }
    workers.reap(40).await.unwrap();
    assert_eq!(workers.available(&principal, 40).unwrap().inputs, 4);
    assert_eq!(workers.engine.available_output_slots(), 4);
    assert!(workers.input_sources.lock().unwrap().is_empty());
    workers.shutdown().await;
    http.abort();
    seed.cancellation_token().cancel();
}

async fn assert_seek_frame(ffmpeg: &str, media: &Path, movie: &Path, root: &Path) {
    let playlist = std::fs::read_dir(media)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "m3u8")
        })
        .unwrap();
    for (label, input, seconds) in [
        ("seek-observed", playlist.as_path(), 0.0),
        ("seek-expected", movie, 3.0),
        ("seek-zero", movie, 0.0),
    ] {
        let mut command = tokio::process::Command::new(ffmpeg);
        command.args(["-v", "error", "-nostdin"]);
        if seconds > 0.0 {
            command.args(["-ss", &seconds.to_string()]);
        }
        let result = command
            .arg("-i")
            .arg(input)
            .args(["-frames:v", "1", "-pix_fmt", "rgb24", "-f", "rawvideo"])
            .arg(root.join(label))
            .output()
            .await
            .unwrap();
        assert!(result.status.success());
    }
    let observed = std::fs::read(root.join("seek-observed")).unwrap();
    let expected = std::fs::read(root.join("seek-expected")).unwrap();
    let zero = std::fs::read(root.join("seek-zero")).unwrap();
    assert_eq!(observed.len(), 320 * 180 * 3);
    assert_eq!(observed.len(), expected.len());
    let mse = |reference: &[u8]| {
        observed
            .iter()
            .zip(reference)
            .map(|(a, b)| {
                let delta = f64::from(*a) - f64::from(*b);
                delta * delta
            })
            .sum::<f64>()
            / observed.len() as f64
    };
    assert!(
        mse(&expected) < 100.0,
        "seek first decoded frame must match requested source time; mse={} zero={}",
        mse(&expected),
        mse(&zero)
    );
    assert!(
        mse(&expected) < mse(&zero) * 0.1,
        "seek must not restart at time zero"
    );
}

async fn assert_decodable(ffmpeg: &str, media: &Path, root: &Path, label: &str) {
    let playlist = std::fs::read_dir(media)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "m3u8")
        })
        .expect("output playlist");
    let result = tokio::process::Command::new(ffmpeg)
        .args([
            "-v",
            "error",
            "-nostdin",
            "-protocol_whitelist",
            "file,crypto,data",
            "-i",
        ])
        .arg(playlist)
        .args(["-frames:v", "8", "-f", "framemd5"])
        .arg(root.join(format!("{label}.md5")))
        .output()
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "managed torrent output must decode"
    );
    let frames = std::fs::read_to_string(root.join(format!("{label}.md5"))).unwrap();
    assert!(frames.lines().filter(|line| !line.starts_with('#')).count() >= 8);
}
