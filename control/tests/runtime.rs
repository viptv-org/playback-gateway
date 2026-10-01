#![cfg(target_os = "linux")]
//! Real process/HTTP lifecycle in an isolated disposable Docker network.
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
};
const BOOTSTRAP: &str = "runtime-qualification-bootstrap-fixture-only";
/// Real-time paced progressive source length; long enough that no fixture
/// observes the source loop.
const PROGRESSIVE_SECONDS: f64 = 240.0;
fn progressive() -> bool {
    std::env::var("PLAYBACK_TEST_PROGRESSIVE").as_deref() == Ok("1")
}
fn state_root(root: &Path) -> PathBuf {
    std::env::var_os("PLAYBACK_TEST_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("state"))
}

fn isolated_address() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    assert_eq!(
        std::env::var("PLAYBACK_TEST_ISOLATED_NETWORK").as_deref(),
        Ok("container")
    );
    assert!(
        Path::new("/.dockerenv").exists(),
        "never configure host networking"
    );
    let interfaces: Vec<_> = std::fs::read_dir("/sys/class/net")
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        interfaces,
        vec![std::ffi::OsString::from("lo")],
        "run with Docker network none"
    );
    // Same guarded public-classified loopback seam as the proven backend fixture.
    // These ioctls can affect only this disposable, network-none namespace.
    unsafe {
        let raw = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        assert!(raw >= 0);
        let socket = OwnedFd::from_raw_fd(raw);
        let mut request: libc::ifreq = std::mem::zeroed();
        for (slot, byte) in request.ifr_name.iter_mut().zip(b"lo:fixture\0") {
            *slot = *byte as libc::c_char;
        }
        for (operation, octets) in [
            (libc::SIOCSIFADDR, [11, 255, 255, 1]),
            (libc::SIOCSIFNETMASK, [255, 255, 255, 255]),
        ] {
            let address = libc::sockaddr_in {
                sin_family: libc::AF_INET as u16,
                sin_port: 0,
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(octets),
                },
                sin_zero: [0; 8],
            };
            request.ifr_ifru.ifru_addr =
                std::ptr::read((&address as *const libc::sockaddr_in).cast::<libc::sockaddr>());
            assert_eq!(
                libc::ioctl(socket.as_raw_fd(), operation, &request),
                0,
                "fixture requires NET_ADMIN"
            );
        }
    }
}
struct Source {
    url: String,
    active: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Source {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn source(bytes: Vec<u8>) -> Source {
    let listener = TcpListener::bind("11.255.255.1:0").await.unwrap();
    let url = format!("http://{}/live.ts", listener.local_addr().unwrap());
    let active = Arc::new(AtomicUsize::new(0));
    let observed = active.clone();
    let requests = Arc::new(AtomicUsize::new(0));
    let opened = requests.clone();
    let high = std::env::var("PLAYBACK_TEST_CBR_4K").as_deref() == Ok("1");
    let progressive = progressive();
    let captions = std::env::var("PLAYBACK_TEST_SUBTITLES").as_deref() == Ok("1") || progressive;
    let overflow = std::env::var("PLAYBACK_TEST_CAPTION_OVERFLOW").as_deref() == Ok("1");
    let seconds = if progressive {
        PROGRESSIVE_SECONDS
    } else {
        120.0
    };
    let interval = Duration::from_secs_f64(seconds * (188 * 100) as f64 / bytes.len() as f64);
    let bytes = Arc::new(bytes);
    let task = tokio::spawn(async move {
        let mut peers = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                incoming=listener.accept()=>{let Ok((mut socket,_))=incoming else {break};let observed=observed.clone();let opened=opened.clone();let bytes=bytes.clone();peers.spawn(async move {
                    struct Count(Arc<AtomicUsize>);impl Drop for Count{fn drop(&mut self){self.0.fetch_sub(1,Ordering::SeqCst);}}
                    observed.fetch_add(1,Ordering::SeqCst);let _count=Count(observed);
                    opened.fetch_add(1,Ordering::SeqCst);
                    let mut header=Vec::new();while !header.ends_with(b"\r\n\r\n") && header.len()<16384 {match socket.read_u8().await{Ok(byte)=>header.push(byte),Err(_)=>return}}
                    if socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nConnection: close\r\n\r\n").await.is_err(){return}
                    loop {for chunk in bytes.chunks(if high {188*4096}else{188*100}){if socket.write_all(chunk).await.is_err(){return}tokio::time::sleep(if overflow {Duration::from_millis(1)}else if captions {interval}else{Duration::from_millis(if high {150}else{30})}).await;}if overflow {let _=socket.read_u8().await;return;}}
                });}, _=peers.join_next(),if !peers.is_empty()=>{}
            }
        }
    });
    Source {
        url,
        active,
        requests,
        task,
    }
}
async fn connections(source: &Source, expected: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while source.active.load(Ordering::SeqCst) != expected {
            tokio::time::sleep(Duration::from_millis(20)).await
        }
    })
    .await
    .expect("source connections must settle");
}
fn children(parent: u32) -> BTreeSet<u32> {
    let mut pids = BTreeSet::<u32>::new();
    if let Ok(tasks) = std::fs::read_dir(format!("/proc/{parent}/task")) {
        for task in tasks.flatten() {
            if let Ok(text) = std::fs::read_to_string(task.path().join("children")) {
                pids.extend(
                    text.split_whitespace()
                        .filter_map(|pid| pid.parse::<u32>().ok()),
                );
            }
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
            tokio::time::sleep(Duration::from_millis(20)).await
        }
    })
    .await
    .expect("all owned media children must be reaped");
}

