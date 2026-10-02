//! Isolated real TLS media. Trust/loopback exceptions exist only in this test.
use super::tests::{request, BOOTSTRAP};
use super::*;
use crate::credentials::{KeySpec, Quotas};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::{Child, Command};

struct TlsSource {
    server: Child,
    publisher: tokio::task::JoinHandle<()>,
    http: tokio::task::JoinHandle<()>,
    root: String,
    children: String,
    wrong_host: String,
    certificate: PathBuf,
}
impl Drop for TlsSource {
    fn drop(&mut self) {
        let _ = self.server.start_kill();
        self.publisher.abort();
        self.http.abort();
    }
}

fn publish(dir: &Path, base: &str, parts: &[Vec<u8>], sequence: u64) -> String {
    let mut playlist = format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:{sequence}\n"
    );
    for index in sequence..sequence + 3 {
        let file = dir.join(format!("part-{index}.ts"));
        if !file.exists() {
            let bytes = &parts[index as usize % parts.len()];
            let mut response = format!("HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).into_bytes();
            response.extend_from_slice(bytes);
            std::fs::write(file, response).unwrap();
        }
        playlist.push_str(&format!(
            "#EXT-X-DISCONTINUITY\n#EXTINF:2,\n{base}/part-{index}.ts\n"
        ));
    }
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{playlist}",
        playlist.len()
    );
    std::fs::write(dir.join("playlist.tmp"), response).unwrap();
    std::fs::rename(dir.join("playlist.tmp"), dir.join("playlist.http")).unwrap();
    playlist
}

async fn tls_source(root: &Path, ffmpeg: &Path) -> TlsSource {
    let dir = root.join("tls");
    std::fs::create_dir(&dir).unwrap();
    let certificate = root.join("certificate.pem");
    let key = root.join("key.pem");
    let generated = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new("openssl")
            .kill_on_drop(true)
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=localhost",
                "-addext",
                "subjectAltName=DNS:localhost",
                "-keyout",
            ])
            .arg(&key)
            .arg("-out")
            .arg(&certificate)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(generated.status.success());
    let source_playlist = root.join("source.m3u8");
    let generated = tokio::time::timeout(
        Duration::from_secs(20),
        Command::new(ffmpeg)
            .kill_on_drop(true)
            .args([
                "-v",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x180:rate=25",
                "-t",
                "8",
                "-an",
                "-c:v",
                "libx264",
                "-threads",
                "2",
                "-preset",
                "ultrafast",
                "-g",
                "50",
                "-f",
                "hls",
                "-hls_time",
                "2",
                "-hls_list_size",
                "0",
            ])
            .arg(&source_playlist)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(generated.status.success());
    let parts: Vec<_> = std::fs::read_to_string(source_playlist)
        .unwrap()
        .lines()
        .filter(|line| line.ends_with(".ts"))
        .map(|line| std::fs::read(root.join(line)).unwrap())
        .collect();
    let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let base = format!("https://localhost:{}", address.port());
    let manifest = Arc::new(Mutex::new(publish(&dir, &base, &parts, 0)));
    // HLS holds root/child connections concurrently. The inherited one-peer
    // openssl fixture server is insufficient for this multi-resource fixture.
    let script = r#"import http.server, ssl, sys, pathlib, re
root = pathlib.Path(sys.argv[2])
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_GET(self):
        name = self.path.removeprefix('/')
        if name != 'playlist.http' and re.fullmatch(r'part-[0-9]+\.ts', name) is None:
            self.send_error(404); return
        self.connection.sendall((root / name).read_bytes())
        self.close_connection = True
server = http.server.ThreadingHTTPServer(('127.0.0.1', int(sys.argv[1])), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(sys.argv[3], sys.argv[4])
server.socket = context.wrap_socket(server.socket, server_side=True)
server.serve_forever()
"#;
    let server = Command::new("python3")
        .kill_on_drop(true)
        .args(["-c", script])
        .arg(address.port().to_string())
        .arg(&dir)
        .arg(&certificate)
        .arg(key)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while tokio::net::TcpStream::connect(address).await.is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let current = manifest.clone();
    let origin = base.clone();
    let publisher = tokio::spawn(async move {
        let mut sequence = 0;
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            sequence += 1;
            *current.lock().unwrap() = publish(&dir, &origin, &parts, sequence);
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_address = listener.local_addr().unwrap();
    let app = axum::Router::new().route(
        "/index.m3u8",
        axum::routing::get(move || {
            let manifest = manifest.clone();
            async move { manifest.lock().unwrap().clone() }
        }),
    );
    let http = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    TlsSource {
        server,
        publisher,
        http,
        root: format!("{base}/playlist.http"),
        children: format!("http://{http_address}/index.m3u8"),
        wrong_host: format!("https://{address}/playlist.http"),
        certificate,
    }
}

fn quoted(value: &Path) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', "'\"'\"'"))
}

