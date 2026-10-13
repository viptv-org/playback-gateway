//! Qualify the HTTP relay through actual inspection, remux and video decoding.
use super::*;
use crate::credentials::{KeySpec, Quotas};
use axum::{
    body::Body,
    http::{HeaderMap, Response},
    routing::get,
    Router,
};
use std::os::unix::fs::PermissionsExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires real FFmpeg/ffprobe; isolated authenticated ranged MP4"]
async fn real_header_media_uses_owned_capability_without_child_credentials_and_decodes() {
    let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").expect("real FFmpeg path");
    let ffprobe = std::env::var("PLAYBACK_TEST_FFPROBE").expect("real FFprobe path");
    let root = tempfile::tempdir().unwrap();
    let movie = root.path().join("source.mp4");
    let generation = tokio::process::Command::new(&ffmpeg)
        .kill_on_drop(true)
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
            "8",
            "-c:v",
            "libx264",
            "-threads",
            "2",
            "-preset",
            "ultrafast",
            "-g",
            "50",
            "-c:a",
            "aac",
            "-movflags",
            "+faststart",
        ])
        .arg(&movie)
        .output()
        .await
        .unwrap();
    assert!(generation.status.success());
    let bytes = Arc::new(std::fs::read(movie).unwrap());
    let contacts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let router = Router::new().route(
        "/media",
        get({
            let contacts = contacts.clone();
            move |headers: HeaderMap| {
                let bytes = bytes.clone();
                let contacts = contacts.clone();
                async move {
                    assert_eq!(headers.get("x-api-key").unwrap(), "header-private-fixture");
                    assert_eq!(
                        headers.get("authorization").unwrap(),
                        "Bearer header-private-fixture"
                    );
                    assert!(!headers.contains_key("x-playback-egress-proxy"));
                    contacts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let range = headers
                        .get("range")
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .strip_prefix("bytes=")
                        .unwrap();
                    let (start, end) = range.split_once('-').unwrap();
                    let (start, end) = (
                        start.parse::<usize>().unwrap(),
                        end.parse::<usize>().unwrap(),
                    );
                    Response::builder()
                        .status(206)
                        .header(
                            "content-range",
                            format!("bytes {start}-{end}/{}", bytes.len()),
                        )
                        .body(Body::from(bytes[start..=end].to_vec()))
                        .unwrap()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let input_url = format!("http://{}/media", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    // Wrappers fail if any original credential or upstream URL reaches a child.
    // Markers record only that the executable ran; they never record its argv.
    let mut wrappers = Vec::new();
    for (name, executable) in [("ffmpeg", &ffmpeg), ("ffprobe", &ffprobe)] {
        let wrapper = root.path().join(name);
        let quote = |value: &str| format!("'{}'", value.replace('\'', "'\"'\"'"));
        std::fs::write(&wrapper, format!("#!/bin/sh\nfor arg do case \"$arg\" in *header-private-fixture*|*{}*) exit 98 ;; esac; done\nprintf x >> \"$0.called\"\nexec {} \"$@\"\n", input_url, quote(executable))).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        wrappers.push(wrapper);
    }
    let engine = PlaybackManager::new(playback_engine::Config {
        ffmpeg: wrappers[0].clone(),
        ffprobe: wrappers[1].clone(),
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
                label: "HTTP header fixture".into(),
                namespaces: vec!["fixture".into()],
                scopes: vec![Scope::Create, Scope::Read, Scope::Renew, Scope::Release],
                quotas: Quotas {
                    inputs: 1,
                    outputs: 1,
                    viewers: 2,
                },
                expires_at: None,
            },
            0,
        )
        .unwrap();
    let principal = credentials.authenticate(&key.secret, 0).unwrap();
    let workers = Workers::new(engine, credentials, NetworkPolicy::loopback_fixture())
        .await
        .unwrap();
    let mut request = tests::request(640);
    request.input.live = false;
    request.input.url = input_url;
    request.input.headers = BTreeMap::from([
        ("X-api-key".into(), "header-private-fixture".into()),
        (
            "Authorization".into(),
            "Bearer header-private-fixture".into(),
        ),
    ]);
    let first = workers
        .create(&principal, &request, "first", 0)
        .await
        .unwrap();
    assert_eq!(first.status, Status::Ready);
    let second = workers
        .create(&principal, &request, "second", 1)
        .await
        .unwrap();
    assert_eq!(first.job_id, second.job_id);
    assert!(workers.input_file(&first).is_none());
    assert!(root.path().join("ffmpeg.called").exists());
    assert!(root.path().join("ffprobe.called").exists());
    assert!(contacts.load(std::sync::atomic::Ordering::SeqCst) > 2);
    let target = workers.active.lock().unwrap()[&first.job_id].clone();
    let playlist = root
        .path()
        .join("media")
        .join(&target.id)
        .join("index.m3u8");
    let decoded = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(&ffmpeg)
            .kill_on_drop(true)
            .args([
                "-v",
                "error",
                "-protocol_whitelist",
                "file,crypto,data",
                "-i",
            ])
            .arg(playlist)
            .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(decoded.status.success());
    assert_eq!(decoded.stdout.len(), 320 * 180 * 3);
    workers
        .credentials
        .revoke(tests::BOOTSTRAP, &principal.key.id)
        .unwrap();
    workers.reap(2).await.unwrap();
    workers.reap(18).await.unwrap();
    assert_eq!(workers.engine.active_count().await, 0);
    assert_eq!(workers.sessions.lock().unwrap().job_count(), 0);
    workers.shutdown().await;
    server.abort();
}
