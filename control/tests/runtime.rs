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
                    loop {for chunk in bytes.chunks(if high {188*4096}else{188*100}){if socket.write_all(chunk).await.is_err(){return}tokio::time::sleep(Duration::from_millis(if high {150}else{30})).await;}}
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
struct Fixture {
    root: tempfile::TempDir,
    child: Child,
    base: String,
    address: std::net::SocketAddr,
    source: Source,
    client: reqwest::Client,
    key: String,
}
impl Fixture {
    async fn new() -> Self {
        isolated_address();
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input.ts");
        let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").unwrap();
        let high = std::env::var("PLAYBACK_TEST_CBR_4K").as_deref() == Ok("1");
        let matroska = std::env::var("PLAYBACK_TEST_MATROSKA_AV").as_deref() == Ok("1");
        assert!(!(high && matroska));
        let mut generation = Command::new(&ffmpeg);
        generation.kill_on_drop(true);
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            generation
                .args([
                    "-v",
                    "error",
                    "-nostdin",
                    "-f",
                    "lavfi",
                    "-i",
                    if high {
                        "color=c=black:size=3840x2160:rate=5"
                    } else if matroska {
                        "testsrc2=size=128x72:rate=10"
                    } else {
                        "testsrc2=size=320x180:rate=25"
                    },
                    "-t",
                    if high { "24" } else { "8" },
                    "-an",
                    "-c:v",
                    if matroska { "ffv1" } else { "libx264" },
                    "-threads",
                    "2",
                    "-g",
                    if high { "5" } else { "25" },
                    "-f",
                    if matroska { "matroska" } else { "mpegts" },
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
        let source = source(std::fs::read(input).unwrap()).await;
        let port = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = port.local_addr().unwrap();
        drop(port);
        let child = Self::spawn(root.path(), address);
        let base = format!("http://{address}");
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(35))
            .build()
            .unwrap();
        let mut fixture = Self {
            root,
            child,
            base,
            address,
            source,
            client,
            key: String::new(),
        };
        fixture.ready().await;
        let response=fixture.client.post(format!("{}/v1/keys",fixture.base)).bearer_auth(BOOTSTRAP).json(&json!({"label":"runtime fixture","namespaces":["fixture"],"scopes":["create","read","release","capabilities"],"quotas":{"inputs":1,"outputs":2,"viewers":3}})).send().await.map_err(|_|"key request failed").unwrap();
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
            .env("MAX_OUTPUTS", "2")
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
        let response=self.client.post(format!("{}/v1/sessions",self.base)).bearer_auth(&self.key).header("Idempotency-Key",idempotency).json(&json!({"namespace":"fixture","input":{"url":self.source.url,"headers":{},"live":true},"output":{"protocol":"hls","video_codecs":["h264"],"audio_codecs":["aac"],"max_width":width,"max_height":width/16*9},"position_seconds":0})).send().await.map_err(|_|"session request failed").unwrap();
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
        let text = response
            .text()
            .await
            .map_err(|_| "playlist body failed")
            .unwrap();
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
#[ignore = "requires isolated Docker network-none, NET_ADMIN and current real service/media binaries"]
async fn real_service_active_and_stalled_media_shutdown_reaps_workers() {
    let mut fixture = Fixture::new().await;
    let first = fixture.start("first", 1920).await;
    let second = fixture.start("second", 160).await;
    assert_ne!(first["job_id"], second["job_id"]);
    connections(&fixture.source, 1).await;
    fixture.segment(&second).await;
    let pids = children(fixture.child.id().unwrap());
    assert_eq!(pids.len(), 3, "one ingest plus two outputs");
    let dirs = fixture.media_dirs();
    let outputs: Vec<_> = dirs
        .iter()
        .filter(|dir| {
            std::fs::read_to_string(dir.join("index.m3u8"))
                .is_ok_and(|text| !text.contains("#EXT-X-INDEPENDENT-SEGMENTS"))
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
