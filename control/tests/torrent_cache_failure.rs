#![cfg(target_os = "linux")]
//! Ready-viewer ENOSPC on a kernel-bounded torrent cache through the service API.
use librqbit::{create_torrent, CreateTorrentOptions};
use serde_json::{json, Value};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    process::{Child, Command},
};
mod support;
#[path = "support/torrent.rs"]
mod torrent_fixture;
const BOOTSTRAP: &str = "torrent-cache-fixture-bootstrap-not-production";

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

fn allocation(path: &Path) -> (u64, u64) {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    let mut status = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    assert_eq!(
        unsafe { libc::statvfs(path.as_ptr(), status.as_mut_ptr()) },
        0
    );
    let status = unsafe { status.assume_init() };
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
    assert_eq!(
        unsafe { libc::statfs(path.as_ptr(), filesystem.as_mut_ptr()) },
        0
    );
    assert_eq!(
        unsafe { filesystem.assume_init() }.f_type,
        libc::TMPFS_MAGIC,
        "cache must be the fixture's real tmpfs mount"
    );
    (
        status.f_blocks * status.f_frsize,
        (status.f_blocks - status.f_bfree) * status.f_frsize,
    )
}
fn control_url(base: &str, viewer: &Value) -> String {
    format!("{base}/v1/sessions/{}", viewer["id"].as_str().unwrap())
}
async fn reclaimed(client: &reqwest::Client, base: &str, key: &str) {
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let caps = reply(
                client
                    .get(format!("{base}/v1/capabilities"))
                    .bearer_auth(key)
                    .send()
                    .await
                    .map_err(|_| "capacity request failed")
                    .unwrap(),
            )
            .await;
            if caps["available"]["inputs"] == 1
                && caps["available"]["outputs"] == 1
                && caps["available"]["viewers"] == 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("failed/released viewer must reclaim admission");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires real service/FFmpeg image, network none, NET_ADMIN and 2 MiB cache tmpfs"]
async fn standalone_ready_torrent_cache_enospc_fails_reclaims_and_recovers() {
    use std::io::Write;
    support::isolated_address();
    assert_eq!(
        std::env::var("PLAYBACK_TEST_DATA_DIR").as_deref(),
        Ok("/fixture/state")
    );
    let data = Path::new("/fixture/state");
    let cache = data.join("torrent-cache");
    assert_eq!(
        allocation(&cache).0,
        2 * 1024 * 1024,
        "dedicated kernel cache bound"
    );
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
            "-b:v",
            "256k",
            "-minrate",
            "256k",
            "-maxrate",
            "256k",
            "-bufsize",
            "256k",
            "-x264-params",
            "nal-hrd=cbr",
            "-c:a",
            "aac",
            "-b:a",
            "32k",
        ])
        .arg(&movie)
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(generated.status.success(), "synthetic clip must encode");
    let payload = std::fs::read(&movie).unwrap();
    assert!(
        payload.len() > 2 * 1024 * 1024,
        "payload cannot fit the bounded cache"
    );
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
    let recovered_movie = root.path().join("recovered.mkv");
    let recovered_media = Command::new(&ffmpeg)
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(&movie)
        .args(["-t", "4", "-c", "copy"])
        .arg(&recovered_movie)
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(
        recovered_media.status.success(),
        "small recovery clip must remux"
    );
    let recovered_payload = std::fs::read(&recovered_movie).unwrap();
    assert!(
        recovered_payload.len() < 256 * 1024,
        "recovery payload remains bounded"
    );
    let recovered_torrent = create_torrent(
        &recovered_movie,
        CreateTorrentOptions {
            piece_length: Some(16 * 1024),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let recovered_hash = recovered_torrent.info_hash().0;
    let recovered_metainfo = recovered_torrent.as_bytes().unwrap();
    let listener = TcpListener::bind("11.255.255.1:0").await.unwrap();
    let source = format!("http://{}/clip.torrent", listener.local_addr().unwrap());
    let recovered_source = format!(
        "http://{}/recovered.torrent",
        listener.local_addr().unwrap()
    );
    let route = axum::Router::new()
        .route(
            "/clip.torrent",
            axum::routing::get(move || {
                let bytes = metainfo.clone();
                async move { bytes }
            }),
        )
        .route(
            "/recovered.torrent",
            axum::routing::get(move || {
                let bytes = recovered_metainfo.clone();
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
    let mut child = Command::new(
        std::env::var("PLAYBACK_TEST_SERVICE_BINARY")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_playback-gateway").into()),
    )
    .env("API_KEY", BOOTSTRAP)
    .env("DATA_DIR", data)
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
    .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(35))
        .build()
        .unwrap();
    let base = format!("http://{address}");
    ready(&mut child, &client, &base).await;
    let issued = reply(client.post(format!("{base}/v1/keys")).bearer_auth(BOOTSTRAP)
        .json(&json!({"label":"ready cache full fixture","namespaces":["fixture"],
          "scopes":["capabilities","create","read","renew","release"],"quotas":{"inputs":1,"outputs":1,"viewers":2}}))
        .send().await.map_err(|_| "key request failed").unwrap()).await;
    let key = issued["secret"].as_str().unwrap().to_owned();
    let admission = {
        let client = client.clone();
        let base = base.clone();
        let key = key.clone();
        let source = source.clone();
        tokio::spawn(async move { session(&client, &base, &key, &source, "before-full").await })
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if TcpStream::connect(peer_address).await.is_ok() {
                break;
            }
            assert!(
                !admission.is_finished(),
                "admission ended before peer listener"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("actual peer listener deadline");
    let mut seed = AbortTask(tokio::spawn(torrent_fixture::seed_peer(
        peer_address,
        hash,
        payload.clone(),
        Duration::from_millis(20),
    )));
    let viewer = tokio::time::timeout(Duration::from_secs(30), admission)
        .await
        .expect("ready torrent admission deadline")
        .unwrap();
    assert_eq!(
        viewer["status"], "ready",
        "ENOSPC must affect an already-ready viewer"
    );
    playback(&client, &base, &viewer, &ffmpeg).await;
    let used = allocation(&cache).1;
    assert!(
        used < payload.len() as u64 && used < 2 * 1024 * 1024,
        "ready decoded playback must precede exhaustion with missing payload pieces"
    );
    println!(
        "Ready public HLS decoded before failure; payload={} cache_allocated={} bytes.",
        payload.len(),
        used
    );
    let fill = cache.join("capacity-fixture.bin");
    let mut file = std::fs::File::create(&fill).unwrap();
    loop {
        match file.write_all(&[0u8; 4096]) {
            Ok(()) => {}
            Err(error) => {
                assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
                break;
            }
        }
    }
    assert_eq!(allocation(&cache).1, 2 * 1024 * 1024);
    println!("Actual torrent-cache tmpfs reached ENOSPC at 2097152 allocated bytes.");
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            let state = reply(
                client
                    .get(control_url(&base, &viewer))
                    .bearer_auth(&key)
                    .send()
                    .await
                    .map_err(|_| "viewer state request failed")
                    .unwrap(),
            )
            .await;
            if state["status"] == "failed" {
                assert_eq!(state["error_code"], "processing_failed");
                assert!(
                    state.get("playback").is_none(),
                    "failed viewer must stop advertising playback"
                );
                break;
            }
            assert_eq!(
                state["status"], "ready",
                "storage failure must not expire the viewer"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("ENOSPC must fail the already-ready public viewer");
    println!("Ready viewer transitioned to failed/processing_failed after cache exhaustion.");
    let media = format!("{base}{}", viewer["playback"]["url"].as_str().unwrap());
    assert!(
        !client
            .get(&media)
            .send()
            .await
            .map_err(|_| "failed media request failed")
            .unwrap()
            .status()
            .is_success(),
        "failed viewer media must be refused"
    );
    reclaimed(&client, &base, &key).await;
    tokio::time::timeout(Duration::from_secs(5), &mut seed.0)
        .await
        .expect("failed peer transport must stop")
        .expect("failed peer task must exit");
    drop(file);
    std::fs::remove_file(fill).unwrap();
    assert!(allocation(&cache).1 < 2 * 1024 * 1024);
    assert!(client
        .delete(control_url(&base, &viewer))
        .bearer_auth(&key)
        .send()
        .await
        .map_err(|_| "failed viewer release failed")
        .unwrap()
        .status()
        .is_success());
    // The small distinct source fits beside the retained idle cache. Recovery
    // must use the same still-running service and unchanged storage policy.
    let fresh_admission = {
        let client = client.clone();
        let base = base.clone();
        let key = key.clone();
        tokio::spawn(
            async move { session(&client, &base, &key, &recovered_source, "after-full").await },
        )
    };
    let mut fresh_seed = AbortTask(tokio::spawn(torrent_fixture::seed_peer(
        peer_address,
        recovered_hash,
        recovered_payload,
        Duration::ZERO,
    )));
    let fresh = tokio::time::timeout(Duration::from_secs(30), fresh_admission)
        .await
        .expect("recovered torrent admission deadline")
        .unwrap();
    assert_eq!(fresh["status"], "ready");
    playback(&client, &base, &fresh, &ffmpeg).await;
    assert!(client
        .delete(control_url(&base, &fresh))
        .bearer_auth(&key)
        .send()
        .await
        .map_err(|_| "fresh viewer release failed")
        .unwrap()
        .status()
        .is_success());
    reclaimed(&client, &base, &key).await;
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
        .expect("fresh peer transport must stop on shutdown")
        .expect("fresh peer task must exit");
    println!("Failed peer stopped and admission reclaimed; removing cache pressure restored real HTTP decode in the same process; fresh release reclaimed all admission.");
}
