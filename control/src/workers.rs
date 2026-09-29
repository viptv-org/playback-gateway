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
            self.engine.prepare_with_selection(
                request.input.url.clone(),
                headers,
                request.position_seconds,
                Some(caps),
                request.output.conversion != crate::protocol::Conversion::Auto,
                request.input.live,
                None,
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
            ),
        )
        .await
        .map_err(|_| "startup_timeout")?
        .map_err(|_| "source_preparation_failed")?;
        let plan = prepared.plan.as_ref().ok_or("unsupported_output")?;
        if (plan.format == "hls") == progressive || !plan.shareable {
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
                                    tokio::time::sleep(Duration::from_millis(30)).await;
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
            ffmpeg: ffmpeg.into(),
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
        let spec = KeySpec {
            label: "real HTTP fixture".into(),
            namespaces: vec!["fixture".into(), "separate".into()],
            scopes: vec![Scope::Create, Scope::Read, Scope::Renew, Scope::Release],
            quotas: Quotas {
                inputs: 2,
                outputs: 2,
                viewers: 6,
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
        input.namespace = "separate".into();
        let separate = workers
            .create(&principal, &input, "separate", 5)
            .await
            .unwrap();
        assert_ne!(separate.job_id, viewers[0].job_id);
        expect_connections(&source, 2).await;
        workers
            .sessions
            .lock()
            .unwrap()
            .release(&principal, &viewers[0].id, 10)
            .unwrap();
        workers.reap(26).await.unwrap();
        expect_connections(&source, 2).await;
        for viewer in &viewers[1..] {
            workers
                .sessions
                .lock()
                .unwrap()
                .release(&principal, &viewer.id, 26)
                .unwrap();
        }
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

    pub(crate) const BOOTSTRAP: &str = "gateway-worker-fixture-bootstrap-key";
    pub(crate) async fn fixture(stall: bool) -> (tempfile::TempDir, Arc<Workers>, Principal) {
        let root = tempfile::tempdir().unwrap();
        let probe = root.path().join("probe");
        std::fs::write(&probe, r#"#!/bin/sh
printf '%s' '{"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":640,"height":360,"pix_fmt":"yuv420p","profile":"High","level":31,"avg_frame_rate":"30/1","r_frame_rate":"30/1","field_order":"progressive"},{"index":1,"codec_type":"audio","codec_name":"aac","profile":"LC","channels":2}],"format":{"format_name":"mpegts"}}'
"#).unwrap();
        let encoder = root.path().join("encoder");
        let script = if stall {
            "#!/bin/sh\nprintf x >> \"$0.count\"\nexec sleep 30\n"
        } else {
            "#!/bin/sh\nprintf x >> \"$0.count\"\nfor last do :; done\ndir=${last%/*}\nprintf 'fixture' > \"$dir/segment-000000000.ts\"\nprintf '#EXTM3U\\n#EXTINF:1,\\nsegment-000000000.ts\\n' > \"$last\"\n"
        };
        std::fs::write(&encoder, script).unwrap();
        for file in [&probe, &encoder] {
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let engine = PlaybackManager::new(playback_engine::Config {
            ffmpeg: encoder,
            ffprobe: probe,
            root: root.path().join("media"),
            max_sessions: 1,
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
                        outputs: 1,
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
            "x"
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
