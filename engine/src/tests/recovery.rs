use super::*;

#[tokio::test]
#[ignore = "requires real FFmpeg/ffprobe and openssl; isolated TLS fixtures"]
async fn real_https_rejects_untrusted_certificates_and_wrong_hostnames() {
    let root = tempfile::tempdir().unwrap();
    let certificate = root.path().join("certificate.pem");
    let key = root.path().join("key.pem");
    let generated = timeout(
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
    let fixture = timeout(
        Duration::from_secs(10),
        Command::new(std::env::var("PLAYBACK_TEST_FFMPEG").unwrap())
            .kill_on_drop(true)
            .args([
                "-v",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=160x90:rate=25",
                "-t",
                "1",
                "-an",
                "-c:v",
                "libx264",
                "-threads",
                "2",
                "-preset",
                "ultrafast",
                "-movflags",
                "+faststart",
                "-f",
                "mp4",
            ])
            .arg(root.path().join("good.mp4"))
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(fixture.status.success());
    let media = std::fs::read(root.path().join("good.mp4")).unwrap();
    let mut response = format!("HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", media.len()).into_bytes();
    response.extend_from_slice(&media);
    std::fs::write(root.path().join("good.http"), response).unwrap();
    let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let mut server = Command::new("openssl")
        .kill_on_drop(true)
        .args(["s_server", "-quiet", "-HTTP", "-accept"])
        .current_dir(root.path())
        .arg(address.to_string())
        .arg("-cert")
        .arg(&certificate)
        .arg("-key")
        .arg(&key)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    timeout(Duration::from_secs(3), async {
        while tokio::net::TcpStream::connect(address).await.is_err() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_address = http.local_addr().unwrap();
    let playlist = format!("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\nhttps://{address}/segment.ts\n#EXT-X-ENDLIST\n");
    let router = axum::Router::new().route(
        "/index.m3u8",
        axum::routing::get(move || {
            let playlist = playlist.clone();
            async move { playlist }
        }),
    );
    let http_server = tokio::spawn(async move {
        axum::serve(http, router).await.unwrap();
    });
    for variable in ["PLAYBACK_TEST_FFPROBE", "PLAYBACK_TEST_FFMPEG"] {
        let binary = std::env::var(variable).unwrap();
        for case in ["untrusted", "hostname", "nested", "valid"] {
            let mut command = Command::new(&binary);
            command.kill_on_drop(true).args(["-v", "error"]);
            input_args(&mut command, "");
            if case == "valid" {
                command.args(["-probesize", "4096", "-analyzeduration", "100000"]);
                if variable == "PLAYBACK_TEST_FFPROBE" {
                    command.args(["-read_intervals", "%+0.1"]);
                }
            }
            if matches!(case, "hostname" | "valid") {
                command.arg("-ca_file").arg(&certificate);
            }
            let url = if case == "valid" {
                format!("https://localhost:{}/good.http", address.port())
            } else if case == "nested" {
                format!("http://{http_address}/index.m3u8")
            } else {
                format!("https://{address}/stream.ts")
            };
            command.arg("-i").arg(url);
            if variable == "PLAYBACK_TEST_FFMPEG" {
                command.args(["-t", "0.1", "-f", "null", "-"]);
            }
            let mut child = command
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stderr = child.stderr.take().unwrap();
            let diagnostics = tokio::spawn(async move {
                let mut bytes = Vec::new();
                let _ = stderr.read_to_end(&mut bytes).await;
                bytes
            });
            let outcome = timeout(Duration::from_secs(8), child.wait()).await;
            if outcome.is_err() {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
            let bytes = diagnostics.await.unwrap();
            let diagnostic = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
            let safe = diagnostic
                .lines()
                .filter(|line| {
                    !line.contains("://") && !line.contains("/tmp/") && !line.contains("headers")
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                outcome.is_ok(),
                "TLS fixture timed out ({variable}, {case}): {safe}"
            );
            let status = outcome.unwrap().unwrap();
            if case == "valid" {
                assert!(
                    status.success(),
                    "a trusted, matching HTTPS source must play: {safe}"
                );
                continue;
            }
            assert!(!status.success());
            let rejected_certificate = diagnostic.contains("certificate")
                && (diagnostic.contains("verif")
                    || diagnostic.contains("not trusted")
                    || diagnostic.contains("does not match hostname"));
            assert!(
                rejected_certificate,
                "TLS must fail before media parsing ({variable}, {case}): {diagnostic}"
            );
            if case == "hostname" {
                assert!(
                    diagnostic.contains("does not match hostname"),
                    "trusted issuer must still fail hostname validation"
                );
            }
        }
    }
    http_server.abort();
    let _ = http_server.await;
    server.kill().await.unwrap();
    let _ = server.wait().await;
}

#[tokio::test]
#[ignore = "requires real FFmpeg/ffprobe; local proxy/forbidden destination fixtures"]
async fn real_egress_proxy_covers_root_redirects_nested_segments_and_keys() {
    use tokio::io::AsyncWriteExt;
    for variable in ["PLAYBACK_TEST_FFPROBE", "PLAYBACK_TEST_FFMPEG"] {
        let binary = std::env::var(variable).unwrap();
        for kind in ["root", "redirect", "segment", "key", "tcp", "tls"] {
            let forbidden = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let destination = forbidden.local_addr().unwrap();
            let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = proxy.local_addr().unwrap();
            let trace = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let received = trace.clone();
            let server = tokio::spawn(async move {
                loop {
                    let (mut socket, _) = proxy.accept().await.unwrap();
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") && request.len() < 16384 {
                        match socket.read_u8().await {
                            Ok(byte) => request.push(byte),
                            Err(_) => break,
                        }
                    }
                    let request = String::from_utf8_lossy(&request).into_owned();
                    let initial = request
                        .lines()
                        .next()
                        .is_some_and(|line| line.contains("/index.m3u8"));
                    received.lock().unwrap().push(request);
                    let body = if initial && kind == "redirect" {
                        format!("HTTP/1.1 302 Found\r\nLocation: http://{destination}/secret.ts\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    } else if initial && kind != "root" {
                        let scheme = if matches!(kind, "tcp" | "tls") {
                            kind
                        } else {
                            "http"
                        };
                        let key = if kind == "key" {
                            format!("#EXT-X-KEY:METHOD=AES-128,URI=\"http://{destination}/secret.key\",IV=0x00000000000000000000000000000001\n")
                        } else {
                            String::new()
                        };
                        let playlist = format!("#EXTM3U\n#EXT-X-TARGETDURATION:4\n{key}#EXTINF:4,\n{scheme}://{destination}/secret.ts\n#EXT-X-ENDLIST\n");
                        format!("HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{playlist}", playlist.len())
                    } else {
                        "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .into()
                    };
                    let _ = socket.write_all(body.as_bytes()).await;
                }
            });
            let mut command = Command::new(&binary);
            command
                .kill_on_drop(true)
                .args(["-v", "error", "-allowed_extensions", "ALL"])
                .env("no_proxy", "*")
                .env("NO_PROXY", "*")
                .env("http_proxy", "http://127.0.0.1:1");
            input_args(
                &mut command,
                &format!("x-playback-egress-proxy: http://{proxy_address}\r\n"),
            );
            if variable == "PLAYBACK_TEST_FFPROBE" {
                command.arg("-show_streams");
            }
            command
                .arg("-i")
                .arg(format!("http://{destination}/index.m3u8"));
            if variable == "PLAYBACK_TEST_FFMPEG" {
                command.args(["-t", "1", "-f", "null", "-"]);
            }
            let outcome = timeout(Duration::from_secs(8), command.output()).await;
            server.abort();
            let _ = server.await;
            assert!(
                timeout(Duration::from_millis(30), forbidden.accept())
                    .await
                    .is_err(),
                "{kind} must never contact a destination outside the proxy"
            );
            let output = outcome.expect("bounded failure").unwrap();
            assert!(!output.status.success(), "forbidden fixture must not play");
            let requests = trace.lock().unwrap();
            assert!(
                !requests.is_empty(),
                "root request must use the explicit proxy"
            );
            if matches!(kind, "redirect" | "segment" | "key") {
                assert!(
                    requests.len() >= 2,
                    "{kind} must reach proxy enforcement for child request"
                );
            }
            assert!(requests
                .iter()
                .all(|request| !request.contains("x-playback-egress-proxy")));
        }
    }
}

#[tokio::test]
#[ignore = "requires a real ffprobe binary"]
async fn real_ffprobe_rejects_nested_local_segments_and_aes_keys() {
    let ffprobe =
        PathBuf::from(std::env::var("PLAYBACK_TEST_FFPROBE").expect("PLAYBACK_TEST_FFPROBE"));
    for encrypted in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let contents = if encrypted {
            format!("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-KEY:METHOD=AES-128,URI=\"file:///viptv-protocol-test.ts\",IV=0x00000000000000000000000000000001\n#EXTINF:4,\n{base}/segment.ts\n#EXT-X-ENDLIST\n")
        } else {
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\nfile:///viptv-protocol-test.ts\n#EXT-X-ENDLIST\n"
                .to_owned()
        };
        let router = axum::Router::new()
            .route(
                "/index.m3u8",
                axum::routing::get(move || {
                    let contents = contents.clone();
                    async move { contents }
                }),
            )
            .route(
                "/segment.ts",
                axum::routing::get(|| async { vec![0u8; 188] }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let mut cmd = Command::new(&ffprobe);
        cmd.kill_on_drop(true)
            .args(["-v", "error", "-allowed_extensions", "ALL"]);
        input_args(&mut cmd, "");
        cmd.args(["-show_streams", "-i"])
            .arg(format!("{base}/index.m3u8"));
        let result = timeout(Duration::from_secs(10), cmd.output()).await;
        server.abort();
        let _ = server.await;
        let output = result.unwrap().unwrap();
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(
            diagnostic.contains("not on whitelist") && diagnostic.contains("file"),
            "local protocol should be blocked (AES={encrypted}): {diagnostic}"
        );
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HttpBodyFault {
    None,
    Once,
    Permanent,
    IgnoreRange,
    Forbidden,
}

struct BodyFaultServer {
    url: String,
    trace: Arc<std::sync::Mutex<Vec<(usize, usize, bool)>>>,
    task: tokio::task::JoinHandle<()>,
    cut: usize,
}

impl Drop for BodyFaultServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn body_fault_server(
    bytes: Arc<Vec<u8>>,
    moov: usize,
    mode: HttpBodyFault,
) -> BodyFaultServer {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/fixture.mp4", listener.local_addr().unwrap());
    let trace = Arc::new(std::sync::Mutex::new(Vec::new()));
    let state = trace.clone();
    let cut = moov + (bytes.len() - moov) / 2;
    let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((mut socket,_)) = accepted else { break; };
                    let bytes = bytes.clone();
                    let state = state.clone();
                    let failed = failed.clone();
                    connections.spawn(async move {
                        let mut request = Vec::new();
                        while !request.ends_with(b"\r\n\r\n") && request.len()<8192 {
                            let mut byte = [0];
                            match timeout(Duration::from_secs(3),socket.read(&mut byte)).await {
                                Ok(Ok(1)) => request.push(byte[0]),
                                _ => return,
                            }
                        }
                        let request = String::from_utf8_lossy(&request);
                        let range = request.lines().find_map(|line| {
                            let (name,value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("range").then_some(value.trim())
                        });
                        let start = range.and_then(|value| value.strip_prefix("bytes="))
                            .and_then(|value| value.split('-').next()).and_then(|value| value.parse::<usize>().ok()).unwrap_or(0);
                        if mode == HttpBodyFault::Forbidden {
                            state.lock().unwrap().push((start,0,false));
                            let _ = socket.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                            return;
                        }
                        if start >= bytes.len() {
                            let _ = socket.write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                            return;
                        }
                        let already_failed = failed.load(std::sync::atomic::Ordering::SeqCst);
                        let ignore = mode == HttpBodyFault::IgnoreRange && already_failed && start >= cut;
                        let fault = start >= moov && mode != HttpBodyFault::None && !ignore &&
                            (mode == HttpBodyFault::Permanent || !failed.swap(true,std::sync::atomic::Ordering::SeqCst));
                        let send_start = if ignore {0} else {start};
                        let end = if fault {cut.max(start)} else {bytes.len()};
                        state.lock().unwrap().push((start,end-send_start,fault));
                        let response = if range.is_some() && !ignore {
                            format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{}\r\n",bytes.len()-1,bytes.len())
                        } else {"HTTP/1.1 200 OK\r\n".to_owned()};
                        let header = format!("{response}Content-Type: video/mp4\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len()-send_start);
                        if socket.write_all(header.as_bytes()).await.is_ok() {
                            let _ = socket.write_all(&bytes[send_start..end]).await;
                        }
                        let _ = socket.shutdown().await;
                    });
                }
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
            }
        }
    });
    BodyFaultServer {
        url,
        trace,
        task,
        cut,
    }
}

async fn tail_moov_fixture(root: &std::path::Path, ffmpeg: &str) -> (Arc<Vec<u8>>, usize) {
    let path = root.join("tail-moov.mp4");
    let output = timeout(
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
                "testsrc2=size=640x360:rate=25",
                "-t",
                "6",
                "-an",
                "-c:v",
                "libx264",
                "-threads",
                "2",
                "-preset",
                "ultrafast",
                "-g",
                "25",
                "-bf",
                "0",
            ])
            .arg(&path)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(output.status.success(), "fixture generation failed");
    let bytes = tokio::fs::read(path).await.unwrap();
    let mut position = 0;
    let mut moov = None;
    while position + 8 <= bytes.len() {
        let size = u32::from_be_bytes(bytes[position..position + 4].try_into().unwrap()) as usize;
        if &bytes[position + 4..position + 8] == b"moov" {
            moov = Some(position);
            break;
        }
        assert!(size >= 8);
        position += size;
    }
    let moov = moov.expect("tail moov");
    assert!(moov > 32768 && moov > bytes.len() / 2);
    (Arc::new(bytes), moov)
}

fn recovery_fixture_args(command: &mut Command, reconnect: bool) {
    if reconnect {
        input_args(command, "");
    } else {
        // Control group: the production flags before HTTP recovery was added.
        command.args([
            "-protocol_whitelist",
            "http,https,httpproxy,tcp,tls,crypto",
            "-rw_timeout",
            "10000000",
        ]);
    }
}

#[tokio::test]
#[ignore = "requires configured real FFmpeg/ffprobe; local raw HTTP fault fixture only"]
async fn real_http_premature_body_recovery() {
    let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").unwrap();
    let ffprobe = std::env::var("PLAYBACK_TEST_FFPROBE").unwrap();
    let root = tempfile::tempdir().unwrap();
    let (bytes, moov) = tail_moov_fixture(root.path(), &ffmpeg).await;
    for reconnect in [false, true] {
        let server = body_fault_server(bytes.clone(), moov, HttpBodyFault::Once).await;
        let mut command = Command::new(&ffprobe);
        command.kill_on_drop(true).args(["-v", "warning"]);
        recovery_fixture_args(&mut command, reconnect);
        command
            .args(["-show_streams", "-of", "json", "-i"])
            .arg(&server.url);
        let output = timeout(Duration::from_secs(12), command.output())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            output.status.success(),
            reconnect,
            "one-shot premature-body outcome"
        );
        let trace = server.trace.lock().unwrap();
        assert!(
            trace.iter().any(|entry| entry.2),
            "fault must actually trigger"
        );
        if reconnect {
            assert!(
                trace.iter().any(|entry| entry.0 == server.cut),
                "resume must request exact missing byte"
            );
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("Will reconnect"),
                "actual reconnect required"
            );
            let probe: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(probe["streams"][0]["codec_name"], "h264");
        }
        eprintln!(
            "HTTP_RECOVERY probe reconnect={reconnect} success={} requests={}",
            output.status.success(),
            trace.len()
        );
    }
    for (mode, seconds) in [(HttpBodyFault::Once, "3"), (HttpBodyFault::None, "10")] {
        let server = body_fault_server(bytes.clone(), moov, mode).await;
        let mut command = Command::new(&ffmpeg);
        command
            .kill_on_drop(true)
            .args(["-v", "warning", "-nostdin", "-nostats"]);
        recovery_fixture_args(&mut command, true);
        command.args(["-re", "-i"]).arg(&server.url).args([
            "-map",
            "0:v:0",
            "-t",
            seconds,
            "-c",
            "copy",
            "-progress",
            "pipe:1",
            "-f",
            "null",
            "-",
        ]);
        let started = Instant::now();
        let output = timeout(Duration::from_secs(12), command.output())
            .await
            .unwrap()
            .unwrap();
        assert!(output.status.success(), "paced copy must complete");
        let progress = String::from_utf8(output.stdout).unwrap();
        let times: Vec<i64> = progress
            .lines()
            .filter_map(|line| line.strip_prefix("out_time_us="))
            .filter_map(|value| value.parse().ok())
            .collect();
        assert!(
            times.windows(2).all(|pair| pair[0] <= pair[1]),
            "timestamps must not rewind"
        );
        let last = *times.last().unwrap();
        if mode == HttpBodyFault::Once {
            assert!(last >= 2_900_000);
            assert!(String::from_utf8_lossy(&output.stderr).contains("Will reconnect"));
            assert!(server
                .trace
                .lock()
                .unwrap()
                .iter()
                .any(|entry| entry.0 == server.cut));
        } else {
            assert!(
                (5_800_000..=6_100_000).contains(&last),
                "natural EOF must end at fixture duration"
            );
            assert!(started.elapsed() < Duration::from_secs(9));
            assert!(!String::from_utf8_lossy(&output.stderr).contains("Will reconnect"));
        }
        eprintln!(
            "HTTP_RECOVERY paced fault={} final_us={last} elapsed={:.3}",
            mode == HttpBodyFault::Once,
            started.elapsed().as_secs_f64()
        );
    }
    for mode in [
        HttpBodyFault::Permanent,
        HttpBodyFault::IgnoreRange,
        HttpBodyFault::Forbidden,
    ] {
        let server = body_fault_server(bytes.clone(), moov, mode).await;
        let mut command = Command::new(&ffprobe);
        command.kill_on_drop(true).args(["-v", "warning"]);
        recovery_fixture_args(&mut command, true);
        command
            .args(["-show_streams", "-of", "json", "-i"])
            .arg(&server.url);
        let started = Instant::now();
        let output = timeout(Duration::from_secs(12), command.output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            !output.status.success(),
            "permanent/ignored range/auth must fail closed"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        if mode == HttpBodyFault::Forbidden {
            assert_eq!(server.trace.lock().unwrap().len(), 1, "no HTTP auth retry");
        }
        eprintln!(
            "HTTP_RECOVERY failure case={} bounded_elapsed={:.3}",
            if mode == HttpBodyFault::Permanent {
                "permanent"
            } else if mode == HttpBodyFault::IgnoreRange {
                "ignored_range"
            } else {
                "forbidden"
            },
            started.elapsed().as_secs_f64()
        );
    }
}
