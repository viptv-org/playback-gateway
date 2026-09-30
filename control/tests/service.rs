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