fn trusted_binary(
    root: &Path,
    name: &str,
    binary: &Path,
    certificate: &Path,
) -> (PathBuf, PathBuf) {
    let wrapper = root.join(name);
    let trace = root.join(format!("{name}.count"));
    let anchors = root.join(format!("{name}-anchors"));
    std::fs::create_dir(&anchors).unwrap();
    std::fs::copy(certificate, anchors.join("fixture.crt")).unwrap();
    // Per-process mount namespace only: never modify the host trust store.
    // HLS children use GnuTLS system trust rather than inheriting root ca_file.
    let body = format!(
        "#!/bin/sh\nremote=0\nfor arg do case \"$arg\" in http://*|https://*) remote=1 ;; esac; done\nif [ \"$remote\" = 1 ]; then\n printf x >> {}\n exec bwrap --die-with-parent --ro-bind / / --bind {} {} --ro-bind {} /etc/ca-certificates/trust-source/anchors --ro-bind {} /etc/ca-certificates/extracted/tls-ca-bundle.pem -- {} \"$@\"\nfi\nexec {} \"$@\"\n",
        quoted(&trace),
        quoted(root),
        quoted(root),
        quoted(&anchors),
        quoted(certificate),
        quoted(binary),
        quoted(binary)
    );
    std::fs::write(&wrapper, body).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    (wrapper, trace)
}

async fn workers(root: &Path, ffmpeg: &Path, ffprobe: &Path) -> (Workers, Principal) {
    let engine = PlaybackManager::new(playback_engine::Config {
        ffmpeg: ffmpeg.into(),
        ffprobe: ffprobe.into(),
        root: root.into(),
        max_sessions: 2,
        ttl: Duration::from_secs(120),
    });
    let credentials = Arc::new(
        Credentials::from_connection(rusqlite::Connection::open_in_memory().unwrap(), BOOTSTRAP)
            .unwrap(),
    );
    let key = credentials
        .issue(
            BOOTSTRAP,
            KeySpec {
                label: "TLS fixture".into(),
                namespaces: vec!["fixture".into()],
                scopes: vec![Scope::Create, Scope::Read, Scope::Release],
                quotas: Quotas {
                    inputs: 1,
                    outputs: 2,
                    viewers: 2,
                },
                expires_at: None,
            },
            0,
        )
        .unwrap();
    let principal = credentials.authenticate(&key.secret, 0).unwrap();
    (
        Workers::new(engine, credentials, NetworkPolicy::loopback_fixture())
            .await
            .unwrap(),
        principal,
    )
}

