//! Engine/ledger bridge. No HTTP route may bypass this layer to create media.
use crate::{
    credentials::{Credentials, Principal, Scope},
    network::{EgressProxy, NetworkPolicy},
    protocol::{CreateSession, OutputProtocol, STARTUP_DEADLINE_SECONDS},
    sessions::{Failure, Sessions, Status, Viewer},
};
use playback_engine::{Capabilities, PlaybackManager, PlaybackResponse, TrackSelection};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[cfg(all(test, unix))]
#[path = "workers_tls_tests.rs"]
mod tls_tests;

pub struct Workers {
    pub sessions: Arc<Mutex<Sessions>>,
    pub credentials: Arc<Credentials>,
    engine: Arc<PlaybackManager>,
    proxy: EgressProxy,
    active: Mutex<BTreeMap<String, PlaybackResponse>>,
    starting: Mutex<BTreeSet<String>>,
    reaper: tokio::sync::Mutex<()>,
}
impl Workers {
    pub fn media_storage_available(&self) -> bool {
        self.engine.media_storage_available()
    }
    pub fn available(
        &self,
        principal: &Principal,
        now: u64,
    ) -> Result<crate::credentials::Quotas, &'static str> {
        let mut available = self
            .sessions
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .available(principal, now);
        available.outputs = available
            .outputs
            .min(self.engine.available_output_slots() as u32);
        if !self.media_storage_available() {
            available.outputs = 0;
        }
        Ok(available)
    }
    /// Safe control response projection: substitute viewer-scoped media URLs
    /// and IDs; never expose the engine capability or source authorization.
    pub(crate) fn delivery(
        &self,
        principal: &Principal,
        viewer: &Viewer,
        now: u64,
    ) -> Result<Option<PlaybackResponse>, &'static str> {
        if viewer.status != Status::Ready {
            return Ok(None);
        }
        let token = self
            .sessions
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .ensure_media(principal, &viewer.id, now)?;
        let mut response = self
            .active
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .get(&viewer.job_id)
            .cloned()
            .ok_or("media_unavailable")?;
        let file = response.url.rsplit('/').next().ok_or("media_unavailable")?;
        response.url = format!("/media/{}/{}/{file}", viewer.id, token.token);
        response.id = viewer.id.clone();
        Ok(Some(response))
    }
    pub async fn shutdown(&self) {
        self.engine.shutdown().await;
    }
    pub async fn new(
        engine: Arc<PlaybackManager>,
        credentials: Arc<Credentials>,
        policy: NetworkPolicy,
    ) -> Result<Self, &'static str> {
        engine
            .initialize()
            .await
            .map_err(|_| "media_storage_unavailable")?;
        Ok(Self {
            sessions: Arc::new(Mutex::new(Sessions::default())),
            credentials,
            engine,
            proxy: EgressProxy::start(policy)
                .await
                .map_err(|_| "egress_unavailable")?,
            active: Mutex::new(BTreeMap::new()),
            starting: Mutex::new(BTreeSet::new()),
            reaper: tokio::sync::Mutex::new(()),
        })
    }

    /// Principal must come from current control authentication. Recheck its key
    /// after inspection/startup because revocation may race network I/O.
    pub async fn create(
        &self,
        principal: &Principal,
        request: &CreateSession,
        idempotency: &str,
        now: u64,
    ) -> Result<Viewer, &'static str> {
        request.validate()?;
        principal.require(&request.namespace, Scope::Create)?;
        self.require_active(principal, now).await?;
        if let Some(viewer) = self
            .sessions
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .replay(principal, request, idempotency, now)?
        {
            return Ok(viewer);
        }
        let started = Instant::now();
        let deadline = Duration::from_secs(STARTUP_DEADLINE_SECONDS);
        let has = |codecs: &[String], name: &str| codecs.iter().any(|codec| codec == name);
        let progressive = request.output.protocol == OutputProtocol::Progressive;
        let caps = Capabilities {
            h264: has(&request.output.video_codecs, "h264"),
            aac: has(&request.output.audio_codecs, "aac"),
            hevc: has(&request.output.video_codecs, "hevc"),
            hevc_sdr: has(&request.output.video_codecs, "hevc"),
            max_width: request.output.max_width,
            max_height: request.output.max_height,
            direct_play: true,
            direct_files: Some(progressive),
            direct_mp4: Some(progressive),
            direct_hls: Some(!progressive),
            direct_video_codecs: Some(request.output.video_codecs.clone()),
            direct_audio_codecs: Some(request.output.audio_codecs.clone()),
            ..Capabilities::default()
        };
        let mut headers: HashMap<_, _> = request
            .input
            .headers
            .iter()
            .map(|(key, value)| (key.to_ascii_lowercase(), value.clone()))
            .collect();
        if headers.len() != request.input.headers.len() {
            return Err("invalid_headers");
        }
        headers.insert("x-playback-egress-proxy".into(), self.proxy.url());
        let prepared = tokio::time::timeout(
            deadline,
            self.engine.prepare_shared_with_selection(
                request.input.url.clone(),
                headers,
                request.position_seconds,
                Some(caps),
                request.output.conversion != crate::protocol::Conversion::Auto,
                request.input.live,
                TrackSelection {
                    conversion_reason: match request.output.conversion {
                        crate::protocol::Conversion::Audio => Some("audio-codec".into()),
                        crate::protocol::Conversion::Video => Some("video-codec".into()),
                        _ => None,
                    },
                    audio_track_index: request.output.audio_track,
                    subtitle_track_index: request.output.subtitle_track,
                    audio_language: request.output.audio_language.clone(),
                    preferred_audio_language: request.output.preferred_audio_language.clone(),
                    preferred_subtitle_language: request.output.preferred_subtitle_language.clone(),
                },
                request.namespace.clone(),
            ),
        )
        .await
        .map_err(|_| "startup_timeout")?
        .map_err(|_| "source_preparation_failed")?;
        let plan = prepared.plan.as_ref().ok_or("unsupported_output")?;
        // Live progressive is the same-container remux subset only. On-demand
        // progressive keeps its pre-existing original-file transport.
        let accepted = if progressive && request.input.live {
            crate::sessions::single_consumer(plan)
        } else if progressive {
            plan.format != "hls" && plan.shareable
        } else {
            plan.format == "hls" && plan.shareable
        };
        if !accepted {
            return Err("unsupported_output");
        }
        let timestamp = now.saturating_add(started.elapsed().as_secs());
        self.require_active(principal, timestamp).await?;
        let reservation = self
            .sessions
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .reserve(principal, request, plan, idempotency, timestamp)?;
        if !reservation.start_job {
            return Ok(reservation.viewer);
        }
        let job = reservation.viewer.job_id.clone();
        self.starting
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .insert(job.clone());
        let mut guard = Startup {
            workers: self,
            job: job.clone(),
            timestamp,
            complete: false,
        };
        let remaining = deadline.saturating_sub(started.elapsed());
        let outcome = tokio::time::timeout(remaining, prepared.start()).await;
        let response = match outcome {
            Ok(Ok(response)) => response,
            _ => return Err("processing_failed"),
        };
        let timestamp = now.saturating_add(started.elapsed().as_secs());
        // Record the engine ID before another await so cancellation cleanup can
        // always find an engine session that has successfully started.
        self.active
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .insert(job.clone(), response);
        if self.require_active(principal, timestamp).await.is_err() {
            return Err("unauthorized");
        }
        if !self
            .sessions
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .ready(&job, timestamp)
        {
            return Err("session_expired");
        }
        guard.complete = true;
        self.starting
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .remove(&job);
        let mut viewer = reservation.viewer;
        viewer.status = Status::Ready;
        Ok(viewer)
    }

    async fn key_active(&self, id: &str, now: u64) -> Result<bool, &'static str> {
        let now = i64::try_from(now).map_err(|_| "unauthorized")?;
        let credentials = self.credentials.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || credentials.active(&id, now))
            .await
            .map_err(|_| "credential_store_unavailable")?
    }
    async fn require_active(&self, principal: &Principal, now: u64) -> Result<(), &'static str> {
        if self.key_active(&principal.key.id, now).await? {
            Ok(())
        } else {
            Err("unauthorized")
        }
    }

    /// Called periodically by the runtime, and after explicit release/revoke.
    /// The ledger retains quota until both processes and cancelled I/O settle.
    pub async fn reap(&self, now: u64) -> Result<(), &'static str> {
        let _reaper = self.reaper.lock().await;
        let owners = self
            .sessions
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .active_owners();
        for owner in owners {
            if !self.key_active(&owner, now).await? {
                self.sessions
                    .lock()
                    .map_err(|_| "session_store_unavailable")?
                    .revoke(&owner, now);
            }
        }
        let active: Vec<_> = self
            .active
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .iter()
            .map(|(job, response)| (job.clone(), response.id.clone()))
            .collect();
        for (job, id) in active {
            if !self.engine.heartbeat(&id).await {
                self.sessions
                    .lock()
                    .map_err(|_| "session_store_unavailable")?
                    .fail(&job, Failure::ProcessingFailed, now);
            }
        }
        let jobs = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| "session_store_unavailable")?;
            sessions.reap(now);
            sessions.stopping_jobs()
        };
        for job in jobs {
            if self
                .starting
                .lock()
                .map_err(|_| "session_store_unavailable")?
                .contains(&job)
            {
                continue;
            }
            let response = self
                .active
                .lock()
                .map_err(|_| "session_store_unavailable")?
                .remove(&job);
            if let Some(response) = response {
                tokio::time::timeout(Duration::from_secs(5), self.engine.stop(&response.id))
                    .await
                    .map_err(|_| "cleanup_pending")?;
            }
            tokio::time::timeout(
                Duration::from_secs(5),
                self.engine.settle_cancelled_inputs(),
            )
            .await
            .map_err(|_| "cleanup_pending")?;
            self.sessions
                .lock()
                .map_err(|_| "session_store_unavailable")?
                .finish_stop(&job);
        }
        Ok(())
    }

    /// Media paths resolve only after independent viewer authorization. The
    /// caller must never return this internal engine capability to a client.
    pub(crate) async fn media_target(
        &self,
        viewer: &str,
        token: &str,
        now: u64,
    ) -> Result<PlaybackResponse, &'static str> {
        let sessions = self.sessions.clone();
        let credentials = self.credentials.clone();
        let viewer = viewer.to_owned();
        let token = token.to_owned();
        let job = tokio::task::spawn_blocking(move || {
            sessions
                .lock()
                .map_err(|_| "media_unauthorized")?
                .authorize_media(&credentials, &viewer, &token, now)
        })
        .await
        .map_err(|_| "media_unauthorized")??;
        self.active
            .lock()
            .map_err(|_| "media_unavailable")?
            .get(&job)
            .cloned()
            .ok_or("media_unavailable")
    }

    pub async fn media(
        &self,
        viewer: &str,
        token: &str,
        file: &str,
        now: u64,
        method: axum::http::Method,
        headers: axum::http::HeaderMap,
    ) -> Result<axum::response::Response, &'static str> {
        if !matches!(method, axum::http::Method::GET | axum::http::Method::HEAD) {
            return Err("method_not_allowed");
        }
        let target = self.media_target(viewer, token, now).await?;
        if file.len() > 160
            || file.contains("..")
            || !file
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err("media_not_found");
        }
        let mut pieces = target
            .url
            .strip_prefix("/media/")
            .ok_or("media_unavailable")?
            .split('/');
        let id = pieces.next().ok_or("media_unavailable")?;
        let capability = pieces.next().ok_or("media_unavailable")?;
        if id != target.id {
            return Err("media_unavailable");
        }
        let mut response = match self
            .engine
            .serve_original(id, capability, file, method.clone(), headers)
            .await
        {
            Some(result) => result.map_err(|_| "media_unavailable")?,
            None => self
                .engine
                .serve(id, capability, file)
                .await
                .map_err(|_| "media_unavailable")?,
        };
        if method == axum::http::Method::HEAD {
            *response.body_mut() = axum::body::Body::empty();
        }
        response.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            "no-store".parse().unwrap(),
        );
        response.headers_mut().insert(
            axum::http::header::REFERRER_POLICY,
            "no-referrer".parse().unwrap(),
        );
        Ok(response)
    }
}

