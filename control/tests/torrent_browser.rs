#![cfg(target_os = "linux")]
//! Actual enabled service binary, public API and HTTP media in network-none Docker.
use librqbit::{create_torrent, CreateTorrentOptions};
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    process::{Child, Command},
};
const BOOTSTRAP: &str = "torrent-runtime-fixture-bootstrap-not-production";
#[allow(dead_code)]
#[path = "support/torrent.rs"]
mod torrent_fixture;
use torrent_fixture::{seed_peer_observed, SeedProgress};
mod support;
use support::isolated_address;

async fn json_reply(response: reqwest::Response) -> Value {
    let status = response.status();
    let value: Value = response
        .json()
        .await
        .map_err(|_| "invalid fixture JSON response")
        .unwrap();
    if !status.is_success() {
        // Only fixed documented error codes may enter fixture diagnostics.
        let code = match value["error"]["code"].as_str() {
            Some("processing_failed") => "processing_failed",
            Some("source_preparation_failed") => "source_preparation_failed",
            Some("startup_timeout") => "startup_timeout",
            Some("input_capacity") => "input_capacity",
            Some("output_capacity") => "output_capacity",
            Some("viewer_capacity") => "viewer_capacity",
            _ => "other",
        };
        panic!("public API rejected fixture with status {status}, code {code}");
    }
    value
}
async fn service_ready(child: &mut Child, client: &reqwest::Client, base: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "service exited before ready"
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "trusted HTTPS browser fixture, network none and isolated driver NET_ADMIN"]
async fn standalone_torrent_browser_uid10001() {
    isolated_address();
    let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").unwrap();
    let root = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let state = root.path().join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let owner = std::ffi::CString::new(state.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::chown(owner.as_ptr(), 10001, 10001) }, 0);
    let seed_dir = root.path().join("seed");
    std::fs::create_dir(&seed_dir).unwrap();
    let movie = seed_dir.join("clip.mkv");
    let dual_audio = std::env::var("PLAYBACK_TEST_DUAL_AUDIO").as_deref() == Ok("true");
    let mut encoder = Command::new(&ffmpeg);
    encoder.args([
        "-v",
        "error",
        "-nostdin",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x180:rate=25",
    ]);
    if dual_audio {
        encoder.args([
            "-f",
            "lavfi",
            "-i",
            "anullsrc=channel_layout=stereo:sample_rate=48000",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=channel_layout=stereo:sample_rate=48000",
        ]);
    } else {
        encoder.args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"]);
    }
    encoder.args([
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
        "-colorspace",
        "bt709",
        "-color_primaries",
        "bt709",
        "-color_trc",
        "bt709",
    ]);
    if dual_audio {
        encoder.args([
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-map",
            "2:a:0",
            "-metadata:s:a:0",
            "language=eng",
            "-metadata:s:a:0",
            "title=English",
            "-metadata:s:a:1",
            "language=spa",
            "-metadata:s:a:1",
            "title=Spanish",
        ]);
    }
    let generated = encoder
        .arg(&movie)
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(generated.status.success(), "generated clip must encode");
    let movie_size = std::fs::metadata(&movie).unwrap().len();
    let torrent = create_torrent(
        &movie,
        CreateTorrentOptions {
            piece_length: Some(16 * 1024),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let metainfo = torrent.as_bytes().unwrap();
    let info_hash = torrent.info_hash().0;
    let listener = TcpListener::bind("11.255.255.1:0").await.unwrap();
    let source_url = format!("http://{}/clip.torrent", listener.local_addr().unwrap());
    let route_bytes = metainfo.clone();
    let route = axum::Router::new().route(
        "/clip.torrent",
        axum::routing::get(move || {
            let bytes = route_bytes.clone();
            async move { bytes }
        }),
    );
    let http = tokio::spawn(async move {
        let _ = axum::serve(listener, route).await;
    });
    struct AbortHttp(tokio::task::JoinHandle<()>);
    impl Drop for AbortHttp {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _http = AbortHttp(http);
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
    .uid(10001)
    .gid(10001)
    .env("API_KEY", BOOTSTRAP)
    .env("DATA_DIR", root.path().join("state"))
    .env("BIND_ADDRESS", address.to_string())
    .env("TORRENT_ENABLED", "true")
    .env("TORRENT_LISTEN_PORT", peer_address.port().to_string())
    .env("FFMPEG_PATH", &ffmpeg)
    .env(
        "FFPROBE_PATH",
        std::env::var("PLAYBACK_TEST_FFPROBE").unwrap(),
    )
    .env("MAX_OUTPUTS", "2")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .kill_on_drop(true)
    .spawn()
    .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(25))
        .build()
        .unwrap();
    let base = format!("http://{address}");
    service_ready(&mut child, &client, &base).await;
    let issued = json_reply(client.post(format!("{base}/v1/keys")).bearer_auth(BOOTSTRAP)
        .json(&json!({"label":"actual torrent HTTP fixture","namespaces":["fixture"],
            "scopes":["capabilities","create","read","renew","release"],"quotas":{"inputs":2,"outputs":2,"viewers":4}}))
        .send().await.map_err(|_|"key request failed").unwrap()).await;
    let key = issued["secret"]
        .as_str()
        .expect("fixture integration key")
        .to_owned();
    let process_status =
        std::fs::read_to_string(format!("/proc/{}/status", child.id().unwrap())).unwrap();
    assert!(process_status
        .lines()
        .any(|line| line == "Uid:\t10001\t10001\t10001\t10001"));
    assert!(process_status
        .lines()
        .any(|line| line == "CapEff:\t0000000000000000"));
    assert!(process_status.lines().any(|line| line == "NoNewPrivs:\t1"));
    let shared = std::path::Path::new("/qualification");
    let reference = Command::new(&ffmpeg)
        .args(["-v", "error", "-nostdin", "-ss", "3", "-i"])
        .arg(&movie)
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
        .output()
        .await
        .unwrap();
    assert!(reference.status.success());
    assert_eq!(reference.stdout.len(), 320 * 180 * 3);
    std::fs::write(shared.join("expected3.rgb"), reference.stdout).unwrap();
    let bytes = std::fs::read(&movie).unwrap();
    let progress = std::sync::Arc::new(SeedProgress::default());
    let seed_progress = progress.clone();
    let seed = tokio::spawn(async move {
        loop {
            if TcpStream::connect(peer_address).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        seed_peer_observed(
            peer_address,
            info_hash,
            bytes,
            Duration::from_millis(80),
            Some(seed_progress),
        )
        .await;
    });
    let socket = tokio::net::UnixListener::bind(shared.join("gateway.sock")).unwrap();
    std::fs::set_permissions(
        shared.join("gateway.sock"),
        std::fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    let proxy = tokio::spawn(async move {
        loop {
            let Ok((mut client, _)) = socket.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let Ok(mut upstream) = TcpStream::connect(address).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    std::fs::write(
        shared.join("fixture.json"),
        json!({"source":source_url,"key":key,"movie_size":movie_size}).to_string(),
    )
    .unwrap();
    std::fs::set_permissions(
        shared.join("fixture.json"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    println!("UID10001 actual gateway ready; guarded isolated torrent peer and Unix bridge ready.");
    fn payloads(path: &std::path::Path) -> usize {
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return 0,
            Err(_) => panic!("fixture cache inventory unavailable"),
        };
        entries
            .map(|entry| {
                let p = entry.expect("fixture cache entry unavailable").path();
                if p.is_dir() {
                    payloads(&p)
                } else {
                    usize::from(p.extension().is_some_and(|x| x == "mkv"))
                }
            })
            .sum()
    }
    let cache = root.path().join("state/torrent-cache");
    let mut observed_payloads = 0;
    tokio::time::timeout(Duration::from_secs(900), async {
        while !shared.join("stop").exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "gateway must stay alive"
            );
            observed_payloads = observed_payloads.max(payloads(&cache));
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("browser fixture deadline");
    let seeded_bytes = progress.bytes.load(std::sync::atomic::Ordering::Relaxed);
    let seeded_millis = progress
        .elapsed_millis
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        seeded_bytes > 1024 * 1024,
        "bounded sustained peer must deliver more than one MiB"
    );
    assert!(
        seeded_millis >= 15_000,
        "bounded sustained peer must transfer for at least fifteen seconds"
    );
    println!(
        "Bounded deterministic peer delivered {} bytes over {} milliseconds.",
        seeded_bytes, seeded_millis
    );
    assert!(
        observed_payloads > 0,
        "active browser playback must allocate a torrent payload"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while payloads(&cache) > 0 {
            assert!(child.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("service default prompt idle-cache eviction after admission reclamation");
    assert!(client
        .get(format!("{base}/ready"))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    tokio::time::timeout(Duration::from_secs(5), seed)
        .await
        .expect("idle peer must stop")
        .expect("peer fixture must not fail");
    println!("Active cache payload observed; default service reaper removed idle payload and stopped peer after final release; readiness retained.");
    assert_eq!(
        unsafe { libc::kill(child.id().unwrap() as libc::pid_t, libc::SIGTERM) },
        0
    );
    assert!(child.wait().await.unwrap().success());
    proxy.abort();
    println!("Owned browser fixture stopped successfully.");
}