#[tokio::test]
#[ignore = "requires real FFmpeg/ffprobe, openssl, python3 and Linux bwrap trust namespace; isolated TLS fixtures"]
async fn real_shared_https_and_http_https_children_verify_trust_and_hostnames() {
    let ffmpeg = PathBuf::from(std::env::var("PLAYBACK_TEST_FFMPEG").unwrap());
    let ffprobe = PathBuf::from(std::env::var("PLAYBACK_TEST_FFPROBE").unwrap());
    let root = tempfile::tempdir().unwrap();
    let host_trust = std::fs::read("/etc/ca-certificates/extracted/tls-ca-bundle.pem").unwrap();
    let source = tls_source(root.path(), &ffmpeg).await;
    let (trusted_ffmpeg, trace) =
        trusted_binary(root.path(), "trusted-ffmpeg", &ffmpeg, &source.certificate);
    let (trusted_ffprobe, _) = trusted_binary(
        root.path(),
        "trusted-ffprobe",
        &ffprobe,
        &source.certificate,
    );
    for (index, (url, trusted)) in [
        (&source.root, false),
        (&source.wrong_host, true),
        (&source.children, false),
    ]
    .into_iter()
    .enumerate()
    {
        let (workers, principal) = workers(
            &root.path().join(format!("denied-{index}")),
            if trusted { &trusted_ffmpeg } else { &ffmpeg },
            if trusted { &trusted_ffprobe } else { &ffprobe },
        )
        .await;
        let mut input = request(1920);
        input.input.url = url.clone();
        assert_eq!(
            workers
                .create(&principal, &input, "denied", 0)
                .await
                .unwrap_err(),
            "source_preparation_failed"
        );
        assert_eq!(workers.sessions.lock().unwrap().job_count(), 0);
        workers.shutdown().await;
    }
    for (index, url) in [&source.root, &source.children].into_iter().enumerate() {
        let before = std::fs::read(&trace).map(|bytes| bytes.len()).unwrap_or(0);
        let media = root.path().join(format!("media-{index}"));
        let (workers, principal) = workers(&media, &trusted_ffmpeg, &trusted_ffprobe).await;
        let mut input = request(1920);
        input.input.url = url.clone();
        let first = workers
            .create(&principal, &input, "first", 0)
            .await
            .unwrap();
        input.output.max_width = 160;
        input.output.max_height = 90;
        let second = workers
            .create(&principal, &input, "second", 5)
            .await
            .unwrap();
        assert_ne!(first.job_id, second.job_id);
        assert_eq!(
            std::fs::read(&trace).unwrap().len() - before,
            1,
            "one real FFmpeg ingest may read the remote source"
        );
        let delivery = workers.delivery(&principal, &second, 5).unwrap().unwrap();
        let token = delivery.url.split('/').nth(3).unwrap();
        let response = workers
            .media(
                &second.id,
                token,
                "index.m3u8",
                5,
                axum::http::Method::GET,
                axum::http::HeaderMap::new(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 128 * 1024)
            .await
            .unwrap();
        let segment = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .find(|line| line.ends_with(".ts"))
            .unwrap();
        let response = workers
            .media(
                &second.id,
                token,
                segment,
                5,
                axum::http::Method::GET,
                axum::http::HeaderMap::new(),
            )
            .await
            .unwrap();
        let sample = root.path().join(format!("decoded-{index}.ts"));
        std::fs::write(
            &sample,
            axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let decoded = tokio::time::timeout(
            Duration::from_secs(10),
            Command::new(&ffmpeg)
                .kill_on_drop(true)
                .args(["-v", "error", "-i"])
                .arg(sample)
                .args(["-frames:v", "1", "-f", "null", "-"])
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            decoded.status.success(),
            "trusted shared replay output must decode"
        );
        for viewer in [first, second] {
            workers
                .sessions
                .lock()
                .unwrap()
                .release(&principal, &viewer.id, 10)
                .unwrap();
        }
        workers.reap(26).await.unwrap();
        assert_eq!(workers.sessions.lock().unwrap().job_count(), 0);
        workers.shutdown().await;
    }
    assert_eq!(
        std::fs::read("/etc/ca-certificates/extracted/tls-ca-bundle.pem").unwrap(),
        host_trust,
        "fixture namespaces must leave host trust untouched"
    );
}
