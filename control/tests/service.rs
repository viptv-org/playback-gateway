#![cfg(unix)]
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::Command,
};

async fn request(address: std::net::SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(bytes).unwrap()
}

#[tokio::test]
#[ignore = "requires qualified real FFmpeg and ffprobe binaries"]
async fn standalone_binary_serves_readiness_locks_storage_and_shuts_down() {
    let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").unwrap();
    let ffprobe = std::env::var("PLAYBACK_TEST_FFPROBE").unwrap();
    let root = tempfile::tempdir().unwrap();
    let port = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = port.local_addr().unwrap();
    drop(port);
    let executable = std::env::var("PLAYBACK_TEST_SERVICE_BINARY")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_playback-gateway").into());
    let configure = |command: &mut Command| {
        command
            .env("API_KEY", "standalone-fixture-bootstrap-not-production")
            .env("DATA_DIR", root.path().join("state"))
            .env("BIND_ADDRESS", address.to_string())
            .env("FFMPEG_PATH", &ffmpeg)
            .env("FFPROBE_PATH", &ffprobe)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
    };
    let mut command = Command::new(&executable);
    configure(&mut command);
    let mut child = command.spawn().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "service must remain running"
            );
            if TcpStream::connect(address).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(request(address, "/health")
        .await
        .starts_with("HTTP/1.1 200"));
    assert!(request(address, "/ready").await.starts_with("HTTP/1.1 200"));
    assert!(request(address, "/v1/capabilities")
        .await
        .starts_with("HTTP/1.1 401"));
    let mut duplicate = Command::new(&executable);
    configure(&mut duplicate);
    let rejected = tokio::time::timeout(Duration::from_secs(5), duplicate.output())
        .await
        .unwrap()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("data directory already in use"));
    assert_eq!(
        unsafe { libc::kill(child.id().unwrap() as libc::pid_t, libc::SIGTERM) },
        0
    );
    assert!(tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
    let lock = Arc::new(
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.path().join("state/instance.lock"))
            .unwrap(),
    );
    assert!(lock.try_lock().is_ok());
}

#[tokio::test]
#[ignore = "requires qualified real service/FFmpeg binaries; no public network"]
async fn standalone_torrent_runtime_off_refuses_before_source_io() {
    use serde_json::{json, Value};
    let root = tempfile::tempdir().unwrap();
    let reservation = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let source = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let source_address = source.local_addr().unwrap();
    let bootstrap = "torrent-runtime-fixture-bootstrap-not-production";
    let mut child = Command::new(
        std::env::var("PLAYBACK_TEST_SERVICE_BINARY")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_playback-gateway").into()),
    )
    .env("API_KEY", bootstrap)
    .env("DATA_DIR", root.path().join("state"))
    .env("BIND_ADDRESS", address.to_string())
    .env("TORRENT_ENABLED", "false")
    .env(
        "FFMPEG_PATH",
        std::env::var("PLAYBACK_TEST_FFMPEG").unwrap(),
    )
    .env(
        "FFPROBE_PATH",
        std::env::var("PLAYBACK_TEST_FFPROBE").unwrap(),
    )
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .kill_on_drop(true)
    .spawn()
    .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let base = format!("http://{address}");
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
                .is_ok_and(|response| response.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("readiness deadline");
    let key: Value = client
        .post(format!("{base}/v1/keys"))
        .bearer_auth(bootstrap)
        .json(
            &json!({"label":"disabled torrent fixture","namespaces":["fixture"],
            "scopes":["capabilities","create","read","renew","release"],
            "quotas":{"inputs":1,"outputs":1,"viewers":1}}),
        )
        .send()
        .await
        .map_err(|_| "key request failed")
        .unwrap()
        .json()
        .await
        .map_err(|_| "key response invalid")
        .unwrap();
    let key = key["secret"].as_str().expect("issued integration key");
    let capabilities: Value = client
        .get(format!("{base}/v1/capabilities"))
        .bearer_auth(key)
        .send()
        .await
        .map_err(|_| "capabilities request failed")
        .unwrap()
        .json()
        .await
        .map_err(|_| "capabilities response invalid")
        .unwrap();
    assert_eq!(capabilities["torrent"], false);
    for (index, input) in [
        "magnet:?xt=urn:btih:1111111111111111111111111111111111111111".to_owned(),
        format!("http://{source_address}/clip.torrent"),
        format!("http://{source_address}/clip.rar"),
    ]
    .into_iter()
    .enumerate()
    {
        let response = client
            .post(format!("{base}/v1/sessions"))
            .bearer_auth(key)
            .header("idempotency-key", format!("disabled-{index}"))
            .json(
                &json!({"namespace":"fixture","input":{"url":input,"live":false},
                "output":{"protocol":"hls","video_codecs":["h264"],"audio_codecs":["aac"],
                    "max_width":320,"max_height":180},"position_seconds":0}),
            )
            .send()
            .await
            .map_err(|_| "session request failed")
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        let error: Value = response
            .json()
            .await
            .map_err(|_| "error response invalid")
            .unwrap();
        assert_eq!(error["error"]["code"], "unsupported_output");
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(250), source.accept())
            .await
            .is_err(),
        "disabled inputs must not contact the fixture source"
    );
    assert!(
        !root.path().join("state/torrent-cache").exists(),
        "disabled transport must not initialize a cache"
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
}
