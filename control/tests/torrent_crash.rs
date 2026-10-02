#![cfg(target_os = "linux")]
//! One synthetic torrent crash/restart case through the actual service binary.
use librqbit::{create_torrent, CreateTorrentOptions};
use serde_json::{json, Value};
use std::{collections::BTreeSet, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
};
mod support;
#[path = "support/torrent.rs"]
mod torrent_fixture;
const BOOTSTRAP: &str = "torrent-crash-fixture-bootstrap-not-production";

async fn reply(response: reqwest::Response) -> Value {
    assert!(response.status().is_success(), "fixture control API failed");
    response
        .json()
        .await
        .map_err(|_| "fixture JSON invalid")
        .unwrap()
}
async fn ready(child: &mut Child, client: &reqwest::Client, base: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "service exited before readiness"
            );
            if client
                .get(format!("{base}/ready"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("service readiness deadline");
}
async fn session(client: &reqwest::Client, base: &str, key: &str, source: &str, id: &str) -> Value {
    reply(client.post(format!("{base}/v1/sessions")).bearer_auth(key)
        .header("Idempotency-Key", id)
        .json(&json!({"namespace":"fixture","input":{"url":source,"live":false,"file_index":0},
          "output":{"protocol":"hls","video_codecs":["h264"],"audio_codecs":["aac"],"max_width":320,"max_height":180}}))
        .send().await.map_err(|_| "session request failed").unwrap()).await
}
fn media_dirs(data: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(data.join("media"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}
fn media_children(parent: u32) -> BTreeSet<u32> {
    let mut pids = BTreeSet::new();
    for task in std::fs::read_dir(format!("/proc/{parent}/task"))
        .unwrap()
        .flatten()
    {
        if let Ok(text) = std::fs::read_to_string(task.path().join("children")) {
            pids.extend(
                text.split_whitespace()
                    .filter_map(|p| p.parse::<u32>().ok()),
            );
        }
    }
    pids.into_iter()
        .filter(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .is_ok_and(|name| matches!(name.trim(), "ffmpeg" | "ffprobe"))
        })
        .collect()
}
async fn gone(pids: &BTreeSet<u32>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while pids
            .iter()
            .any(|pid| Path::new(&format!("/proc/{pid}")).exists())
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("owned media children must be reaped after owner death");
}
fn assert_locks_released(data: &Path) {
    for name in ["instance.lock", "torrent-cache/.gateway-lock"] {
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(data.join(name))
            .unwrap();
        assert!(
            lock.try_lock().is_ok(),
            "OS-held storage lock must release after owner exit"
        );
    }
}
async fn playback(client: &reqwest::Client, base: &str, viewer: &Value, ffmpeg: &str) {
    let media = format!(
        "{base}{}",
        viewer["playback"]["url"]
            .as_str()
            .expect("viewer media URL")
    );
    let response = client
        .get(&media)
        .send()
        .await
        .map_err(|_| "playlist request failed")
        .unwrap();
    assert!(
        response.status().is_success(),
        "ready viewer playlist must be served"
    );
    let decoded = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new(ffmpeg)
            .args([
                "-v",
                "error",
                "-nostdin",
                "-protocol_whitelist",
                "file,http,tcp,crypto,data",
                "-i",
            ])
            .arg(&media)
            .args([
                "-threads",
                "2",
                "-frames:v",
                "1",
                "-pix_fmt",
                "rgb24",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("HTTP decode deadline")
    .unwrap();
    assert!(decoded.status.success(), "actual HTTP HLS must decode");
    assert_eq!(decoded.stdout.len(), 320 * 180 * 3);
}
struct AbortTask(tokio::task::JoinHandle<()>);
impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires real service/FFmpeg image, Docker network none and NET_ADMIN"]
async fn standalone_torrent_crash_reaps_children_releases_locks_and_restarts() {
    support::isolated_address();
    let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").unwrap();
    let root = tempfile::tempdir().unwrap();
    let movie = root.path().join("clip.mkv");
    let generated = Command::new(&ffmpeg)
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
            "120",
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
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(generated.status.success(), "synthetic clip must encode");
    let payload = std::fs::read(&movie).unwrap();
    let torrent = create_torrent(
        &movie,
        CreateTorrentOptions {
            piece_length: Some(16 * 1024),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let hash = torrent.info_hash().0;
    let metainfo = torrent.as_bytes().unwrap();
    let listener = TcpListener::bind("11.255.255.1:0").await.unwrap();
    let source = format!("http://{}/clip.torrent", listener.local_addr().unwrap());
    let route = axum::Router::new().route(
        "/clip.torrent",
        axum::routing::get(move || {
            let bytes = metainfo.clone();
            async move { bytes }
        }),
    );
    let _http = AbortTask(tokio::spawn(async move {
        let _ = axum::serve(listener, route).await;
    }));
    let reservation = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let reservation = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_address = reservation.local_addr().unwrap();
    drop(reservation);
    let data = root.path().join("state");
    let spawn = || {
        Command::new(
            std::env::var("PLAYBACK_TEST_SERVICE_BINARY")
                .unwrap_or_else(|_| env!("CARGO_BIN_EXE_playback-gateway").into()),
        )
        .env("API_KEY", BOOTSTRAP)
        .env("DATA_DIR", &data)
        .env("BIND_ADDRESS", address.to_string())
        .env("TORRENT_ENABLED", "true")
        .env("TORRENT_LISTEN_PORT", peer_address.port().to_string())
        .env("FFMPEG_PATH", &ffmpeg)
        .env(
            "FFPROBE_PATH",
            std::env::var("PLAYBACK_TEST_FFPROBE").unwrap(),
        )
        .env("MAX_OUTPUTS", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
    };
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(25))
        .build()
        .unwrap();
    let base = format!("http://{address}");
    let mut child = spawn();
    ready(&mut child, &client, &base).await;
    let issued = reply(client.post(format!("{base}/v1/keys")).bearer_auth(BOOTSTRAP)
        .json(&json!({"label":"synthetic crash fixture","namespaces":["fixture"],
            "scopes":["capabilities","create","read","release"],"quotas":{"inputs":1,"outputs":1,"viewers":1}}))
        .send().await.map_err(|_| "key request failed").unwrap()).await;
    let key = issued["secret"]
        .as_str()
        .expect("integration key")
        .to_owned();
    let admission = {
        let client = client.clone();
        let base = base.clone();
        let key = key.clone();
        let source = source.clone();
        tokio::spawn(async move { session(&client, &base, &key, &source, "before-crash").await })
    };
    // A slow synthetic peer keeps the real output child active at the crash.
    let mut seed = AbortTask(tokio::spawn(torrent_fixture::seed_peer(
        peer_address,
        hash,
        payload.clone(),
        Duration::from_millis(20),
    )));
    let old = tokio::time::timeout(Duration::from_secs(30), admission)
        .await
        .expect("ready torrent admission deadline")
        .unwrap();
    playback(&client, &base, &old, &ffmpeg).await;
    let pids = media_children(child.id().unwrap());
    assert!(
        !pids.is_empty(),
        "ready torrent must have an active owned media child at the crash"
    );
    println!(
        "Owned active media child count before SIGKILL: {}.",
        pids.len()
    );
    let stale_media = media_dirs(&data);
    assert_eq!(stale_media.len(), 1, "one active media directory");
    let stale_cache: Vec<_> = std::fs::read_dir(data.join("torrent-cache"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("torrent-"))
        .map(|e| e.path())
        .collect();
    assert!(
        !stale_cache.is_empty(),
        "active torrent must own cached payload"
    );
    // Force a bounded, unread public media response after real media decoded.
    // Sparse bytes exercise transport backpressure, not codec qualification.
    let large = std::fs::File::create(stale_media[0].join("segment-999999999.ts")).unwrap();
    large.set_len(32 * 1024 * 1024).unwrap();
    drop(large);
    let media_path = old["playback"]["url"]
        .as_str()
        .unwrap()
        .rsplit_once('/')
        .unwrap()
        .0;
    let mut stalled = TcpStream::connect(address).await.unwrap();
    stalled.write_all(format!("GET {media_path}/segment-999999999.ts HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut header = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !header.ends_with(b"\r\n\r\n") {
            header.push(stalled.read_u8().await.unwrap());
        }
    })
    .await
    .expect("stalled HTTP response header deadline");
    assert!(
        header.starts_with(b"HTTP/1.1 200"),
        "stalled response must be authorized"
    );
    assert_eq!(
        unsafe { libc::kill(child.id().unwrap() as libc::pid_t, libc::SIGKILL) },
        0
    );
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    assert!(
        client.get(format!("{base}/ready")).send().await.is_err(),
        "control transport must stop"
    );
    let mut rest = Vec::new();
    let ended = tokio::time::timeout(Duration::from_secs(5), stalled.read_to_end(&mut rest)).await;
    assert!(
        ended.is_ok(),
        "stalled media transport must stop after owner death"
    );
    assert!(
        rest.len() < 32 * 1024 * 1024,
        "crash must interrupt the unread body"
    );
    gone(&pids).await;
    tokio::time::timeout(Duration::from_secs(5), &mut seed.0)
        .await
        .expect("synthetic peer transport must settle after owner death")
        .expect("synthetic peer task must exit without panic");
    assert_locks_released(&data);
    println!("Ready torrent owner SIGKILL interrupted public media/control transport, reaped owned media children, closed the fixture peer and released data/cache locks.");
    child = spawn();
    ready(&mut child, &client, &base).await;
    assert!(
        stale_media.iter().all(|p| !p.exists()),
        "restart must clean recognized stale media"
    );
    let denied = client
        .get(format!(
            "{base}/v1/sessions/{}",
            old["id"].as_str().unwrap()
        ))
        .bearer_auth(&key)
        .send()
        .await
        .map_err(|_| "stale control request failed")
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::NOT_FOUND);
    let denied = client
        .get(format!(
            "{base}{}",
            old["playback"]["url"].as_str().unwrap()
        ))
        .send()
        .await
        .map_err(|_| "stale media request failed")
        .unwrap();
    assert_eq!(
        denied.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "old viewer media credential must be denied after restart"
    );
    let admission = {
        let client = client.clone();
        let base = base.clone();
        let key = key.clone();
        let source = source.clone();
        tokio::spawn(async move { session(&client, &base, &key, &source, "after-crash").await })
    };
    let mut fresh_seed = AbortTask(tokio::spawn(torrent_fixture::seed_peer(
        peer_address,
        hash,
        payload,
        Duration::ZERO,
    )));
    let fresh = tokio::time::timeout(Duration::from_secs(30), admission)
        .await
        .expect("fresh torrent admission deadline")
        .unwrap();
    assert!(
        stale_cache.iter().all(|p| !p.exists()),
        "new cache owner must remove prior crashed cache"
    );
    playback(&client, &base, &fresh, &ffmpeg).await;
    let released = client
        .delete(format!(
            "{base}/v1/sessions/{}",
            fresh["id"].as_str().unwrap()
        ))
        .bearer_auth(&key)
        .send()
        .await
        .map_err(|_| "release request failed")
        .unwrap();
    assert!(released.status().is_success());
    let release_started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            let available = reply(
                client
                    .get(format!("{base}/v1/capabilities"))
                    .bearer_auth(&key)
                    .send()
                    .await
                    .map_err(|_| "capacity request failed")
                    .unwrap(),
            )
            .await;
            if available["available"]["inputs"] == 1
                && available["available"]["outputs"] == 1
                && available["available"]["viewers"] == 1
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("fresh viewer release must reclaim admission");
    println!(
        "Fresh viewer admission reclaimed in {:.2} seconds.",
        release_started.elapsed().as_secs_f64()
    );
    assert_eq!(
        unsafe { libc::kill(child.id().unwrap() as libc::pid_t, libc::SIGTERM) },
        0
    );
    assert!(tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
    tokio::time::timeout(Duration::from_secs(5), &mut fresh_seed.0)
        .await
        .expect("fresh peer transport must stop on final shutdown")
        .expect("fresh peer task must exit without panic");
    assert_locks_released(&data);
    assert!(
        media_dirs(&data).is_empty(),
        "final shutdown must remove media"
    );
    println!("Same-directory restart rejected old viewer credentials, retained the scoped integration key, decoded fresh torrent HTTP media and reclaimed admission on final release.");
}