async fn demux(bytes: &[u8], format: &str) -> Value {
    let mut child = Command::new(std::env::var("PLAYBACK_TEST_FFPROBE").unwrap())
        .args([
            "-v",
            "error",
            "-f",
            format,
            "-show_streams",
            "-show_packets",
            "-of",
            "json",
            "-i",
            "pipe:0",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let feeding = async {
        input.write_all(bytes).await.unwrap();
        drop(input);
    };
    let output = tokio::time::timeout(Duration::from_secs(8), async {
        let (_, output) = tokio::join!(feeding, child.wait_with_output());
        output.unwrap()
    })
    .await
    .unwrap();
    assert!(output.status.success(), "fetched media must demux");
    serde_json::from_slice(&output.stdout).unwrap()
}

async fn decode(bytes: &[u8], seek: &str) {
    let mut child = Command::new(std::env::var("PLAYBACK_TEST_FFMPEG").unwrap())
        .args([
            "-v",
            "error",
            "-nostdin",
            "-f",
            "mpegts",
            "-i",
            "pipe:0",
            "-ss",
            seek,
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-threads",
            "2",
            "-f",
            "framemd5",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let feeding = async {
        input.write_all(bytes).await.unwrap();
        drop(input);
    };
    let (_, output) = tokio::time::timeout(Duration::from_secs(8), async {
        tokio::join!(feeding, child.wait_with_output())
    })
    .await
    .unwrap();
    assert!(
        output.as_ref().unwrap().status.success(),
        "retained media must decode after seeking"
    );
    assert!(
        String::from_utf8(output.unwrap().stdout)
            .unwrap()
            .lines()
            .any(|line| !line.starts_with('#') && line.contains(',')),
        "seek must actually decode a frame"
    );
}

async fn decoded_audio_frequency(bytes: &[u8]) -> f64 {
    let mut child = Command::new(std::env::var("PLAYBACK_TEST_FFMPEG").unwrap())
        .args([
            "-v", "error", "-nostdin", "-f", "mpegts", "-i", "pipe:0", "-map", "0:a:0", "-t",
            "0.3", "-ac", "1", "-ar", "48000", "-f", "f32le", "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let feeding = async {
        input.write_all(bytes).await.unwrap();
        drop(input);
    };
    let (_, output) = tokio::time::timeout(Duration::from_secs(8), async {
        tokio::join!(feeding, child.wait_with_output())
    })
    .await
    .unwrap();
    let output = output.unwrap();
    assert!(output.status.success());
    let samples = output
        .stdout
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| f32::from_le_bytes(*bytes))
        .collect::<Vec<_>>();
    assert!(samples.len() > 4800, "selected audio must actually decode");
    samples
        .windows(2)
        .filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0)
        .count() as f64
        * 48_000.0
        / samples.len() as f64
}

async fn tls_proxy(upstream: std::net::SocketAddr) -> (String, Child) {
    let reservation = TcpListener::bind("11.255.255.1:0").await.unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let script = r#"import socket, socketserver, ssl, sys, select
context=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain('/fixtures/subtitle-server.pem','/fixtures/subtitle-key.pem')
class Handler(socketserver.BaseRequestHandler):
 def handle(self):
  try:
   with context.wrap_socket(self.request,server_side=True) as client, socket.create_connection((sys.argv[2],int(sys.argv[3])),timeout=5) as upstream:
    upstream.settimeout(None)
    while True:
     ready,_,_=select.select([client,upstream],[],[],1)
     if client.pending() and client not in ready: ready.append(client)
     for peer in ready:
      data=peer.recv(65536)
      if not data: return
      (upstream if peer is client else client).sendall(data)
  except (OSError,ssl.SSLError): pass
class Server(socketserver.ThreadingTCPServer):
 daemon_threads=True
Server(('11.255.255.1',int(sys.argv[1])),Handler).serve_forever()
"#;
    let child = Command::new("python3")
        .args(["-c", script])
        .arg(address.port().to_string())
        .arg(upstream.ip().to_string())
        .arg(upstream.port().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while TcpStream::connect(address).await.is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (format!("https://{address}"), child)
}
struct Fixture {
    root: tempfile::TempDir,
    child: Child,
    base: String,
    address: std::net::SocketAddr,
    source: Source,
    client: reqwest::Client,
    key: String,
    _tls: Vec<Child>,
}
impl Fixture {
    async fn new() -> Self {
        isolated_address();
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input.ts");
        let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").unwrap();
        let high = std::env::var("PLAYBACK_TEST_CBR_4K").as_deref() == Ok("1");
        let matroska = std::env::var("PLAYBACK_TEST_MATROSKA_AV").as_deref() == Ok("1");
        let captions = std::env::var("PLAYBACK_TEST_SUBTITLES").as_deref() == Ok("1");
        assert!(!(high && matroska));
        let english = root.path().join("english.srt");
        let spanish = root.path().join("spanish.srt");
        if captions {
            std::fs::write(&english, "1\n00:00:01,000 --> 00:01:15,000\nLONG_ENGLISH\n\n2\n00:00:02,000 --> 00:01:05,000\nOVERLAPPING_ENGLISH\n\n3\n00:00:04,000 --> 00:00:06,000\nEXPIRED_ENGLISH\n\n4\n00:00:40,000 --> 00:00:48,000\nLATER_ENGLISH\n").unwrap();
            std::fs::write(
                &spanish,
                "1\n00:00:01,000 --> 00:01:30,000\nWRONG_SPANISH\n",
            )
            .unwrap();
            if std::env::var("PLAYBACK_TEST_CAPTION_OVERFLOW").as_deref() == Ok("1") {
                let mut cues = String::new();
                for index in 0..70 {
                    cues.push_str(&format!(
                        "{}\n00:00:01,{:03} --> 00:01:40,000\n{}-{index}\n\n",
                        index + 1,
                        index * 10,
                        "X".repeat(20 * 1024)
                    ));
                }
                std::fs::write(&english, cues).unwrap();
            }
        }
        let mut generation = Command::new(&ffmpeg);
        generation.kill_on_drop(true);
        generation.args([
            "-v",
            "error",
            "-nostdin",
            "-f",
            "lavfi",
            "-i",
            if captions {
                "testsrc2=size=128x72:rate=10"
            } else if high {
                "color=c=black:size=3840x2160:rate=5"
            } else if matroska {
                "testsrc2=size=128x72:rate=10"
            } else {
                "testsrc2=size=320x180:rate=25"
            },
        ]);
        if captions {
            generation
                .args([
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=880:sample_rate=48000",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:sample_rate=48000",
                    "-i",
                ])
                .arg(&spanish)
                .arg("-i")
                .arg(&english)
                .args([
                    "-map",
                    "1:a",
                    "-map",
                    "3:s",
                    "-map",
                    "0:v",
                    "-map",
                    "2:a",
                    "-map",
                    "4:s",
                    "-c:a",
                    "aac",
                    "-c:s",
                    "srt",
                    "-metadata:s:s:0",
                    "language=spa",
                    "-metadata:s:s:1",
                    "language=eng",
                ]);
        } else if progressive() {
            // One real audio track: AAC in MPEG-TS, FLAC in Matroska.
            generation.args([
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-c:a",
                if matroska { "flac" } else { "aac" },
            ]);
        } else {
            generation.arg("-an");
        }
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            generation
                .args([
                    "-t",
                    if progressive() {
                        "240"
                    } else if captions {
                        "120"
                    } else if high {
                        "24"
                    } else {
                        "8"
                    },
                    "-c:v",
                    if matroska { "ffv1" } else { "libx264" },
                    "-threads",
                    "2",
                    "-g",
                    if high { "5" } else { "25" },
                    "-f",
                    if matroska || captions {
                        "matroska"
                    } else {
                        "mpegts"
                    },
                ])
                .args(if matroska {
                    &[][..]
                } else {
                    &["-preset", "ultrafast"][..]
                })
                .args(if high {
                    &[
                        "-b:v",
                        "40M",
                        "-minrate",
                        "40M",
                        "-maxrate",
                        "40M",
                        "-bufsize",
                        "40M",
                        "-x264-params",
                        "nal-hrd=cbr:force-cfr=1",
                        "-level:v",
                        "5.1",
                        "-bf",
                        "0",
                    ][..]
                } else {
                    &[]
                })
                .arg(&input)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(output.status.success());
        if high {
            let facts = Command::new(std::env::var("PLAYBACK_TEST_FFPROBE").unwrap())
                .args(["-v", "error", "-threads", "2", "-show_streams", "-show_packets", "-show_entries", "packet=size:stream=width,height,profile,pix_fmt,level,avg_frame_rate,r_frame_rate,duration", "-of", "json", "-i"])
                .arg(&input)
                .output()
                .await
                .unwrap();
            let facts: Value = serde_json::from_slice(&facts.stdout).unwrap();
            let video = &facts["streams"][0];
            let packets = facts["packets"].as_array().unwrap();
            let packet_bytes: u64 = packets
                .iter()
                .map(|packet| packet["size"].as_str().unwrap().parse::<u64>().unwrap())
                .sum();
            assert_eq!(packets.len(), 120);
            assert!(packet_bytes > 64 * 1024 * 1024);
            println!("CBR encoded frames={} duration={} H264_packet_bytes={packet_bytes} TS_file_bytes={}", packets.len(), video["duration"], std::fs::metadata(&input).unwrap().len());
            println!(
                "CBR source width={} height={} profile={} pix_fmt={} level={} avg={} rate={}",
                video["width"],
                video["height"],
                video["profile"],
                video["pix_fmt"],
                video["level"],
                video["avg_frame_rate"],
                video["r_frame_rate"]
            );
        }
        let mut source = source(std::fs::read(input).unwrap()).await;
        let mut tls = Vec::new();
        let use_tls = std::env::var("PLAYBACK_TEST_SUBTITLE_TLS").as_deref() == Ok("1");
        if use_tls {
            let address = source
                .url
                .strip_prefix("http://")
                .unwrap()
                .split('/')
                .next()
                .unwrap()
                .parse()
                .unwrap();
            let (base, proxy) = tls_proxy(address).await;
            source.url = format!("{base}/{}", if captions { "live.mkv" } else { "live.ts" });
            tls.push(proxy);
        }
        let port = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = port.local_addr().unwrap();
        drop(port);
        let child = Self::spawn(root.path(), address);
        let base = if use_tls {
            let (base, proxy) = tls_proxy(address).await;
            tls.push(proxy);
            base
        } else {
            format!("http://{address}")
        };
        let mut client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(35));
        if use_tls {
            client = client.add_root_certificate(
                reqwest::Certificate::from_pem(
                    &std::fs::read("/fixtures/subtitle-ca.pem").unwrap(),
                )
                .unwrap(),
            );
        }
        let client = client.build().unwrap();
        let mut fixture = Self {
            root,
            child,
            base,
            address,
            source,
            client,
            key: String::new(),
            _tls: tls,
        };
        fixture.ready().await;
        let response=fixture.client.post(format!("{}/v1/keys",fixture.base)).bearer_auth(BOOTSTRAP).json(&json!({"label":"runtime fixture","namespaces":["fixture"],"scopes":["create","read","renew","release","capabilities"],"quotas":if progressive() {json!({"inputs":1,"outputs":4,"viewers":5})} else {json!({"inputs":1,"outputs":2,"viewers":3})}})).send().await.map_err(|_|"key request failed").unwrap();
        assert!(response.status().is_success());
        let value: Value = response
            .json()
            .await
            .map_err(|_| "key response failed")
            .unwrap();
        fixture.key = value["secret"].as_str().unwrap().into();
        fixture
    }
    fn spawn(root: &Path, address: std::net::SocketAddr) -> Child {
        let mut command = Command::new(
            std::env::var("PLAYBACK_TEST_SERVICE_BINARY")
                .unwrap_or_else(|_| env!("CARGO_BIN_EXE_playback-gateway").into()),
        );
        command
            .env("API_KEY", BOOTSTRAP)
            .env("DATA_DIR", state_root(root))
            .env("BIND_ADDRESS", address.to_string())
            .env(
                "FFMPEG_PATH",
                std::env::var("PLAYBACK_TEST_FFMPEG").unwrap(),
            )
            .env(
                "FFPROBE_PATH",
                std::env::var("PLAYBACK_TEST_FFPROBE").unwrap(),
            )
            .env("MAX_OUTPUTS", if progressive() { "4" } else { "2" })
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if std::env::var_os("PLAYBACK_TEST_DATA_DIR").is_some() {
            command.uid(10001).gid(10001);
        }
        command.spawn().unwrap()
    }
    async fn ready(&mut self) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                assert!(
                    self.child.try_wait().unwrap().is_none(),
                    "service must remain running"
                );
                if self
                    .client
                    .get(format!("{}/ready", self.base))
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
        .expect("service readiness deadline");
    }
    async fn start(&self, idempotency: &str, width: u32) -> Value {
        self.start_with_captions(idempotency, width, true).await
    }
    async fn start_with_captions(&self, idempotency: &str, width: u32, enabled: bool) -> Value {
        let captions = std::env::var("PLAYBACK_TEST_SUBTITLES").as_deref() == Ok("1");
        let response=self.client.post(format!("{}/v1/sessions",self.base)).bearer_auth(&self.key).header("Idempotency-Key",idempotency).json(&json!({"namespace":"fixture","input":{"url":self.source.url,"headers":{},"live":true},"output":{"protocol":"hls","video_codecs":["h264"],"audio_codecs":["aac"],"max_width":width,"max_height":width/16*9,"subtitle_track":if captions && enabled {Some(4)}else{None},"subtitles_off":!enabled,"audio_track":if captions {Some(3)}else{None}},"position_seconds":0})).send().await.map_err(|_|"session request failed").unwrap();
        assert!(response.status().is_success(), "session must start");
        response
            .json()
            .await
            .map_err(|_| "session response failed")
            .unwrap()
    }
    async fn segment(&self, viewer: &Value) {
        let url = viewer["playback"]["url"].as_str().unwrap();
        let response = self
            .client
            .get(format!("{}{url}", self.base))
            .send()
            .await
            .map_err(|_| "playlist request failed")
            .unwrap();
        assert!(response.status().is_success());
        let mut text = response
            .text()
            .await
            .map_err(|_| "playlist body failed")
            .unwrap();
        if url.ends_with("master.m3u8") {
            text = self
                .client
                .get(format!(
                    "{}{}/index.m3u8",
                    self.base,
                    url.rsplit_once('/').unwrap().0
                ))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
        }
        let segment = text.lines().find(|line| line.ends_with(".ts")).unwrap();
        let path = url.rsplit_once('/').unwrap().0;
        let response = self
            .client
            .get(format!("{}{path}/{segment}", self.base))
            .send()
            .await
            .map_err(|_| "segment request failed")
            .unwrap();
        assert!(response.status().is_success());
        assert!(!response
            .bytes()
            .await
            .map_err(|_| "segment body failed")
            .unwrap()
            .is_empty());
    }
    async fn signal(&mut self, name: &str) {
        let signal = match name {
            "-TERM" => libc::SIGTERM,
            "-KILL" => libc::SIGKILL,
            _ => panic!("unsupported fixture signal"),
        };
        // Exact child of this fixture; never target a process group or host PID.
        assert_eq!(
            unsafe { libc::kill(self.child.id().unwrap() as libc::pid_t, signal) },
            0
        );
    }
    fn media_dirs(&self) -> Vec<PathBuf> {
        std::fs::read_dir(state_root(self.root.path()).join("media"))
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .collect()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated built-image subtitle HTTP/media fixture"]
async fn real_shared_subtitles_preserve_sparse_identity_and_decoded_stream_index() {
    assert_eq!(std::env::var("PLAYBACK_TEST_SUBTITLES").as_deref(), Ok("1"));
    let mut fixture = Fixture::new().await;
    let first = fixture.start("captions-first", 128).await;
    assert_eq!(first["playback"]["selected_subtitle"]["input_index"], 4);
    assert_eq!(
        first["playback"]["selected_subtitle"]["output_stream_index"],
        0
    );
    assert_eq!(first["playback"]["selected_audio"]["input_index"], 3);
    let url = first["playback"]["url"].as_str().unwrap();
    let path = url.rsplit_once('/').unwrap().0;
    let playlist = fixture
        .client
        .get(format!("{}{path}/index_vtt.m3u8", fixture.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let file = playlist
        .lines()
        .find(|line| line.ends_with(".vtt"))
        .unwrap();
    let vtt = fixture
        .client
        .get(format!("{}{path}/{file}", fixture.base))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let mut decoder = Command::new(std::env::var("PLAYBACK_TEST_FFPROBE").unwrap())
        .args([
            "-v",
            "error",
            "-f",
            "webvtt",
            "-show_streams",
            "-of",
            "json",
            "-i",
            "pipe:0",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    decoder.stdin.take().unwrap().write_all(&vtt).await.unwrap();
    let output = decoder.wait_with_output().await.unwrap();
    assert!(output.status.success());
    let facts: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        facts["streams"][0]["index"],
        first["playback"]["selected_subtitle"]["output_stream_index"]
    );
    assert_eq!(facts["streams"][0]["codec_name"], "webvtt");
    connections(&fixture.source, 1).await;
    fixture.signal("-TERM").await;
    assert!(
        tokio::time::timeout(Duration::from_secs(8), fixture.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    connections(&fixture.source, 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated built-image caption cache budget fixture"]
async fn real_shared_caption_budget_overflow_fails_safely_and_reclaims_admission() {
    assert_eq!(
        std::env::var("PLAYBACK_TEST_CAPTION_OVERFLOW").as_deref(),
        Ok("1")
    );
    let mut fixture = Fixture::new().await;
    let response=fixture.client.post(format!("{}/v1/sessions",fixture.base)).bearer_auth(&fixture.key).header("Idempotency-Key","caption-overflow").json(&json!({"namespace":"fixture","input":{"url":fixture.source.url,"headers":{},"live":true},"output":{"protocol":"hls","video_codecs":["h264"],"audio_codecs":["aac"],"max_width":128,"max_height":72,"audio_track":3,"subtitle_track":4}})).send().await.unwrap();
    let status = response.status().as_u16();
    assert!(status == 201 || status == 502);
    let value: Value = response.json().await.unwrap();
    if status == 502 {
        assert_eq!(value, json!({"error":{"code":"processing_failed"}}));
    } else {
        // Overflow may occur just after first-segment readiness; it must fail
        // the admitted viewer safely instead of publishing incomplete captions.
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let state: Value = fixture
                    .client
                    .get(format!(
                        "{}/v1/sessions/{}",
                        fixture.base,
                        value["id"].as_str().unwrap()
                    ))
                    .bearer_auth(&fixture.key)
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if state["status"] == "failed" {
                    assert_eq!(state["error_code"], "processing_failed");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("caption cache overflow must terminate affected playback");
    }
    connections(&fixture.source, 0).await;
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let capabilities: Value = fixture
                .client
                .get(format!("{}/v1/capabilities", fixture.base))
                .bearer_auth(&fixture.key)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if capabilities["available"]["inputs"] == 1 && capabilities["available"]["outputs"] == 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("real terminated caption ingest must reclaim its reservations");
    fixture.signal("-TERM").await;
    assert!(
        tokio::time::timeout(Duration::from_secs(8), fixture.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(fixture.media_dirs().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated built-image subtitle HTTP/media fixture"]
async fn real_late_shared_subtitle_output_retains_overlapping_cues_after_eviction() {
    assert_eq!(std::env::var("PLAYBACK_TEST_SUBTITLES").as_deref(), Ok("1"));
    let mut fixture = Fixture::new().await;
    let first = fixture
        .start_with_captions("captions-original", 128, false)
        .await;
    assert!(first["playback"]["selected_subtitle"].is_null());
    let expiring = fixture
        .start_with_captions("captions-expiring", 128, false)
        .await;
    assert_eq!(first["job_id"], expiring["job_id"]);
    let source_opens = fixture.source.requests.load(Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let evicted = fixture.media_dirs().iter().any(|dir| {
                let Ok(text) = std::fs::read_to_string(dir.join("index.m3u8")) else {
                    return false;
                };
                let Some(name) = text.lines().find(|line| line.ends_with(".mkv")) else {
                    return false;
                };
                name != "segment-000000000.mkv" && !dir.join("segment-000000000.mkv").exists()
            });
            if evicted {
                break;
            }
            let response = fixture
                .client
                .post(format!(
                    "{}/v1/sessions/{}/renew",
                    fixture.base,
                    first["id"].as_str().unwrap()
                ))
                .bearer_auth(&fixture.key)
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success());
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .expect("original cue segment must be actually evicted");
    let second = fixture.start("captions-late", 64).await;
    assert_ne!(first["job_id"], second["job_id"]);
    connections(&fixture.source, 1).await;
    assert_eq!(fixture.source.requests.load(Ordering::SeqCst), source_opens);
    let url = second["playback"]["url"].as_str().unwrap();
    let path = url.rsplit_once('/').unwrap().0;
    let playlist = fixture
        .client
        .get(format!("{}{path}/index_vtt.m3u8", fixture.base))
        .send()
        .await
        .unwrap();
    assert!(playlist.status().is_success());
    let playlist = playlist.text().await.unwrap();
    let caption = playlist
        .lines()
        .find(|line| line.ends_with(".vtt"))
        .unwrap();
    let vtt = fixture
        .client
        .get(format!("{}{path}/{caption}", fixture.base))
        .send()
        .await
        .unwrap();
    assert!(vtt.status().is_success());
    let vtt = vtt.text().await.unwrap();
    assert_eq!(vtt.matches("LONG_ENGLISH").count(), 1);
    assert_eq!(vtt.matches("OVERLAPPING_ENGLISH").count(), 1);
    assert!(!vtt.contains("EXPIRED_ENGLISH") && !vtt.contains("WRONG_SPANISH"));
    let cue_facts = demux(vtt.as_bytes(), "webvtt").await;
    let packets = cue_facts["packets"].as_array().unwrap();
    assert_eq!(packets.len(), 2);
    for (packet, expected_start, expected_duration) in
        [(&packets[0], 1.0, 74.0), (&packets[1], 2.0, 63.0)]
    {
        let start = packet["pts_time"].as_str().unwrap().parse::<f64>().unwrap();
        let duration = packet["duration_time"]
            .as_str()
            .unwrap()
            .parse::<f64>()
            .unwrap();
        assert!(
            (start - expected_start).abs() < 0.05 && (duration - expected_duration).abs() < 0.001,
            "decoded caption interval must retain source timing"
        );
    }
    let av_playlist = fixture
        .client
        .get(format!("{}{path}/index.m3u8", fixture.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let names = av_playlist
        .lines()
        .filter(|line| line.ends_with(".ts"))
        .take(2)
        .collect::<Vec<_>>();
    assert_eq!(names.len(), 2);
    for name in &names {
        let response = fixture
            .client
            .get(format!("{}{path}/{name}", fixture.base))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let bytes = response.bytes().await.unwrap();
        let facts = demux(&bytes, "mpegts").await;
        let video = facts["streams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["codec_type"] == "video")
            .unwrap();
        assert_eq!(video["width"], 64);
        let audio = facts["streams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|stream| stream["codec_type"] == "audio")
            .unwrap();
        assert_eq!(
            audio["index"],
            second["playback"]["selected_audio"]["output_stream_index"]
        );
        let frequency = decoded_audio_frequency(&bytes).await;
        assert!(
            (430.0..450.0).contains(&frequency),
            "selected original audio3 must decode its440Hz content, not audio0's880Hz"
        );
        let time = facts["packets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["stream_index"] == video["index"])
            .unwrap()["pts_time"]
            .as_str()
            .unwrap()
            .parse::<f64>()
            .unwrap();
        assert!(
            time > 25.0 && time < 65.0,
            "late AV must retain the same absolute clock as active cues"
        );
        decode(&bytes, "0.5").await;
        let number = name
            .strip_prefix("segment-")
            .unwrap()
            .strip_suffix(".ts")
            .unwrap();
        let captions = fixture
            .client
            .get(format!("{}{path}/index{number}.vtt", fixture.base))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(captions.matches("LONG_ENGLISH").count(), 1);
        assert_eq!(captions.matches("OVERLAPPING_ENGLISH").count(), 1);
        let ids = |text: &str| {
            text.lines()
                .filter(|line| line.starts_with("cue-"))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(&captions),
            ids(&vtt),
            "retained-window seek must reuse stable cue identity"
        );
    }
    let released = fixture
        .client
        .delete(format!(
            "{}/v1/sessions/{}",
            fixture.base,
            first["id"].as_str().unwrap()
        ))
        .bearer_auth(&fixture.key)
        .send()
        .await
        .unwrap();
    assert_eq!(released.status().as_u16(), 204);
    let denied = fixture
        .client
        .get(format!(
            "{}{}",
            fixture.base,
            first["playback"]["url"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap();
    assert!(!denied.status().is_success());
    let tampered = path.rsplit_once('/').unwrap().0;
    assert!(!fixture
        .client
        .get(format!("{}{tampered}/wrong/index_vtt.m3u8", fixture.base))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    assert!(fixture
        .client
        .get(format!("{}{path}/index_vtt.m3u8", fixture.base))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    connections(&fixture.source, 1).await;
    assert_eq!(fixture.source.requests.load(Ordering::SeqCst), source_opens);
    tokio::time::timeout(Duration::from_secs(35), async {
        loop {
            // Media reads deliberately keep happening; they must not renew a lease.
            let response = fixture
                .client
                .get(format!(
                    "{}{}",
                    fixture.base,
                    expiring["playback"]["url"].as_str().unwrap()
                ))
                .send()
                .await
                .unwrap();
            if !response.status().is_success() {
                break;
            }
            let renewal = fixture
                .client
                .post(format!(
                    "{}/v1/sessions/{}/renew",
                    fixture.base,
                    second["id"].as_str().unwrap()
                ))
                .bearer_auth(&fixture.key)
                .send()
                .await
                .unwrap();
            assert!(renewal.status().is_success());
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .expect("unrenewed viewer must expire despite media reads");
    let expired = fixture
        .client
        .post(format!(
            "{}/v1/sessions/{}/renew",
            fixture.base,
            expiring["id"].as_str().unwrap()
        ))
        .bearer_auth(&fixture.key)
        .send()
        .await
        .unwrap();
    assert_eq!(expired.status().as_u16(), 410);
    assert!(fixture
        .client
        .get(format!("{}{path}/index_vtt.m3u8", fixture.base))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    connections(&fixture.source, 1).await;
    let pids = children(fixture.child.id().unwrap());
    assert!(!pids.is_empty());
    let dirs = fixture.media_dirs();
    fixture.signal("-TERM").await;
    assert!(
        tokio::time::timeout(Duration::from_secs(8), fixture.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    connections(&fixture.source, 0).await;
    gone(&pids).await;
    assert!(
        dirs.iter().all(|dir| !dir.exists()),
        "owned replay/output storage must disappear after teardown"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated Docker network-none, NET_ADMIN and current real service/media binaries"]
async fn real_service_active_and_stalled_media_shutdown_reaps_workers() {
    let mut fixture = Fixture::new().await;
    let first = fixture.start("first", 1920).await;
    let second_width = if std::env::var("PLAYBACK_TEST_MATROSKA_AV").as_deref() == Ok("1") {
        64
    } else {
        160
    };
    let second = fixture.start("second", second_width).await;
    assert_ne!(first["job_id"], second["job_id"]);
    connections(&fixture.source, 1).await;
    fixture.segment(&second).await;
    let pids = children(fixture.child.id().unwrap());
    assert_eq!(pids.len(), 3, "one ingest plus two outputs");
    let dirs = fixture.media_dirs();
    let outputs: Vec<_> = dirs
        .iter()
        .filter(|dir| {
            std::fs::read_to_string(dir.join("index.m3u8")).is_ok_and(|text| {
                !text.contains("#EXT-X-INDEPENDENT-SEGMENTS") && !text.contains(".mkv")
            })
        })
        .collect();
    assert_eq!(outputs.len(), 2);
    // Deliberately large synthetic generated file forces TCP backpressure. Its
    // contents are not codec acceptance evidence; the normal segment above is real.
    for output in outputs {
        let large = std::fs::File::create(output.join("segment-999999999.ts")).unwrap();
        large.set_len(32 * 1024 * 1024).unwrap();
    }
    let mut stalled = TcpStream::connect(fixture.address).await.unwrap();
    let path = first["playback"]["url"]
        .as_str()
        .unwrap()
        .rsplit_once('/')
        .unwrap()
        .0;
    stalled.write_all(format!("GET {path}/segment-999999999.ts HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut header = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !header.ends_with(b"\r\n\r\n") {
            header.push(stalled.read_u8().await.unwrap());
        }
    })
    .await
    .unwrap();
    assert!(header.starts_with(b"HTTP/1.1 200"));
    fixture.signal("-TERM").await;
    let status = tokio::time::timeout(Duration::from_secs(8), fixture.child.wait())
        .await
        .expect("unread media must not indefinitely defer worker shutdown")
        .unwrap();
    assert!(status.success());
    gone(&pids).await;
    connections(&fixture.source, 0).await;
    drop(stalled);
    assert!(fixture.media_dirs().is_empty());
    fixture.child = Fixture::spawn(fixture.root.path(), fixture.address);
    fixture.ready().await;
    let denied = fixture
        .client
        .get(format!(
            "{}/v1/sessions/{}",
            fixture.base,
            first["id"].as_str().unwrap()
        ))
        .bearer_auth(&fixture.key)
        .send()
        .await
        .map_err(|_| "restart request failed")
        .unwrap();
    assert_eq!(denied.status().as_u16(), 404);
    let fresh = fixture.start("after-restart", 1920).await;
    fixture.segment(&fresh).await;
    fixture.signal("-TERM").await;
    assert!(
        tokio::time::timeout(Duration::from_secs(8), fixture.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    connections(&fixture.source, 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated Docker network-none, NET_ADMIN and current real service/media binaries"]
async fn real_service_crash_stops_media_children_and_restart_cleans_storage() {
    let mut fixture = Fixture::new().await;
    let viewer = fixture.start("first", 1920).await;
    fixture.segment(&viewer).await;
    connections(&fixture.source, 1).await;
    let pids = children(fixture.child.id().unwrap());
    assert_eq!(pids.len(), 2);
    fixture.signal("-KILL").await;
    assert!(!fixture.child.wait().await.unwrap().success());
    gone(&pids).await;
    connections(&fixture.source, 0).await;
    assert!(
        !fixture.media_dirs().is_empty(),
        "crash leaves recognized media for startup cleanup"
    );
    fixture.child = Fixture::spawn(fixture.root.path(), fixture.address);
    fixture.ready().await;
    assert!(fixture.media_dirs().is_empty());
    let fresh = fixture.start("after-crash", 1920).await;
    fixture.segment(&fresh).await;
    fixture.signal("-TERM").await;
    assert!(
        tokio::time::timeout(Duration::from_secs(8), fixture.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    connections(&fixture.source, 0).await;
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
    (
        status.f_blocks * status.f_frsize,
        (status.f_blocks - status.f_bfree) * status.f_frsize,
    )
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated Docker fixture with dedicated 2 MiB DATA_DIR/media tmpfs"]
async fn real_kernel_bounded_media_full_failure_reclaims_capacity_and_restarts() {
    use std::io::Write;
    assert_eq!(
        std::env::var("PLAYBACK_TEST_DATA_DIR").as_deref(),
        Ok("/var/lib/playback-gateway")
    );
    let mut fixture = Fixture::new().await;
    let media = state_root(fixture.root.path()).join("media");
    let process_status =
        std::fs::read_to_string(format!("/proc/{}/status", fixture.child.id().unwrap())).unwrap();
    assert!(process_status.lines().any(|line| line.starts_with("Uid:")
        && line.split_whitespace().skip(1).all(|uid| uid == "10001")));
    assert!(process_status
        .lines()
        .any(|line| line.starts_with("CapEff:") && line.ends_with("0000000000000000")));
    assert_eq!(allocation(&media).0, 2 * 1024 * 1024);
    let before = allocation(&media).1;
    let sparse = media.join("sparse-fixture.bin");
    let file = std::fs::File::create(&sparse).unwrap();
    file.set_len(1024 * 1024 * 1024).unwrap();
    assert_eq!(
        allocation(&media).1,
        before,
        "logical length is not allocated bytes"
    );
    drop(file);
    std::fs::remove_file(sparse).unwrap();
    let viewer = fixture.start("first", 1920).await;
    if std::env::var("PLAYBACK_TEST_SUBTITLES").as_deref() == Ok("1") {
        assert_eq!(viewer["playback"]["selected_subtitle"]["input_index"], 4);
        assert!(
            fixture
                .media_dirs()
                .iter()
                .any(|dir| std::fs::read_to_string(dir.join("index.m3u8"))
                    .is_ok_and(|text| text.contains(".mkv"))),
            "hard mount must contain actual subtitle replay writes"
        );
    }
    if std::env::var("PLAYBACK_TEST_MATROSKA_AV").as_deref() == Ok("1") {
        assert_eq!(viewer["playback"]["video_mode"], "encode");
        assert!(
            fixture
                .media_dirs()
                .iter()
                .any(|dir| std::fs::read_to_string(dir.join("index.m3u8"))
                    .is_ok_and(|manifest| manifest.contains(".mkv"))),
            "the small hard mount must exercise actual private Matroska writes"
        );
    }
    fixture.segment(&viewer).await;
    connections(&fixture.source, 1).await;
    let pids = children(fixture.child.id().unwrap());
    assert_eq!(pids.len(), 2);
    let fill = media.join("capacity-fixture.bin");
    let mut file = std::fs::File::create(&fill).unwrap();
    let bytes = vec![0u8; 4096];
    loop {
        match file.write_all(&bytes) {
            Ok(()) => assert!(allocation(&media).1 <= 2 * 1024 * 1024),
            Err(error) => {
                assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
                break;
            }
        }
    }
    assert_eq!(allocation(&media).1, 2 * 1024 * 1024);
    let ready = fixture
        .client
        .get(format!("{}/ready", fixture.base))
        .send()
        .await
        .map_err(|_| "ready check failed")
        .unwrap();
    assert_eq!(ready.status().as_u16(), 503);
    let denied=fixture.client.post(format!("{}/v1/sessions",fixture.base)).bearer_auth(&fixture.key).header("Idempotency-Key","full").json(&json!({"namespace":"fixture","input":{"url":fixture.source.url,"headers":{},"live":true},"output":{"protocol":"hls","video_codecs":["h264"],"audio_codecs":["aac"],"max_width":160,"max_height":90}})).send().await.map_err(|_|"full request failed").unwrap();
    assert!(!denied.status().is_success());
    let denied: Value = denied
        .json()
        .await
        .map_err(|_| "full response failed")
        .unwrap();
    assert_eq!(denied["error"]["code"], "not_ready");
    assert!(denied.get("playback").is_none());
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let response = fixture
                .client
                .get(format!(
                    "{}/v1/sessions/{}",
                    fixture.base,
                    viewer["id"].as_str().unwrap()
                ))
                .bearer_auth(&fixture.key)
                .send()
                .await
                .map_err(|_| "status failed")
                .unwrap();
            let state: Value = response
                .json()
                .await
                .map_err(|_| "status body failed")
                .unwrap();
            if state["status"] == "failed" {
                assert_eq!(state["error_code"], "processing_failed");
                assert!(state.get("playback").is_none());
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("filesystem-full producers must fail safely");
    gone(&pids).await;
    connections(&fixture.source, 0).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let response = fixture
                .client
                .get(format!("{}/v1/capabilities", fixture.base))
                .bearer_auth(&fixture.key)
                .send()
                .await
                .map_err(|_| "capacity failed")
                .unwrap();
            let caps: Value = response
                .json()
                .await
                .map_err(|_| "capacity body failed")
                .unwrap();
            if caps["available"]["inputs"] == 1 && caps["available"]["viewers"] == 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("reservations must settle after actual process teardown");
    drop(file);
    std::fs::remove_file(fill).unwrap();
    assert!(allocation(&media).1 < 2 * 1024 * 1024);
    let fresh = fixture.start("after-full", 1920).await;
    fixture.segment(&fresh).await;
    fixture.signal("-TERM").await;
    assert!(fixture.child.wait().await.unwrap().success());
    connections(&fixture.source, 0).await;
    assert!(state_root(fixture.root.path())
        .join("credentials.sqlite")
        .exists());
    assert!(state_root(fixture.root.path())
        .join("instance.lock")
        .exists());
    fixture.child = Fixture::spawn(fixture.root.path(), fixture.address);
    fixture.ready().await;
    let fresh = fixture.start("after-storage-restart", 1920).await;
    fixture.segment(&fresh).await;
    fixture.signal("-TERM").await;
    assert!(fixture.child.wait().await.unwrap().success());
    connections(&fixture.source, 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated Docker fixture, bounded1 GiB media/256 MiB temp and CBR4K generation"]
async fn real_cbr_4k_copy_retains_over_old_budget_and_reclaims_spool() {
    assert_eq!(std::env::var("PLAYBACK_TEST_CBR_4K").as_deref(), Ok("1"));
    assert_eq!(
        std::env::var("LIVE_REPLAY_MAX_BYTES").as_deref(),
        Ok("536870912")
    );
    let mut fixture = Fixture::new().await;
    let first = fixture.start("first", 3840).await;
    assert_eq!(first["playback"]["video_mode"], "copy");
    let output = fixture
        .media_dirs()
        .into_iter()
        .find(|dir| {
            std::fs::read_to_string(dir.join("index.m3u8"))
                .is_ok_and(|text| !text.contains("#EXT-X-INDEPENDENT-SEGMENTS"))
        })
        .unwrap();
    let last_sequence = |dir: &Path| -> u64 {
        std::fs::read_to_string(dir.join("index.m3u8"))
            .unwrap()
            .lines()
            .filter_map(|line| {
                line.strip_prefix("segment-")
                    .and_then(|line| line.strip_suffix(".ts"))
            })
            .map(|sequence| sequence.parse::<u64>().unwrap())
            .max()
            .unwrap()
    };
    let initial_sequence = last_sequence(&output);
    connections(&fixture.source, 1).await;
    let opened = fixture.source.requests.load(Ordering::SeqCst);
    let second = fixture.start("headroom", 7680).await;
    assert_eq!(first["job_id"], second["job_id"]);
    assert_eq!(fixture.source.requests.load(Ordering::SeqCst), opened);
    let media = state_root(fixture.root.path()).join("media");
    assert_eq!(allocation(&media).0, 1024 * 1024 * 1024);
    let ingest = fixture
        .media_dirs()
        .into_iter()
        .find(|dir| {
            std::fs::read_to_string(dir.join("index.m3u8"))
                .is_ok_and(|text| text.contains("#EXT-X-INDEPENDENT-SEGMENTS"))
        })
        .unwrap();
    let start = std::time::Instant::now();
    let mut retained = 0u64;
    let mut allocated = 0u64;
    while start.elapsed() < Duration::from_secs(18) {
        retained = std::fs::read_dir(&ingest)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".ts"))
            .filter_map(|entry| entry.metadata().ok())
            .map(|metadata| metadata.len())
            .sum();
        allocated = allocation(&media).1;
        assert!(allocated <= 1024 * 1024 * 1024);
        connections(&fixture.source, 1).await;
        let status = fixture
            .client
            .get(format!(
                "{}/v1/sessions/{}",
                fixture.base,
                first["id"].as_str().unwrap()
            ))
            .bearer_auth(&fixture.key)
            .send()
            .await
            .map_err(|_| "status failed")
            .unwrap();
        let value: Value = status.json().await.map_err(|_| "status failed").unwrap();
        assert_eq!(value["status"], "ready");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        retained > 64 * 1024 * 1024,
        "real replay must exceed prior fixed ceiling"
    );
    fixture.segment(&first).await;
    let final_sequence = last_sequence(&output);
    assert!(
        final_sequence >= initial_sequence + 8,
        "copy output must continue beyond startup segments"
    );
    println!("CBR output initial_segment_sequence={initial_sequence} final_segment_sequence={final_sequence}");
    let segment = std::fs::read_dir(&ingest)
        .unwrap()
        .flatten()
        .find(|entry| entry.file_name().to_string_lossy().ends_with(".ts"))
        .unwrap()
        .path();
    let ffprobe = std::env::var("PLAYBACK_TEST_FFPROBE").unwrap();
    let inspected = tokio::time::timeout(
        Duration::from_secs(15),
        Command::new(&ffprobe)
            .kill_on_drop(true)
            .args([
                "-v",
                "error",
                "-show_streams",
                "-show_packets",
                "-show_entries",
                "packet=size:stream=codec_type,width,height",
                "-of",
                "json",
                "-i",
            ])
            .arg(&segment)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(inspected.status.success());
    let facts: Value = serde_json::from_slice(&inspected.stdout).unwrap();
    let video = &facts["streams"][0];
    assert_eq!(video["width"], 3840);
    assert_eq!(video["height"], 2160);
    let packet_bytes: u64 = facts["packets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|packet| packet["size"].as_str().unwrap().parse::<u64>().unwrap())
        .sum();
    assert!(
        packet_bytes > 4 * 1024 * 1024,
        "H264 packet bytes must carry filler, not TS null padding"
    );
    let output = fixture
        .media_dirs()
        .into_iter()
        .find(|dir| {
            std::fs::read_to_string(dir.join("index.m3u8"))
                .is_ok_and(|text| !text.contains("#EXT-X-INDEPENDENT-SEGMENTS"))
        })
        .unwrap();
    let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").unwrap();
    let decoded = tokio::time::timeout(
        Duration::from_secs(15),
        Command::new(ffmpeg)
            .kill_on_drop(true)
            .args([
                "-v",
                "error",
                "-threads",
                "2",
                "-protocol_whitelist",
                "file",
                "-allowed_extensions",
                "ts",
                "-i",
            ])
            .arg(output.join("index.m3u8"))
            .args(["-frames:v", "1", "-f", "null", "-"])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        decoded.status.success(),
        "copied actual4K output must decode"
    );
    let pids = children(fixture.child.id().unwrap());
    for viewer in [first, second] {
        let response = fixture
            .client
            .delete(format!(
                "{}/v1/sessions/{}",
                fixture.base,
                viewer["id"].as_str().unwrap()
            ))
            .bearer_auth(&fixture.key)
            .send()
            .await
            .map_err(|_| "release failed")
            .unwrap();
        assert!(response.status().is_success());
    }
    tokio::time::timeout(Duration::from_secs(25), async {
        while !fixture.media_dirs().is_empty() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("release grace must remove retained spool");
    gone(&pids).await;
    connections(&fixture.source, 0).await;
    assert_eq!(allocation(&media).1, 0);
    println!("CBR4K retained_replay_bytes={retained} allocated_media_bytes={allocated} sample_video_packet_bytes={packet_bytes}; allocation_after_release=0");
    fixture.signal("-TERM").await;
    assert!(fixture.child.wait().await.unwrap().success());
}

fn progressive_codecs() -> (&'static str, &'static str, &'static str, &'static str) {
    // (video, audio, file, content type) of the same-container remux.
    if std::env::var("PLAYBACK_TEST_MATROSKA_AV").as_deref() == Ok("1") {
        ("ffv1", "flac", "stream.mkv", "video/x-matroska")
    } else {
        ("h264", "aac", "stream.ts", "video/mp2t")
    }
}
async fn create_progressive(
    fixture: &Fixture,
    key: &str,
    namespace: &str,
    idempotency: &str,
    video: &str,
) -> reqwest::Response {
    let (_, audio, _, _) = progressive_codecs();
    fixture
        .client
        .post(format!("{}/v1/sessions", fixture.base))
        .bearer_auth(key)
        .header("Idempotency-Key", idempotency)
        .json(&json!({"namespace":namespace,"input":{"url":fixture.source.url,"headers":{},"live":true},"output":{"protocol":"progressive","video_codecs":[video],"audio_codecs":[audio],"max_width":1920,"max_height":1080},"position_seconds":0}))
        .send()
        .await
        .map_err(|_| "progressive request failed")
        .unwrap()
}
async fn start_progressive(fixture: &Fixture, key: &str, namespace: &str, id: &str) -> Value {
    let (video, _, file, _) = progressive_codecs();
    let response = create_progressive(fixture, key, namespace, id, video).await;
    assert!(response.status().is_success(), "progressive viewer starts");
    let viewer: Value = response.json().await.unwrap();
    let playback = &viewer["playback"];
    assert!(
        playback["url"].as_str().unwrap().ends_with(file),
        "{playback}"
    );
    viewer
}
/// Continuously consumes one progressive body; keeps its first `keep` bytes.
struct Consumer {
    total: Arc<AtomicUsize>,
    head: Arc<std::sync::Mutex<Vec<u8>>>,
    task: tokio::task::JoinHandle<bool>,
}
impl Consumer {
    fn new(mut response: reqwest::Response, keep: usize) -> Self {
        let total = Arc::new(AtomicUsize::new(0));
        let head = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (counted, kept) = (total.clone(), head.clone());
        let task = tokio::spawn(async move {
            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        let mut kept = kept.lock().unwrap();
                        let room = keep.saturating_sub(kept.len()).min(chunk.len());
                        kept.extend_from_slice(&chunk[..room]);
                        counted.fetch_add(chunk.len(), Ordering::SeqCst);
                    }
                    Ok(None) => return true,
                    Err(_) => return false,
                }
            }
        });
        Self { total, head, task }
    }
    async fn reach(&self, bytes: usize) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while self.total.load(Ordering::SeqCst) < bytes {
                assert!(
                    !self.task.is_finished(),
                    "progressive body must keep flowing"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("progressive body must keep flowing");
    }
    async fn advances(&self) {
        let before = self.total.load(Ordering::SeqCst);
        self.reach(before + 16 * 1024).await;
    }
}
async fn session_state(fixture: &Fixture, key: &str, id: &str) -> (u16, Value) {
    let response = fixture
        .client
        .get(format!("{}/v1/sessions/{id}", fixture.base))
        .bearer_auth(key)
        .send()
        .await
        .map_err(|_| "status failed")
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}
async fn release(fixture: &Fixture, key: &str, id: &str) -> u16 {
    fixture
        .client
        .delete(format!("{}/v1/sessions/{id}", fixture.base))
        .bearer_auth(key)
        .send()
        .await
        .map_err(|_| "release failed")
        .unwrap()
        .status()
        .as_u16()
}
async fn failed_processing(fixture: &Fixture, id: &str, within: Duration) {
    tokio::time::timeout(within, async {
        loop {
            let (_, state) = session_state(fixture, &fixture.key, id).await;
            if state["status"] == "failed" {
                assert_eq!(state["error_code"], "processing_failed");
                assert!(state.get("playback").is_none());
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the affected progressive viewer must fail safely");
}
async fn available(fixture: &Fixture, inputs: u64, outputs: u64, viewers: u64) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let caps: Value = fixture
                .client
                .get(format!("{}/v1/capabilities", fixture.base))
                .bearer_auth(&fixture.key)
                .send()
                .await
                .map_err(|_| "capacity failed")
                .unwrap()
                .json()
                .await
                .map_err(|_| "capacity body failed")
                .unwrap();
            let free = &caps["available"];
            if free["inputs"] == inputs && free["outputs"] == outputs && free["viewers"] == viewers
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("reservations must settle after actual teardown");
}
async fn probe_progressive(bytes: &[u8]) -> Value {
    let format = if progressive_codecs().2 == "stream.mkv" {
        "matroska"
    } else {
        "mpegts"
    };
    demux(bytes, format).await
}
async fn decode_all(bytes: &[u8]) {
    let format = if progressive_codecs().2 == "stream.mkv" {
        "matroska"
    } else {
        "mpegts"
    };
    let mut child = Command::new(std::env::var("PLAYBACK_TEST_FFMPEG").unwrap())
        .args([
            "-v", "error", "-xerror", "-nostdin", "-f", format, "-i", "pipe:0",
        ])
        // Only the first two seconds: the fetched prefix ends mid-packet.
        .args([
            "-map", "0:v:0", "-map", "0:a:0", "-t", "2", "-threads", "2", "-f", "framecrc",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let feeding = async {
        // The decoder may stop reading once two seconds are decoded.
        let _ = input.write_all(bytes).await;
        drop(input);
    };
    let (_, output) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(feeding, child.wait_with_output())
    })
    .await
    .unwrap();
    let output = output.unwrap();
    let frames = String::from_utf8(output.stdout).unwrap();
    let video = frames.lines().filter(|line| line.starts_with("0,")).count();
    let audio = frames.lines().filter(|line| line.starts_with("1,")).count();
    println!("Decoded progressive prefix: video_frames={video} audio_frames={audio}");
    assert!(output.status.success(), "progressive output must decode");
    assert!(
        video >= 10 && audio >= 10,
        "both tracks must actually decode"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated Docker network-none, NET_ADMIN and the built image's service/media binaries"]
async fn real_progressive_image_share_late_join_evict_isolate_and_reap() {
    assert!(progressive());
    let mut fixture = Fixture::new().await;
    let (video, audio, file, content_type) = progressive_codecs();
    let service = fixture.child.id().unwrap();

    // A decoder the client cannot use fails explicitly and opens nothing.
    let refused = create_progressive(&fixture, &fixture.key, "fixture", "hevc", "hevc").await;
    assert_eq!(refused.status().as_u16(), 422, "explicit refusal");
    let refused: Value = refused.json().await.unwrap();
    assert_eq!(refused["error"]["code"], "unsupported_output");
    println!("Undecodable request refused: {}", refused["error"]["code"]);

    let first = start_progressive(&fixture, &fixture.key, "fixture", "first").await;
    connections(&fixture.source, 1).await;
    let opened = fixture.source.requests.load(Ordering::SeqCst);
    let first_url = format!(
        "{}{}",
        fixture.base,
        first["playback"]["url"].as_str().unwrap()
    );
    println!(
        "First viewer playback: {}",
        json!({"format":first["playback"]["format"],"mode":first["playback"]["mode"],"video_mode":first["playback"]["video_mode"],"audio_mode":first["playback"]["audio_mode"]})
    );
    let head = fixture.client.head(&first_url).send().await.unwrap();
    assert_eq!(head.status().as_u16(), 200);
    assert_eq!(head.headers()["content-type"], content_type);
    assert_eq!(head.headers()["accept-ranges"], "none");
    assert!(
        head.headers().get("content-length").is_none() || head.headers()["content-length"] == "0"
    );
    let response = fixture
        .client
        .get(&first_url)
        .timeout(Duration::from_secs(600))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.headers()["accept-ranges"], "none");
    assert!(response.headers().get("content-length").is_none());
    let first_reader = Consumer::new(response, 256 * 1024);
    first_reader.reach(256 * 1024).await;
    {
        let head = first_reader.head.lock().unwrap();
        if file == "stream.ts" {
            assert_eq!(head[0], 0x47, "fresh MPEG-TS packets");
        } else {
            assert_eq!(&head[..4], &[0x1a, 0x45, 0xdf, 0xa3], "fresh EBML header");
        }
    }
    // Once opened, the same viewer's media URL is single-consumer.
    let again = fixture.client.get(&first_url).send().await.unwrap();
    assert!(!again.status().is_success(), "second GET is refused");

    // A late compatible viewer joins the same upstream.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let late = start_progressive(&fixture, &fixture.key, "fixture", "late").await;
    assert_ne!(late["id"], first["id"]);
    connections(&fixture.source, 1).await;
    assert_eq!(
        fixture.source.requests.load(Ordering::SeqCst),
        opened,
        "a progressive late join must not re-open the provider"
    );
    let late_path = late["playback"]["url"].as_str().unwrap().to_owned();
    let late_url = format!("{}{late_path}", fixture.base);
    let first_token = first["playback"]["url"]
        .as_str()
        .unwrap()
        .split('/')
        .nth(3)
        .unwrap();
    let mut crossed: Vec<&str> = late_path.split('/').collect();
    crossed[3] = first_token;
    let crossed = fixture
        .client
        .get(format!("{}{}", fixture.base, crossed.join("/")))
        .send()
        .await
        .unwrap();
    assert!(
        !crossed.status().is_success(),
        "media tokens are per viewer"
    );
    let response = fixture
        .client
        .get(&late_url)
        .timeout(Duration::from_secs(600))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let late_reader = Consumer::new(response, 1024 * 1024);
    late_reader.reach(1024 * 1024).await;
    first_reader.advances().await;
    let late_bytes = late_reader.head.lock().unwrap().clone();
    let facts = probe_progressive(&late_bytes).await;
    let streams: Vec<(String, String)> = facts["streams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|stream| {
            (
                stream["codec_type"].as_str().unwrap().to_owned(),
                stream["codec_name"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    println!("Late output streams: {streams:?}");
    assert_eq!(
        streams,
        vec![
            ("video".into(), video.into()),
            ("audio".into(), audio.into())
        ],
        "same codecs, first video and the selected audio only"
    );
    let first_video = facts["packets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|packet| packet["codec_type"] == "video")
        .unwrap();
    assert!(
        first_video["flags"].as_str().unwrap().starts_with('K'),
        "late join starts at a keyframe: {first_video}"
    );
    decode_all(&late_bytes).await;

    // Namespace isolation: another namespace never shares or sees these viewers.
    let other_key: Value = fixture
        .client
        .post(format!("{}/v1/keys", fixture.base))
        .bearer_auth(BOOTSTRAP)
        .json(&json!({"label":"other namespace","namespaces":["other"],"scopes":["create","read","release"],"quotas":{"inputs":1,"outputs":1,"viewers":1}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let other_key = other_key["secret"].as_str().unwrap().to_owned();
    let denied = create_progressive(&fixture, &other_key, "fixture", "wrong-ns", video).await;
    assert!(
        !denied.status().is_success(),
        "a key cannot use another namespace"
    );
    let other = start_progressive(&fixture, &other_key, "other", "other").await;
    connections(&fixture.source, 2).await;
    // Its own source preparation and its own ingest; never the fixture's.
    let after_other = fixture.source.requests.load(Ordering::SeqCst);
    assert!(
        after_other > opened,
        "another namespace opens its own upstream"
    );
    println!(
        "Other namespace provider requests: {} (fixture namespace: {opened})",
        after_other - opened
    );
    let (status, _) = session_state(&fixture, &other_key, first["id"].as_str().unwrap()).await;
    assert!(
        !(200..300).contains(&status),
        "other namespace cannot read a viewer"
    );
    assert!(
        !(200..300).contains(&release(&fixture, &other_key, first["id"].as_str().unwrap()).await)
    );
    let (status, _) = session_state(&fixture, &fixture.key, other["id"].as_str().unwrap()).await;
    assert!(!(200..300).contains(&status));
    let other_path = other["playback"]["url"].as_str().unwrap().to_owned();
    let mut crossed: Vec<&str> = other_path.split('/').collect();
    crossed[3] = first_token;
    let crossed = fixture
        .client
        .get(format!("{}{}", fixture.base, crossed.join("/")))
        .send()
        .await
        .unwrap();
    assert!(!crossed.status().is_success());
    let response = fixture
        .client
        .get(format!("{}{other_path}", fixture.base))
        .timeout(Duration::from_secs(600))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let other_reader = Consumer::new(response, 0);
    other_reader.reach(64 * 1024).await;
    println!("Namespace isolation: separate upstream, cross-namespace read/release/token denied");
    assert_eq!(
        release(&fixture, &other_key, other["id"].as_str().unwrap()).await,
        204
    );

    // A slow reader: opened, then never read, with a tiny receive window.
    let slow = start_progressive(&fixture, &fixture.key, "fixture", "slow").await;
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    let mut stalled = socket.connect(fixture.address).await.unwrap();
    stalled
        .write_all(
            format!(
                "GET {} HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n",
                slow["playback"]["url"].as_str().unwrap()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        header.push(stalled.read_u8().await.unwrap());
    }
    assert!(header.starts_with(b"HTTP/1.1 200"));
    let stalled_at = std::time::Instant::now();
    // Releasing one viewer during the stall leaves the others running.
    assert_eq!(
        release(&fixture, &fixture.key, first["id"].as_str().unwrap()).await,
        204
    );
    tokio::time::timeout(Duration::from_secs(30), &mut { first_reader.task })
        .await
        .expect("a released viewer's stream ends")
        .unwrap();
    println!(
        "First viewer released after {:?}; late viewer continues",
        stalled_at.elapsed()
    );
    late_reader.advances().await;
    tokio::time::timeout(Duration::from_secs(30), &mut { other_reader.task })
        .await
        .expect("the released other-namespace stream ends")
        .unwrap();
    connections(&fixture.source, 1).await;
    failed_processing(
        &fixture,
        slow["id"].as_str().unwrap(),
        Duration::from_secs(150),
    )
    .await;
    println!(
        "Stalled reader evicted (processing_failed) after {:?}",
        stalled_at.elapsed()
    );
    late_reader.advances().await;
    connections(&fixture.source, 1).await;
    let mut rest = Vec::new();
    let drained =
        tokio::time::timeout(Duration::from_secs(30), stalled.read_to_end(&mut rest)).await;
    assert!(drained.is_ok(), "the evicted response must end");
    assert!(
        !rest.ends_with(b"0\r\n\r\n"),
        "eviction ends the body with an error, not a clean end"
    );
    println!(
        "Evicted body ended after {} buffered bytes ({:?})",
        rest.len(),
        drained.unwrap().map(|_| "eof")
    );
    assert_eq!(
        fixture.source.requests.load(Ordering::SeqCst),
        after_other,
        "a slow or released viewer never re-opens the provider"
    );

    // The last release cancels the reader, the ingest and its storage.
    let pids = children(service);
    println!("Media children before last release: {}", pids.len());
    assert!(!pids.is_empty());
    let last_release = std::time::Instant::now();
    assert_eq!(
        release(&fixture, &fixture.key, late["id"].as_str().unwrap()).await,
        204
    );
    tokio::time::timeout(Duration::from_secs(30), &mut { late_reader.task })
        .await
        .expect("the last viewer's stream ends")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while fixture.source.active.load(Ordering::SeqCst) != 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("upstream closes after last release");
    gone(&pids).await;
    assert!(children(service).is_empty());
    tokio::time::timeout(Duration::from_secs(10), async {
        while !fixture.media_dirs().is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("replay storage is reclaimed");
    available(&fixture, 1, 4, 5).await;
    println!(
        "Last release reaped processes, upstream, storage and reservations in {:?}",
        last_release.elapsed()
    );
    fixture.signal("-TERM").await;
    assert!(fixture.child.wait().await.unwrap().success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated Docker fixture with dedicated 2 MiB DATA_DIR/media tmpfs"]
async fn real_progressive_kernel_bounded_media_full_fails_and_reclaims() {
    use std::io::Write;
    assert!(progressive());
    assert_eq!(
        std::env::var("PLAYBACK_TEST_DATA_DIR").as_deref(),
        Ok("/var/lib/playback-gateway")
    );
    let mut fixture = Fixture::new().await;
    let service = fixture.child.id().unwrap();
    let media = state_root(fixture.root.path()).join("media");
    let process_status = std::fs::read_to_string(format!("/proc/{service}/status")).unwrap();
    assert!(process_status.lines().any(|line| line.starts_with("Uid:")
        && line.split_whitespace().skip(1).all(|uid| uid == "10001")));
    assert_eq!(allocation(&media).0, 2 * 1024 * 1024);
    let viewer = start_progressive(&fixture, &fixture.key, "fixture", "first").await;
    let response = fixture
        .client
        .get(format!(
            "{}{}",
            fixture.base,
            viewer["playback"]["url"].as_str().unwrap()
        ))
        .timeout(Duration::from_secs(600))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let reader = Consumer::new(response, 0);
    reader.reach(64 * 1024).await;
    connections(&fixture.source, 1).await;
    assert!(
        fixture
            .media_dirs()
            .iter()
            .any(|dir| dir.join("index.m3u8").exists()),
        "the hard mount holds the shared private replay"
    );
    let pids = children(service);
    assert_eq!(pids.len(), 2, "one ingest and one progressive reader");
    let fill = media.join("capacity-fixture.bin");
    let mut file = std::fs::File::create(&fill).unwrap();
    let bytes = vec![0u8; 4096];
    loop {
        match file.write_all(&bytes) {
            Ok(()) => assert!(allocation(&media).1 <= 2 * 1024 * 1024),
            Err(error) => {
                assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
                break;
            }
        }
    }
    assert_eq!(allocation(&media).1, 2 * 1024 * 1024);
    println!(
        "Media tmpfs full: ENOSPC at {} bytes allocated",
        allocation(&media).1
    );
    let ready = fixture
        .client
        .get(format!("{}/ready", fixture.base))
        .send()
        .await
        .unwrap();
    assert_eq!(ready.status().as_u16(), 503);
    let (video, _, _, _) = progressive_codecs();
    let denied = create_progressive(&fixture, &fixture.key, "fixture", "full", video).await;
    assert!(!denied.status().is_success());
    let denied: Value = denied.json().await.unwrap();
    assert_eq!(denied["error"]["code"], "not_ready");
    failed_processing(
        &fixture,
        viewer["id"].as_str().unwrap(),
        Duration::from_secs(60),
    )
    .await;
    let ended = tokio::time::timeout(Duration::from_secs(30), &mut { reader.task })
        .await
        .expect("the affected progressive stream ends")
        .unwrap();
    assert!(!ended, "the affected stream ends with an error");
    gone(&pids).await;
    connections(&fixture.source, 0).await;
    available(&fixture, 1, 4, 5).await;
    drop(file);
    std::fs::remove_file(fill).unwrap();
    assert!(allocation(&media).1 < 2 * 1024 * 1024);
    println!(
        "Full storage failed the viewer safely; processes, upstream and reservations reclaimed"
    );
    let fresh = start_progressive(&fixture, &fixture.key, "fixture", "after-full").await;
    let response = fixture
        .client
        .get(format!(
            "{}{}",
            fixture.base,
            fresh["playback"]["url"].as_str().unwrap()
        ))
        .timeout(Duration::from_secs(600))
        .send()
        .await
        .unwrap();
    let fresh_reader = Consumer::new(response, 0);
    fresh_reader.reach(64 * 1024).await;
    fixture.signal("-TERM").await;
    assert!(fixture.child.wait().await.unwrap().success());
    connections(&fixture.source, 0).await;
}