struct Startup<'a> {
    workers: &'a Workers,
    job: String,
    timestamp: u64,
    complete: bool,
}
impl Drop for Startup<'_> {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        if let Ok(mut sessions) = self.workers.sessions.lock() {
            sessions.fail(&self.job, Failure::ProcessingFailed, self.timestamp);
        }
        if let Ok(mut starting) = self.workers.starting.lock() {
            starting.remove(&self.job);
        }
        // Reaper performs async teardown. Dropping the in-flight prepared future
        // already schedules child termination in the engine; quotas stay held.
    }
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use crate::{
        credentials::{KeySpec, Quotas},
        protocol::{Input, Output},
    };
    use std::os::unix::fs::PermissionsExt;

    struct HttpSource {
        url: String,
        active: Arc<std::sync::atomic::AtomicUsize>,
        requests: Arc<std::sync::atomic::AtomicUsize>,
        server: tokio::task::JoinHandle<()>,
    }
    impl Drop for HttpSource {
        fn drop(&mut self) {
            self.server.abort();
        }
    }
    struct ConnectionCount(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for ConnectionCount {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    async fn http_source(bytes: Vec<u8>) -> HttpSource {
        http_source_with_interval(bytes, Duration::from_millis(30)).await
    }
    async fn http_source_with_interval(bytes: Vec<u8>, delay: Duration) -> HttpSource {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/live.ts", listener.local_addr().unwrap());
        let active = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = active.clone();
        let opened = requests.clone();
        let bytes = Arc::new(bytes);
        let server = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    incoming = listener.accept() => {
                        let Ok((mut socket, _)) = incoming else { break; };
                        let observed = observed.clone(); let opened = opened.clone(); let bytes = bytes.clone();
                        clients.spawn(async move {
                            observed.fetch_add(1, Ordering::SeqCst); opened.fetch_add(1, Ordering::SeqCst);
                            let _count = ConnectionCount(observed);
                            let mut request = Vec::new();
                            while !request.ends_with(b"\r\n\r\n") && request.len() < 16384 {
                                match socket.read_u8().await { Ok(byte) => request.push(byte), Err(_) => return }
                            }
                            assert!(!String::from_utf8_lossy(&request).contains("x-playback-egress-proxy"));
                            if socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nConnection: close\r\n\r\n").await.is_err() { return; }
                            loop {
                                for chunk in bytes.chunks(188 * 100) {
                                    if socket.write_all(chunk).await.is_err() { return; }
                                    tokio::time::sleep(delay).await;
                                }
                            }
                        });
                    },
                    _ = clients.join_next(), if !clients.is_empty() => {}
                }
            }
        });
        HttpSource {
            url,
            active,
            requests,
            server,
        }
    }
    async fn expect_connections(source: &HttpSource, expected: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while source.active.load(std::sync::atomic::Ordering::SeqCst) != expected {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("upstream connection count must settle");
    }

    async fn http_hls_source(
        parts: Vec<Vec<u8>>,
    ) -> (HttpSource, Arc<Mutex<BTreeMap<String, usize>>>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/index.m3u8", listener.local_addr().unwrap());
        let active = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let records = Arc::new(Mutex::new(BTreeMap::<String, usize>::new()));
        let observed = active.clone();
        let opened = requests.clone();
        let fetched = records.clone();
        let parts = Arc::new(parts);
        let started = Instant::now();
        let server = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((mut socket, _)) = accepted else { break };
                        let observed = observed.clone(); let opened = opened.clone();
                        let fetched = fetched.clone(); let parts = parts.clone();
                        clients.spawn(async move {
                            observed.fetch_add(1, Ordering::SeqCst);
                            opened.fetch_add(1, Ordering::SeqCst);
                            let _count = ConnectionCount(observed);
                            let mut request = Vec::new();
                            while !request.ends_with(b"\r\n\r\n") && request.len() < 16384 {
                                match socket.read_u8().await { Ok(byte) => request.push(byte), Err(_) => return }
                            }
                            let text = String::from_utf8_lossy(&request);
                            let path = text.split_whitespace().nth(1).unwrap_or("").to_owned();
                            let (mime, body) = if path == "/index.m3u8" {
                                let sequence = started.elapsed().as_millis() as u64 / 500;
                                let mut playlist = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:{sequence}\n");
                                for part in sequence..sequence + 3 {
                                    playlist.push_str(&format!("#EXT-X-DISCONTINUITY\n#EXTINF:2,\npart-{part}.ts\n"));
                                }
                                ("application/vnd.apple.mpegurl", playlist.into_bytes())
                            } else if let Some(index) = path.strip_prefix("/part-").and_then(|p| p.strip_suffix(".ts")).and_then(|p| p.parse::<usize>().ok()) {
                                *fetched.lock().unwrap().entry(path).or_default() += 1;
                                ("video/mp2t", parts[index % parts.len()].clone())
                            } else { return };
                            let header = format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                            let _ = socket.write_all(header.as_bytes()).await;
                            let _ = socket.write_all(&body).await;
                        });
                    },
                    _ = clients.join_next(), if !clients.is_empty() => {}
                }
            }
        });
        (
            HttpSource {
                url,
                active,
                requests,
                server,
            },
            records,
        )
    }

    #[tokio::test]
    #[ignore = "requires real FFmpeg/ffprobe; isolated HTTP HLS live fixture"]
    async fn real_hls_multi_output_reuses_ingest_without_duplicate_child_fetches() {
        check_real_hls_outputs(false).await;
    }

    #[tokio::test]
    #[ignore = "requires real FFmpeg/libx265/ffprobe; isolated HTTP HLS live fixture"]
    async fn real_hevc_hls_copy_and_encode_share_ingest_without_forced_copy_conversion() {
        check_real_hls_outputs(true).await;
    }

    async fn check_real_hls_outputs(native_hevc: bool) {
        let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").unwrap();
        let ffprobe = std::env::var("PLAYBACK_TEST_FFPROBE").unwrap();
        let root = tempfile::tempdir().unwrap();
        let generated = root.path().join("input.m3u8");
        let mut generation = tokio::process::Command::new(&ffmpeg);
        generation.kill_on_drop(true).args([
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
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000",
            "-map",
            "0:v",
            "-map",
            "1:a",
        ]);
        if !native_hevc {
            generation.args(["-map", "2:a"]);
        }
        if native_hevc {
            generation.args([
                "-x265-params",
                "pools=1:frame-threads=1:log-level=error:open-gop=0",
            ]);
        }
        generation
            .args([
                "-t",
                "8",
                "-c:v",
                if native_hevc { "libx265" } else { "libx264" },
                "-threads",
                "2",
                "-preset",
                "ultrafast",
                "-g",
                "50",
                "-c:a",
                "aac",
                "-f",
                "hls",
                "-hls_time",
                "2",
                "-hls_list_size",
                "0",
            ])
            .arg(&generated);
        let result = tokio::time::timeout(Duration::from_secs(20), generation.output())
            .await
            .unwrap()
            .unwrap();
        assert!(result.status.success());
        let manifest = std::fs::read_to_string(generated).unwrap();
        let parts = manifest
            .lines()
            .filter(|line| line.ends_with(".ts"))
            .map(|line| std::fs::read(root.path().join(line)).unwrap())
            .collect();
        let (source, records) = http_hls_source(parts).await;
        let engine = PlaybackManager::new(playback_engine::Config {
            ffmpeg: ffmpeg.clone().into(),
            ffprobe: ffprobe.into(),
            root: root.path().join("media"),
            max_sessions: 2,
            ttl: Duration::from_secs(120),
        });
        let credentials = Arc::new(
            Credentials::from_connection(
                rusqlite::Connection::open_in_memory().unwrap(),
                BOOTSTRAP,
            )
            .unwrap(),
        );
        let key = credentials
            .issue(
                BOOTSTRAP,
                KeySpec {
                    label: "HLS fixture".into(),
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
        let workers = Workers::new(engine, credentials, NetworkPolicy::loopback_fixture())
            .await
            .unwrap();
        let mut input = request(1920);
        input.input.url = source.url.clone();
        if native_hevc {
            input.output.video_codecs.push("hevc".into());
        }
        input.output.audio_track = Some(1);
        let first = workers
            .create(&principal, &input, "first", 0)
            .await
            .unwrap();
        let before = records.lock().unwrap().clone();
        if native_hevc {
            assert_eq!(
                workers
                    .delivery(&principal, &first, 5)
                    .unwrap()
                    .unwrap()
                    .video_mode,
                "copy"
            );
            let engine_id = workers.active.lock().unwrap()[&first.job_id].id.clone();
            let copy_playlist = root.path().join("media").join(engine_id).join("index.m3u8");
            let decoded = tokio::time::timeout(
                Duration::from_secs(10),
                tokio::process::Command::new(&ffmpeg)
                    .kill_on_drop(true)
                    .args([
                        "-v",
                        "error",
                        "-protocol_whitelist",
                        "file",
                        "-allowed_extensions",
                        "ts",
                        "-i",
                    ])
                    .arg(copy_playlist)
                    .args(["-t", "1", "-f", "null", "-"])
                    .output(),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(
                decoded.status.success(),
                "native HEVC copy output must decode"
            );
        }
        input.output.max_width = 160;
        input.output.max_height = 90;
        let selected_audio = if native_hevc { 1 } else { 2 };
        input.output.audio_track = Some(selected_audio);
        let second = workers
            .create(&principal, &input, "second", 5)
            .await
            .unwrap();
        assert_ne!(first.job_id, second.job_id);
        let delivery = workers.delivery(&principal, &second, 5).unwrap().unwrap();
        assert_eq!(delivery.selected_audio.unwrap().input_index, selected_audio);
        assert_eq!(delivery.video_mode, "encode");
        let token = delivery.url.split('/').nth(3).unwrap();
        let playlist = workers
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
        let playlist = axum::body::to_bytes(playlist.into_body(), 128 * 1024)
            .await
            .unwrap();
        let segment = std::str::from_utf8(&playlist)
            .unwrap()
            .lines()
            .find(|line| line.ends_with(".ts"))
            .unwrap();
        let body = workers
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
        let body = axum::body::to_bytes(body.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let sample = root.path().join("selected-audio.ts");
        std::fs::write(&sample, body).unwrap();
        let audio = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new(&ffmpeg)
                .kill_on_drop(true)
                .args(["-v", "error", "-i"])
                .arg(&sample)
                .args([
                    "-map", "0:a:0", "-vn", "-t", "1", "-ac", "1", "-ar", "48000", "-f", "f32le",
                    "pipe:1",
                ])
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(audio.status.success(), "selected replay audio must decode");
        let samples: Vec<_> = audio
            .stdout
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| f32::from_le_bytes(*bytes))
            .collect();
        assert!(samples.len() > 24000);
        let crossings = samples
            .windows(2)
            .filter(|pair| pair[0].is_sign_negative() != pair[1].is_sign_negative())
            .count();
        let frequency = crossings as f64 * 48000.0 / (2.0 * samples.len() as f64);
        let expected = if native_hevc {
            350.0..550.0
        } else {
            750.0..1000.0
        };
        assert!(
            expected.contains(&frequency),
            "replay must retain the selected original audio content"
        );
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let after = records.lock().unwrap().clone();
        for (path, count) in &after {
            assert_eq!(
                *count,
                before.get(path).copied().unwrap_or(1),
                "another output must not refetch a provider segment"
            );
        }
        workers
            .sessions
            .lock()
            .unwrap()
            .release(&principal, &first.id, 10)
            .unwrap();
        workers.reap(26).await.unwrap();
        let surviving_engine_id = workers.active.lock().unwrap()[&second.job_id].id.clone();
        assert!(workers.engine.input_running(&surviving_engine_id).await);
        workers
            .sessions
            .lock()
            .unwrap()
            .release(&principal, &second.id, 26)
            .unwrap();
        workers.reap(41).await.unwrap();
        assert_eq!(workers.sessions.lock().unwrap().job_count(), 0);
        workers.shutdown().await;
    }

    #[tokio::test]
    #[ignore = "requires real FFmpeg/ffprobe; isolated plain-HTTP live fixture"]
    async fn real_http_live_five_viewers_share_one_upstream_and_scopes_remain_isolated() {
        let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").unwrap();
        let ffprobe = std::env::var("PLAYBACK_TEST_FFPROBE").unwrap();
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input.ts");
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            tokio::process::Command::new(&ffmpeg)
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
                    "12",
                    "-an",
                    "-c:v",
                    "libx264",
                    "-threads",
                    "2",
                    "-preset",
                    "ultrafast",
                    "-g",
                    "25",
                    "-f",
                    "mpegts",
                ])
                .arg(&input)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(output.status.success(), "fixture generation must succeed");
        let source = http_source(std::fs::read(input).unwrap()).await;
        let engine = PlaybackManager::new(playback_engine::Config {
            ffmpeg: ffmpeg.clone().into(),
            ffprobe: ffprobe.clone().into(),
            root: root.path().join("media"),
            max_sessions: 3,
            ttl: Duration::from_secs(120),
        });
        let credentials = Arc::new(
            Credentials::from_connection(
                rusqlite::Connection::open_in_memory().unwrap(),
                BOOTSTRAP,
            )
            .unwrap(),
        );
        let spec = KeySpec {
            label: "real HTTP fixture".into(),
            namespaces: vec!["fixture".into(), "separate".into()],
            scopes: vec![Scope::Create, Scope::Read, Scope::Renew, Scope::Release],
            quotas: Quotas {
                inputs: 2,
                outputs: 3,
                viewers: 7,
            },
            expires_at: None,
        };
        let key = credentials.issue(BOOTSTRAP, spec, 0).unwrap();
        let principal = credentials.authenticate(&key.secret, 0).unwrap();
        let workers = Workers::new(engine, credentials, NetworkPolicy::loopback_fixture())
            .await
            .unwrap();
        let mut input = request(1920);
        input.input.url = source.url.clone();
        let first = workers
            .create(&principal, &input, "first", 0)
            .await
            .unwrap();
        expect_connections(&source, 1).await;
        let opened = source.requests.load(std::sync::atomic::Ordering::SeqCst);
        let mut viewers = vec![first];
        for index in 1..5 {
            input.output.max_width = if index % 2 == 0 { 1920 } else { 3840 };
            input.output.max_height = input.output.max_width / 16 * 9;
            viewers.push(
                workers
                    .create(&principal, &input, &format!("viewer-{index}"), 5)
                    .await
                    .unwrap(),
            );
        }
        assert!(viewers
            .iter()
            .all(|viewer| viewer.job_id == viewers[0].job_id));
        expect_connections(&source, 1).await;
        assert_eq!(
            source.requests.load(std::sync::atomic::Ordering::SeqCst),
            opened,
            "additional viewers must not re-open the provider stream"
        );
        let media = workers
            .delivery(&principal, &viewers[1], 5)
            .unwrap()
            .unwrap();
        let token = media.url.split('/').nth(3).unwrap();
        let playlist = workers
            .media(
                &viewers[1].id,
                token,
                "index.m3u8",
                5,
                axum::http::Method::GET,
                axum::http::HeaderMap::new(),
            )
            .await
            .unwrap();
        let playlist = axum::body::to_bytes(playlist.into_body(), 128 * 1024)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&playlist).contains(".ts"));
        // A late viewer requests a genuinely different normalized output.
        // It reads replay keyframes, not another provider stream.
        input.output.max_width = 160;
        input.output.max_height = 90;
        let resized = workers
            .create(&principal, &input, "resized", 5)
            .await
            .unwrap();
        assert_ne!(resized.job_id, viewers[0].job_id);
        let resized_delivery = workers.delivery(&principal, &resized, 5).unwrap().unwrap();
        assert_eq!(resized_delivery.video_mode, "encode");
        expect_connections(&source, 1).await;
        assert_eq!(
            source.requests.load(std::sync::atomic::Ordering::SeqCst),
            opened,
            "different output plan must not open the provider again"
        );
        // Releasing the entire first output must not stop the ingest still
        // owned by the resized output.
        for viewer in &viewers {
            workers
                .sessions
                .lock()
                .unwrap()
                .release(&principal, &viewer.id, 10)
                .unwrap();
        }
        workers.reap(26).await.unwrap();
        expect_connections(&source, 1).await;
        let token = resized_delivery.url.split('/').nth(3).unwrap();
        let playlist = workers
            .media(
                &resized.id,
                token,
                "index.m3u8",
                26,
                axum::http::Method::GET,
                axum::http::HeaderMap::new(),
            )
            .await
            .unwrap();
        let playlist = axum::body::to_bytes(playlist.into_body(), 128 * 1024)
            .await
            .unwrap();
        let text = std::str::from_utf8(&playlist).unwrap();
        let segment = text.lines().find(|line| line.ends_with(".ts")).unwrap();
        let bytes = workers
            .media(
                &resized.id,
                token,
                segment,
                26,
                axum::http::Method::GET,
                axum::http::HeaderMap::new(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(bytes.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let decoded = root.path().join("resized.ts");
        std::fs::write(&decoded, bytes).unwrap();
        let decode = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new(&ffmpeg)
                .kill_on_drop(true)
                .args(["-v", "error", "-i"])
                .arg(&decoded)
                .args(["-f", "null", "-"])
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            decode.status.success(),
            "late replay output must decode after first-output teardown"
        );
        input.namespace = "separate".into();
        let separate = workers
            .create(&principal, &input, "separate", 26)
            .await
            .unwrap();
        assert_ne!(separate.job_id, viewers[0].job_id);
        expect_connections(&source, 2).await;
        workers
            .sessions
            .lock()
            .unwrap()
            .release(&principal, &resized.id, 26)
            .unwrap();
        workers.reap(41).await.unwrap();
        expect_connections(&source, 1).await;
        workers
            .sessions
            .lock()
            .unwrap()
            .release(&principal, &separate.id, 41)
            .unwrap();
        workers.reap(56).await.unwrap();
        expect_connections(&source, 0).await;
        assert_eq!(workers.sessions.lock().unwrap().job_count(), 0);
        workers.shutdown().await;
    }

    /// Read at least `limit` more bytes of a live progressive body.
    async fn read_live(body: &mut axum::body::Body, limit: usize) -> Vec<u8> {
        use axum::body::HttpBody;
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(20), async {
            while bytes.len() < limit {
                let frame =
                    std::future::poll_fn(|cx| std::pin::Pin::new(&mut *body).poll_frame(cx))
                        .await
                        .expect("live body ended early")
                        .expect("live body failed");
                if let Ok(data) = frame.into_data() {
                    bytes.extend_from_slice(&data);
                }
            }
        })
        .await
        .expect("progressive body must keep flowing");
        bytes
    }

    #[tokio::test]
    #[ignore = "requires real FFmpeg/ffprobe; isolated plain-HTTP progressive live fixture"]
    async fn real_progressive_viewers_share_one_upstream_late_join_decodes_and_last_release_reaps()
    {
        let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").unwrap();
        let ffprobe = std::env::var("PLAYBACK_TEST_FFPROBE").unwrap();
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input.ts");
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            tokio::process::Command::new(&ffmpeg)
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
                    "12",
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
                    "-b:a",
                    "64k",
                    "-f",
                    "mpegts",
                ])
                .arg(&input)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(output.status.success(), "fixture generation must succeed");
        let source = http_source(std::fs::read(input).unwrap()).await;
        let media_root = root.path().join("media");
        let engine = PlaybackManager::new(playback_engine::Config {
            ffmpeg: ffmpeg.clone().into(),
            ffprobe: ffprobe.clone().into(),
            root: media_root.clone(),
            max_sessions: 4,
            ttl: Duration::from_secs(120),
        });
        let credentials = Arc::new(
            Credentials::from_connection(
                rusqlite::Connection::open_in_memory().unwrap(),
                BOOTSTRAP,
            )
            .unwrap(),
        );
        let spec = KeySpec {
            label: "real progressive fixture".into(),
            namespaces: vec!["fixture".into()],
            scopes: vec![Scope::Create, Scope::Read, Scope::Renew, Scope::Release],
            quotas: Quotas {
                inputs: 1,
                outputs: 3,
                viewers: 5,
            },
            expires_at: None,
        };
        let key = credentials.issue(BOOTSTRAP, spec, 0).unwrap();
        let principal = credentials.authenticate(&key.secret, 0).unwrap();
        let workers = Workers::new(engine, credentials, NetworkPolicy::loopback_fixture())
            .await
            .unwrap();
        let mut request = request(1920);
        request.input.url = source.url.clone();
        request.output.protocol = OutputProtocol::Progressive;

        // Decoders the client cannot use fail explicitly, never via HLS.
        let mut undecodable = request.clone();
        undecodable.output.video_codecs = vec!["hevc".into()];
        assert_eq!(
            workers
                .create(&principal, &undecodable, "undecodable", 0)
                .await
                .unwrap_err(),
            "unsupported_output"
        );

        let first = workers
            .create(&principal, &request, "first", 0)
            .await
            .unwrap();
        expect_connections(&source, 1).await;
        let opened = source.requests.load(std::sync::atomic::Ordering::SeqCst);
        let delivery = workers.delivery(&principal, &first, 0).unwrap().unwrap();
        assert_eq!(delivery.format, "mpegts");
        assert_eq!(delivery.mode, "remux");
        assert_eq!(
            (delivery.video_mode.as_str(), delivery.audio_mode.as_str()),
            ("copy", "copy")
        );
        assert!(delivery.url.ends_with("/stream.ts"));
        let first_token = delivery.url.split('/').nth(3).unwrap().to_owned();
        let head = workers
            .media(
                &first.id,
                &first_token,
                "stream.ts",
                0,
                axum::http::Method::HEAD,
                axum::http::HeaderMap::new(),
            )
            .await
            .unwrap();
        assert_eq!(head.headers()["content-type"], "video/mp2t");
        assert_eq!(head.headers()["accept-ranges"], "none");
        let first_body = workers
            .media(
                &first.id,
                &first_token,
                "stream.ts",
                0,
                axum::http::Method::GET,
                axum::http::HeaderMap::new(),
            )
            .await
            .unwrap();
        let mut first_body = first_body.into_body();
        let first_bytes = read_live(&mut first_body, 256 * 1024).await;
        assert_eq!(first_bytes[0], 0x47, "fresh MPEG-TS packets");
        // The first viewer keeps consuming while another viewer joins.
        let first_reader = tokio::spawn(async move {
            use axum::body::HttpBody;
            // Ends when the release tears this viewer's reader down.
            while let Some(Ok(_)) =
                std::future::poll_fn(|cx| std::pin::Pin::new(&mut first_body).poll_frame(cx)).await
            {
            }
        });

        // A late viewer gets a private reader over the same ingest.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let late = workers
            .create(&principal, &request, "late", 3)
            .await
            .unwrap();
        assert_ne!(late.job_id, first.job_id);
        expect_connections(&source, 1).await;
        assert_eq!(
            source.requests.load(std::sync::atomic::Ordering::SeqCst),
            opened,
            "a progressive late join must not re-open the provider"
        );
        let late_delivery = workers.delivery(&principal, &late, 3).unwrap().unwrap();
        let late_token = late_delivery.url.split('/').nth(3).unwrap().to_owned();
        // Viewer media authorization is independent.
        assert!(workers
            .media(
                &late.id,
                &first_token,
                "stream.ts",
                3,
                axum::http::Method::GET,
                axum::http::HeaderMap::new()
            )
            .await
            .is_err());
        let late_body = workers
            .media(
                &late.id,
                &late_token,
                "stream.ts",
                3,
                axum::http::Method::GET,
                axum::http::HeaderMap::new(),
            )
            .await
            .unwrap();
        let mut late_body = late_body.into_body();
        let late_bytes = read_live(&mut late_body, 512 * 1024).await;
        assert!(
            !first_reader.is_finished(),
            "the first viewer keeps receiving media"
        );
        let decoded = root.path().join("late.ts");
        std::fs::write(&decoded, &late_bytes).unwrap();
        // The late output starts at a keyframe with fresh headers and decodes.
        let probe = tokio::process::Command::new(&ffprobe)
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-read_intervals",
                "%+#1",
                "-show_entries",
                "packet=flags",
                "-show_entries",
                "stream=codec_name",
                "-of",
                "csv=p=0",
            ])
            .arg(&decoded)
            .output()
            .await
            .unwrap();
        let facts = String::from_utf8_lossy(&probe.stdout);
        assert!(
            facts.contains("h264"),
            "late output carries the original codec: {facts}"
        );
        assert!(
            facts.lines().any(|line| line.starts_with('K')),
            "first video packet is a keyframe: {facts}"
        );
        let decode = tokio::process::Command::new(&ffmpeg)
            .args(["-v", "error", "-xerror", "-i"])
            .arg(&decoded)
            .args(["-map", "0:v:0", "-map", "0:a:0", "-f", "null", "-"])
            .output()
            .await
            .unwrap();
        assert!(
            decode.status.success(),
            "late progressive output must decode"
        );
        // A stream is single-consumer: a second GET on one viewer is refused.
        assert!(workers
            .media(
                &late.id,
                &late_token,
                "stream.ts",
                3,
                axum::http::Method::GET,
                axum::http::HeaderMap::new()
            )
            .await
            .is_err());

        // Releasing one viewer leaves the other reader and the ingest running.
        workers
            .sessions
            .lock()
            .unwrap()
            .release(&principal, &first.id, 5)
            .unwrap();
        workers.reap(21).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), first_reader)
            .await
            .expect("a released viewer's stream ends")
            .unwrap();
        expect_connections(&source, 1).await;
        assert!(workers
            .media_target(&late.id, &late_token, 21)
            .await
            .is_ok());
        read_live(&mut late_body, 128 * 1024).await;
        drop(late_body);
        // The last release cancels the reader, then the ingest and its storage.
        workers
            .sessions
            .lock()
            .unwrap()
            .release(&principal, &late.id, 21)
            .unwrap();
        workers.reap(37).await.unwrap();
        expect_connections(&source, 0).await;
        assert_eq!(workers.sessions.lock().unwrap().job_count(), 0);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mut entries = std::fs::read_dir(&media_root).unwrap();
                if entries.next().is_none() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("replay storage is reclaimed after teardown");
        workers.shutdown().await;
    }

    pub(crate) const BOOTSTRAP: &str = "gateway-worker-fixture-bootstrap-key";
    #[tokio::test]
    #[ignore = "requires real FFmpeg/ffprobe; small software-only 4K copy fixture"]
    async fn real_shared_4k_copy_keeps_declared_dimensions_without_a_quality_clamp() {
        let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").unwrap();
        let ffprobe = std::env::var("PLAYBACK_TEST_FFPROBE").unwrap();
        let root = tempfile::tempdir().unwrap();
        let input_file = root.path().join("input-4k.ts");
        let generated = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::process::Command::new(&ffmpeg)
                .kill_on_drop(true)
                .args([
                    "-v",
                    "error",
                    "-nostdin",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=black:size=3840x2160:rate=1",
                    "-t",
                    "4",
                    "-an",
                    "-c:v",
                    "libx264",
                    "-threads",
                    "2",
                    "-preset",
                    "ultrafast",
                    "-level:v",
                    "5.1",
                    "-g",
                    "1",
                    "-bf",
                    "0",
                    "-f",
                    "mpegts",
                ])
                .arg(&input_file)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(generated.status.success());
        let source = http_source(std::fs::read(input_file).unwrap()).await;
        let engine = PlaybackManager::new(playback_engine::Config {
            ffmpeg: ffmpeg.clone().into(),
            ffprobe: ffprobe.clone().into(),
            root: root.path().join("media"),
            max_sessions: 1,
            ttl: Duration::from_secs(120),
        });
        let credentials = Arc::new(
            Credentials::from_connection(
                rusqlite::Connection::open_in_memory().unwrap(),
                BOOTSTRAP,
            )
            .unwrap(),
        );
        let key = credentials
            .issue(
                BOOTSTRAP,
                KeySpec {
                    label: "4K fixture".into(),
                    namespaces: vec!["fixture".into()],
                    scopes: vec![Scope::Create, Scope::Read, Scope::Release],
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
        let mut input = request(3840);
        input.input.url = source.url.clone();
        let first = workers
            .create(&principal, &input, "first", 0)
            .await
            .unwrap();
        expect_connections(&source, 1).await;
        let opened = source.requests.load(std::sync::atomic::Ordering::SeqCst);
        input.output.max_width = 7680;
        input.output.max_height = 4320;
        let second = workers
            .create(&principal, &input, "headroom", 5)
            .await
            .unwrap();
        assert_eq!(first.job_id, second.job_id);
        assert_eq!(
            source.requests.load(std::sync::atomic::Ordering::SeqCst),
            opened
        );
        let delivery = workers.delivery(&principal, &second, 5).unwrap().unwrap();
        assert_eq!(delivery.video_mode, "copy");
        assert_eq!(delivery.mode, "remux");
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
        let playlist = axum::body::to_bytes(response.into_body(), 128 * 1024)
            .await
            .unwrap();
        let segment = std::str::from_utf8(&playlist)
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
        let sample = root.path().join("output-4k.ts");
        std::fs::write(
            &sample,
            axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let inspected = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new(&ffprobe)
                .kill_on_drop(true)
                .args(["-v", "error", "-show_streams", "-of", "json", "-i"])
                .arg(&sample)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(inspected.status.success());
        let facts: serde_json::Value = serde_json::from_slice(&inspected.stdout).unwrap();
        let video = facts["streams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|stream| stream["codec_type"] == "video")
            .unwrap();
        assert_eq!(video["width"], 3840);
        assert_eq!(video["height"], 2160);
        let decoded = tokio::time::timeout(
            Duration::from_secs(15),
            tokio::process::Command::new(&ffmpeg)
                .kill_on_drop(true)
                .args(["-v", "error", "-threads", "2", "-i"])
                .arg(&sample)
                .args(["-frames:v", "1", "-f", "null", "-"])
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            decoded.status.success(),
            "copied 4K output must decode a frame"
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
        expect_connections(&source, 0).await;
        workers.shutdown().await;
    }
    #[tokio::test]
    #[ignore = "requires real FFmpeg/ffprobe; isolated nonshared MPEG4 source fixture"]
    async fn real_fallback_outputs_reserve_each_physical_input_and_share_identical_jobs() {
        check_live_codec_ingest(false, false, false, None).await;
    }
    #[tokio::test]
    #[ignore = "requires real FFmpeg/ffprobe; isolated shared MPEG2 live source fixture"]
    async fn real_mpeg2_live_differing_encoded_outputs_share_ingest_and_input_quota() {
        check_live_codec_ingest(true, false, false, None).await;
    }
    #[tokio::test]
    #[ignore = "requires real FFmpeg/ffprobe; isolated FFV1/FLAC private Matroska replay"]
    async fn real_matroska_av_live_outputs_share_indices_late_join_eviction_and_reap() {
        check_live_codec_ingest(true, true, false, None).await;
    }
    #[tokio::test]
    #[ignore = "requires real FFmpeg/ffprobe; H264 copy and FLAC private replay"]
    async fn real_matroska_h264_flac_copy_and_encoded_live_outputs_share_ingest() {
        check_live_codec_ingest(true, true, true, None).await;
    }
    #[tokio::test]
    #[ignore = "requires real FFmpeg/ffprobe; configured private Matroska replay watchdog"]
    async fn real_matroska_budget_failure_holds_input_until_actual_output_teardown() {
        check_live_codec_ingest(true, true, false, Some(2 * 1024 * 1024)).await;
    }
    async fn check_live_codec_ingest(
        shared: bool,
        matroska: bool,
        copy_source: bool,
        budget: Option<u64>,
    ) {
        let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").unwrap();
        let ffprobe = std::env::var("PLAYBACK_TEST_FFPROBE").unwrap();
        let root = tempfile::tempdir().unwrap();
        let input_file = root.path().join(if matroska {
            "private-source.mkv"
        } else if shared {
            "mpeg2.ts"
        } else {
            "fallback.mkv"
        });
        let generated = tokio::time::timeout(
            Duration::from_secs(20),
            tokio::process::Command::new(&ffmpeg)
                .kill_on_drop(true)
                .args([
                    "-v",
                    "error",
                    "-nostdin",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=320x180:rate=25",
                ])
                .args(if matroska {
                    &[
                        "-f",
                        "lavfi",
                        "-i",
                        "sine=frequency=880:sample_rate=48000",
                        "-f",
                        "lavfi",
                        "-i",
                        "sine=frequency=440:sample_rate=48000",
                        "-map",
                        "1:a",
                        "-map",
                        "0:v",
                        "-map",
                        "2:a",
                        "-c:a",
                        "flac",
                        "-metadata:s:a:0",
                        "language=spa",
                        "-metadata:s:a:1",
                        "language=eng",
                    ][..]
                } else {
                    &[][..]
                })
                .args([
                    "-t",
                    if matroska { "40" } else { "8" },
                    "-c:v",
                    if copy_source {
                        "libx264"
                    } else if matroska {
                        "ffv1"
                    } else if shared {
                        "mpeg2video"
                    } else {
                        "mpeg4"
                    },
                    "-threads",
                    "2",
                    "-g",
                    "25",
                    "-f",
                    if shared && !matroska {
                        "mpegts"
                    } else {
                        "matroska"
                    },
                ])
                .args(if matroska { &[][..] } else { &["-an"][..] })
                .arg(&input_file)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(generated.status.success());
        let bytes = std::fs::read(input_file).unwrap();
        let delay = if copy_source {
            Duration::from_millis((188 * 100 * 10_000u64 / bytes.len() as u64).max(1))
        } else {
            Duration::from_millis(30)
        };
        let source = http_source_with_interval(bytes, delay).await;
        let config = playback_engine::Config {
            ffmpeg: ffmpeg.clone().into(),
            ffprobe: ffprobe.clone().into(),
            root: root.path().join("media"),
            max_sessions: 3,
            ttl: Duration::from_secs(120),
        };
        let engine = match budget {
            Some(bytes) => {
                PlaybackManager::new_with_hardware_and_replay_budget(config, None, None, bytes)
            }
            None => PlaybackManager::new(config),
        };
        let credentials = Arc::new(
            Credentials::from_connection(
                rusqlite::Connection::open_in_memory().unwrap(),
                BOOTSTRAP,
            )
            .unwrap(),
        );
        let key = credentials
            .issue(
                BOOTSTRAP,
                KeySpec {
                    label: "fallback fixture".into(),
                    namespaces: vec!["fixture".into()],
                    scopes: vec![Scope::Create, Scope::Read, Scope::Release],
                    quotas: Quotas {
                        inputs: if shared { 1 } else { 2 },
                        outputs: 3,
                        viewers: 4,
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
        let mut input = request(1920);
        input.input.url = source.url.clone();
        if matroska {
            input.output.audio_track = Some(2);
        }
        let first = workers
            .create(&principal, &input, "first", 0)
            .await
            .unwrap();
        expect_connections(&source, 1).await;
        let opened = source.requests.load(std::sync::atomic::Ordering::SeqCst);
        let same = workers.create(&principal, &input, "same", 1).await.unwrap();
        assert_eq!(same.job_id, first.job_id);
        assert_eq!(
            source.requests.load(std::sync::atomic::Ordering::SeqCst),
            opened
        );
        if budget.is_some() {
            let engine_id = workers.active.lock().unwrap()[&first.job_id].id.clone();
            tokio::time::timeout(Duration::from_secs(20), async {
                while workers.engine.input_running(&engine_id).await {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .expect("configured replay watchdog must terminate the actual producer");
            expect_connections(&source, 0).await;
            assert_eq!(
                workers.available(&principal, 2).unwrap().inputs,
                0,
                "ledger must retain admission while output cleanup has not run"
            );
            workers.reap(3).await.unwrap();
            assert_eq!(workers.available(&principal, 3).unwrap().inputs, 1);
            assert!(std::fs::read_dir(root.path().join("media"))
                .unwrap()
                .next()
                .is_none());
            workers.shutdown().await;
            return;
        }
        if matroska {
            let replay = std::fs::read_dir(root.path().join("media"))
                .unwrap()
                .flatten()
                .map(|entry| entry.path())
                .find(|dir| {
                    std::fs::read_to_string(dir.join("index.m3u8"))
                        .is_ok_and(|text| text.contains(".mkv"))
                })
                .unwrap();
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    let manifest = std::fs::read_to_string(replay.join("index.m3u8")).unwrap();
                    let sequence = manifest
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:")
                                .and_then(|sequence| sequence.parse::<u64>().ok())
                        })
                        .unwrap_or(0);
                    if sequence >= 4 && !replay.join("segment-000000000.mkv").exists() {
                        break;
                    }
                    let engine_id = workers
                        .active
                        .lock()
                        .unwrap()
                        .get(&first.job_id)
                        .unwrap()
                        .id
                        .clone();
                    assert!(workers.engine.input_running(&engine_id).await);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .expect("private replay must evict old segments before late join");
            let count = std::fs::read_dir(&replay)
                .unwrap()
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".mkv"))
                .count();
            assert!(
                count <= 16,
                "playlist, three delete-grace segments and one open file are bounded"
            );
            let manifest = std::fs::read_to_string(replay.join("index.m3u8")).unwrap();
            let completed = manifest
                .lines()
                .find(|line| line.ends_with(".mkv"))
                .unwrap();
            let inspected = tokio::time::timeout(
                Duration::from_secs(10),
                tokio::process::Command::new(&ffprobe)
                    .kill_on_drop(true)
                    .args([
                        "-v",
                        "error",
                        "-show_streams",
                        "-show_packets",
                        "-show_entries",
                        "stream=index,codec_name,codec_type:packet=stream_index,pts_time,dts_time",
                        "-of",
                        "json",
                        "-i",
                    ])
                    .arg(replay.join(completed))
                    .output(),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(inspected.status.success());
            let facts: serde_json::Value = serde_json::from_slice(&inspected.stdout).unwrap();
            assert_eq!(
                facts["streams"][0]["codec_name"],
                if copy_source { "h264" } else { "ffv1" }
            );
            assert_eq!(facts["streams"][1]["codec_name"], "flac");
            assert_eq!(facts["streams"][2]["codec_name"], "flac");
            let mut last = [None; 3];
            for packet in facts["packets"].as_array().unwrap() {
                let stream = packet["stream_index"].as_u64().unwrap() as usize;
                let pts = packet["pts_time"].as_str().unwrap().parse::<f64>().unwrap();
                assert!(
                    pts >= 7.9,
                    "private replay must preserve the evicted window's absolute media clock"
                );
                if let Some(dts) = packet["dts_time"]
                    .as_str()
                    .and_then(|dts| dts.parse::<f64>().ok())
                {
                    if let Some(previous) = last[stream] {
                        assert!(
                            dts >= previous,
                            "packet DTS must be monotonic per retained track"
                        );
                    }
                    last[stream] = Some(dts);
                }
            }
            assert!(last.iter().all(Option::is_some));
            assert_eq!(
                source.requests.load(std::sync::atomic::Ordering::SeqCst),
                opened
            );
        }
        input.output.max_width = 160;
        input.output.max_height = 90;
        let second = workers
            .create(&principal, &input, "second", 1)
            .await
            .unwrap();
        assert_ne!(first.job_id, second.job_id);
        expect_connections(&source, if shared { 1 } else { 2 }).await;
        if shared {
            assert_eq!(
                source.requests.load(std::sync::atomic::Ordering::SeqCst),
                opened,
                "MPEG2 differing output plans must use the already running ingest"
            );
        }
        assert_eq!(workers.available(&principal, 1).unwrap().inputs, 0);
        if shared {
            for (viewer, width, height) in [(&first, 320, 180), (&second, 160, 90)] {
                let delivery = workers.delivery(&principal, viewer, 2).unwrap().unwrap();
                assert_eq!(
                    delivery.video_mode,
                    if copy_source && width == 320 {
                        "copy"
                    } else {
                        "encode"
                    }
                );
                if matroska {
                    assert_eq!(delivery.selected_audio.as_ref().unwrap().input_index, 2);
                }
                let token = delivery.url.split('/').nth(3).unwrap();
                let playlist = workers
                    .media(
                        &viewer.id,
                        token,
                        "index.m3u8",
                        2,
                        axum::http::Method::GET,
                        axum::http::HeaderMap::new(),
                    )
                    .await
                    .unwrap();
                let playlist = axum::body::to_bytes(playlist.into_body(), 128 * 1024)
                    .await
                    .unwrap();
                let filename = std::str::from_utf8(&playlist)
                    .unwrap()
                    .lines()
                    .find(|line| line.ends_with(".ts"))
                    .unwrap();
                assert!(
                    !std::str::from_utf8(&playlist).unwrap().contains(".mkv"),
                    "private Matroska is never a public HLS format"
                );
                let segment = workers
                    .media(
                        &viewer.id,
                        token,
                        filename,
                        2,
                        axum::http::Method::GET,
                        axum::http::HeaderMap::new(),
                    )
                    .await
                    .unwrap();
                let segment = axum::body::to_bytes(segment.into_body(), 16 * 1024 * 1024)
                    .await
                    .unwrap();
                let sample = root.path().join(format!("decoded-{width}.ts"));
                std::fs::write(&sample, segment).unwrap();
                let inspected = tokio::time::timeout(
                    Duration::from_secs(10),
                    tokio::process::Command::new(&ffprobe)
                        .kill_on_drop(true)
                        .args(["-v", "error", "-show_streams", "-of", "json", "-i"])
                        .arg(&sample)
                        .output(),
                )
                .await
                .unwrap()
                .unwrap();
                assert!(inspected.status.success());
                let facts: serde_json::Value = serde_json::from_slice(&inspected.stdout).unwrap();
                assert_eq!(facts["streams"][0]["codec_name"], "h264");
                assert_eq!(facts["streams"][0]["width"], width);
                assert_eq!(facts["streams"][0]["height"], height);
                if matroska {
                    assert_eq!(facts["streams"][1]["codec_name"], "aac");
                    let start = |index: usize| {
                        facts["streams"][index]["start_time"]
                            .as_str()
                            .unwrap()
                            .parse::<f64>()
                            .unwrap()
                    };
                    assert!(
                        (start(0) - start(1)).abs() < 0.1,
                        "replayed audio/video must retain their timestamp alignment"
                    );
                    let decoded = tokio::time::timeout(
                        Duration::from_secs(10),
                        tokio::process::Command::new(&ffmpeg)
                            .kill_on_drop(true)
                            .args(["-v", "error", "-threads", "2", "-i"])
                            .arg(&sample)
                            .args([
                                "-map", "0:a:0", "-vn", "-t", "1", "-ac", "1", "-ar", "48000",
                                "-f", "f32le", "pipe:1",
                            ])
                            .output(),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                    assert!(decoded.status.success());
                    let samples: Vec<f32> = decoded
                        .stdout
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|bytes| f32::from_le_bytes(*bytes))
                        .collect();
                    assert!(samples.len() > 24000);
                    let crossings = samples
                        .windows(2)
                        .filter(|pair| pair[0].is_sign_negative() != pair[1].is_sign_negative())
                        .count();
                    let frequency = crossings as f64 * 48000.0 / (2.0 * samples.len() as f64);
                    assert!(
                        (350.0..550.0).contains(&frequency),
                        "original audio2 must remain the selected440Hz track, not audio0/880Hz"
                    );
                }
                let decoded = tokio::time::timeout(
                    Duration::from_secs(10),
                    tokio::process::Command::new(&ffmpeg)
                        .kill_on_drop(true)
                        .args(["-v", "error", "-threads", "2", "-i"])
                        .arg(&sample)
                        .args(["-frames:v", "1", "-f", "null", "-"])
                        .output(),
                )
                .await
                .unwrap()
                .unwrap();
                assert!(
                    decoded.status.success(),
                    "each differing MPEG2-derived output must decode"
                );
            }
        }
        if !shared {
            input.output.max_width = 80;
            input.output.max_height = 44;
            assert_eq!(
                workers
                    .create(&principal, &input, "third", 1)
                    .await
                    .unwrap_err(),
                "input_capacity"
            );
        }
        for viewer in [first, same] {
            workers
                .sessions
                .lock()
                .unwrap()
                .release(&principal, &viewer.id, 10)
                .unwrap();
        }
        workers.reap(26).await.unwrap();
        expect_connections(&source, 1).await;
        assert_eq!(
            workers.available(&principal, 26).unwrap().inputs,
            if shared { 0 } else { 1 }
        );
        workers
            .sessions
            .lock()
            .unwrap()
            .release(&principal, &second.id, 26)
            .unwrap();
        workers.reap(41).await.unwrap();
        expect_connections(&source, 0).await;
        assert_eq!(
            workers.available(&principal, 41).unwrap().inputs,
            if shared { 1 } else { 2 }
        );
        workers.shutdown().await;
    }
    pub(crate) async fn fixture(stall: bool) -> (tempfile::TempDir, Arc<Workers>, Principal) {
        fixture_with_outputs(stall, 1).await
    }
    async fn fixture_with_outputs(
        stall: bool,
        outputs: usize,
    ) -> (tempfile::TempDir, Arc<Workers>, Principal) {
        let root = tempfile::tempdir().unwrap();
        let probe = root.path().join("probe");
        std::fs::write(&probe, r#"#!/bin/sh
printf '%s' '{"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":640,"height":360,"pix_fmt":"yuv420p","profile":"High","level":31,"avg_frame_rate":"30/1","r_frame_rate":"30/1","field_order":"progressive"},{"index":1,"codec_type":"audio","codec_name":"aac","profile":"LC","channels":2}],"format":{"format_name":"mpegts"}}'
"#).unwrap();
        let encoder = root.path().join("encoder");
        let script = if stall {
            "#!/bin/sh\nprintf x >> \"$0.count\"\nexec sleep 30\n"
        } else {
            "#!/bin/sh\nprintf x >> \"$0.count\"\nprintf '%s\\n' \"$$\" >> \"$0.pids\"\nfor arg do case \"$arg\" in *independent_segments*) printf '%s' \"$$\" > \"$0.ingest-pid\" ;; esac; done\nfor last do :; done\ndir=${last%/*}\nprintf 'fixture' > \"$dir/segment-000000000.ts\"\nprintf '#EXTM3U\\n#EXTINF:1,\\nsegment-000000000.ts\\n' > \"$last\"\nexec sleep 30\n"
        };
        std::fs::write(&encoder, script).unwrap();
        for file in [&probe, &encoder] {
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let engine = PlaybackManager::new(playback_engine::Config {
            ffmpeg: encoder,
            ffprobe: probe,
            root: root.path().join("media"),
            max_sessions: outputs,
            ttl: Duration::from_secs(60),
        });
        let credentials = Arc::new(
            Credentials::from_connection(
                rusqlite::Connection::open_in_memory().unwrap(),
                BOOTSTRAP,
            )
            .unwrap(),
        );
        let key = credentials
            .issue(
                BOOTSTRAP,
                KeySpec {
                    label: "fixture".into(),
                    namespaces: vec!["fixture".into()],
                    scopes: vec![Scope::Create, Scope::Read, Scope::Renew, Scope::Release],
                    quotas: Quotas {
                        inputs: 1,
                        outputs: outputs as u32,
                        viewers: 5,
                    },
                    expires_at: None,
                },
                0,
            )
            .unwrap();
        let principal = credentials.authenticate(&key.secret, 0).unwrap();
        (
            root,
            Arc::new(
                Workers::new(engine, credentials, NetworkPolicy::default())
                    .await
                    .unwrap(),
            ),
            principal,
        )
    }
    pub(crate) fn request(width: u32) -> CreateSession {
        CreateSession {
            namespace: "fixture".into(),
            input: Input {
                url: "https://fixture.invalid/source".into(),
                headers: BTreeMap::new(),
                live: true,
            },
            output: Output {
                conversion: crate::protocol::Conversion::Auto,
                audio_language: None,
                preferred_audio_language: None,
                preferred_subtitle_language: None,
                subtitles_off: false,
                protocol: OutputProtocol::Hls,
                video_codecs: vec!["h264".into()],
                audio_codecs: vec!["aac".into()],
                max_width: width,
                max_height: width / 16 * 9,
                audio_track: None,
                subtitle_track: None,
            },
            position_seconds: 0.0,
        }
    }

    #[tokio::test]
    async fn live_matroska_progressive_is_refused_before_any_media_job() {
        let (root, workers, p) = fixture(false).await;
        std::fs::write(root.path().join("probe"), r#"#!/bin/sh
printf '%s' '{"streams":[{"index":0,"codec_type":"video","codec_name":"ffv1","width":640,"height":360,"pix_fmt":"yuv420p","avg_frame_rate":"25/1","r_frame_rate":"25/1","field_order":"progressive"},{"index":1,"codec_type":"audio","codec_name":"flac","channels":2}],"format":{"format_name":"matroska,webm"}}'
"#).unwrap();
        let mut matroska = request(1920);
        matroska.output.protocol = OutputProtocol::Progressive;
        matroska.output.video_codecs = vec!["ffv1".into()];
        matroska.output.audio_codecs = vec!["flac".into()];
        assert_eq!(
            workers.create(&p, &matroska, "mkv", 0).await.unwrap_err(),
            "unsupported_output"
        );
        assert!(
            !root.path().join("encoder.count").exists(),
            "refused at request time: no ingest or reader is started"
        );
        assert_eq!(workers.engine.active_count().await, 0);
        assert_eq!(workers.sessions.lock().unwrap().job_count(), 0);
        workers.engine.shutdown().await;
    }

    #[tokio::test]
    async fn five_viewers_use_one_worker_and_media_is_independently_authorized() {
        let (root, workers, p) = fixture(false).await;
        let mut viewers = Vec::new();
        for index in 0..5 {
            let viewer = workers
                .create(
                    &p,
                    &request(if index % 2 == 0 { 1920 } else { 3840 }),
                    &format!("v{index}"),
                    0,
                )
                .await
                .unwrap();
            assert_eq!(viewer.status, Status::Ready);
            viewers.push(viewer);
        }
        assert!(viewers
            .iter()
            .all(|viewer| viewer.job_id == viewers[0].job_id));
        assert_eq!(workers.engine.active_count().await, 1);
        assert_eq!(
            std::fs::read_to_string(root.path().join("encoder.count")).unwrap(),
            "xx",
            "one ingest and one output, regardless of viewer count"
        );
        let token = workers
            .sessions
            .lock()
            .unwrap()
            .issue_media(&p, &viewers[1].id, 1)
            .unwrap();
        let response = workers
            .media(
                &viewers[1].id,
                &token.token,
                "index.m3u8",
                1,
                axum::http::Method::GET,
                axum::http::HeaderMap::new(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let playlist = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(playlist.contains("segment-000000000.ts"));
        assert!(!playlist.contains("/media/") && !playlist.contains("fixture.invalid"));
        assert!(workers
            .media_target(&viewers[0].id, &token.token, 1)
            .await
            .is_err());
        workers
            .sessions
            .lock()
            .unwrap()
            .release(&p, &viewers[0].id, 2)
            .unwrap();
        workers.reap(18).await.unwrap();
        assert!(workers
            .media_target(&viewers[1].id, &token.token, 18)
            .await
            .is_ok());
        workers.credentials.revoke(BOOTSTRAP, &p.key.id).unwrap();
        assert!(workers
            .media_target(&viewers[1].id, &token.token, 18)
            .await
            .is_err());
        workers.reap(18).await.unwrap();
        workers.reap(33).await.unwrap();
        assert_eq!(workers.engine.active_count().await, 0);
        assert_eq!(workers.sessions.lock().unwrap().job_count(), 0);
        workers.engine.shutdown().await;
    }

    #[tokio::test]
    async fn shared_ingest_crash_fails_each_output_and_settles_its_reservations() {
        let (root, workers, principal) = fixture_with_outputs(false, 2).await;
        let first = workers
            .create(&principal, &request(1920), "first", 0)
            .await
            .unwrap();
        let second = workers
            .create(&principal, &request(320), "second", 0)
            .await
            .unwrap();
        assert_ne!(first.job_id, second.job_id);
        assert_eq!(
            std::fs::read_to_string(root.path().join("encoder.count")).unwrap(),
            "xxx"
        );
        let pid = std::fs::read_to_string(root.path().join("encoder.ingest-pid")).unwrap();
        assert!(pid.bytes().all(|byte| byte.is_ascii_digit()));
        assert!(tokio::process::Command::new("/bin/kill")
            .args(["-KILL", &pid])
            .status()
            .await
            .unwrap()
            .success());
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                workers.reap(1).await.unwrap();
                if workers.sessions.lock().unwrap().job_count() == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        for viewer in [first, second] {
            assert_eq!(
                workers
                    .sessions
                    .lock()
                    .unwrap()
                    .get(&principal, &viewer.id, 1)
                    .unwrap()
                    .status,
                Status::Failed
            );
        }
        assert_eq!(workers.engine.active_count().await, 0);
        assert_eq!(workers.available(&principal, 1).unwrap().inputs, 1);
        assert_eq!(workers.available(&principal, 1).unwrap().outputs, 2);
        assert_eq!(
            std::fs::read_dir(root.path().join("media"))
                .unwrap()
                .count(),
            0
        );
        workers.shutdown().await;
    }

    #[tokio::test]
    async fn failed_output_releases_its_slot_without_stopping_shared_ingest() {
        let (root, workers, principal) = fixture_with_outputs(false, 2).await;
        let first = workers
            .create(&principal, &request(1920), "first", 0)
            .await
            .unwrap();
        let second = workers
            .create(&principal, &request(320), "second", 0)
            .await
            .unwrap();
        let pids = std::fs::read_to_string(root.path().join("encoder.pids")).unwrap();
        let output_pid = pids.lines().nth(1).unwrap();
        assert!(output_pid.bytes().all(|byte| byte.is_ascii_digit()));
        assert!(tokio::process::Command::new("/bin/kill")
            .args(["-KILL", output_pid])
            .status()
            .await
            .unwrap()
            .success());
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                workers.reap(1).await.unwrap();
                if workers.sessions.lock().unwrap().job_count() == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            workers
                .sessions
                .lock()
                .unwrap()
                .get(&principal, &first.id, 1)
                .unwrap()
                .status,
            Status::Failed
        );
        assert_eq!(
            workers
                .sessions
                .lock()
                .unwrap()
                .get(&principal, &second.id, 1)
                .unwrap()
                .status,
            Status::Ready
        );
        assert_eq!(workers.available(&principal, 1).unwrap().inputs, 0);
        assert_eq!(workers.available(&principal, 1).unwrap().outputs, 1);
        workers
            .sessions
            .lock()
            .unwrap()
            .release(&principal, &second.id, 1)
            .unwrap();
        workers.reap(16).await.unwrap();
        assert_eq!(
            std::fs::read_dir(root.path().join("media"))
                .unwrap()
                .count(),
            0
        );
        workers.shutdown().await;
    }

    #[tokio::test]
    async fn cancelling_start_reaps_the_worker_before_releasing_quota() {
        let (root, workers, p) = fixture(true).await;
        let task = {
            let workers = workers.clone();
            tokio::spawn(async move { workers.create(&p, &request(1920), "cancel", 0).await })
        };
        tokio::time::timeout(Duration::from_secs(3), async {
            while !root.path().join("encoder.count").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(workers.sessions.lock().unwrap().job_count(), 1);
        workers.reap(1).await.unwrap();
        assert_eq!(workers.sessions.lock().unwrap().job_count(), 0);
        assert_eq!(workers.engine.active_count().await, 0);
        assert_eq!(
            std::fs::read_dir(root.path().join("media"))
                .unwrap()
                .count(),
            0
        );
        workers.engine.shutdown().await;
    }
}
