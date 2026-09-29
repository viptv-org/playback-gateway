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
    pub async fn new(
        engine: Arc<PlaybackManager>,
        credentials: Arc<Credentials>,
        policy: NetworkPolicy,
    ) -> Result<Self, &'static str> {
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
        self.require_active(principal, now)?;
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
                false,
                request.input.live,
                None,
                TrackSelection {
                    audio_track_index: request.output.audio_track,
                    subtitle_track_index: request.output.subtitle_track,
                    ..TrackSelection::default()
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
        self.require_active(principal, timestamp)?;
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
        if self.require_active(principal, timestamp).is_err() {
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

    fn require_active(&self, principal: &Principal, now: u64) -> Result<(), &'static str> {
        let now = i64::try_from(now).map_err(|_| "unauthorized")?;
        if self.credentials.active(&principal.key.id, now)? {
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
            if !self
                .credentials
                .active(&owner, i64::try_from(now).map_err(|_| "clock_unavailable")?)?
            {
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
    pub(crate) fn media_target(
        &self,
        viewer: &str,
        token: &str,
        now: u64,
    ) -> Result<PlaybackResponse, &'static str> {
        let job = self
            .sessions
            .lock()
            .map_err(|_| "media_unauthorized")?
            .authorize_media(&self.credentials, viewer, token, now)?;
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
        let target = self.media_target(viewer, token, now)?;
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
mod tests {
    use super::*;
    use crate::{
        credentials::{KeySpec, Quotas},
        protocol::{Input, Output},
    };
    use std::os::unix::fs::PermissionsExt;

    const BOOTSTRAP: &str = "gateway-worker-fixture-bootstrap-key";
    async fn fixture(stall: bool) -> (tempfile::TempDir, Arc<Workers>, Principal) {
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
    fn request(width: u32) -> CreateSession {
        CreateSession {
            namespace: "fixture".into(),
            input: Input {
                url: "https://fixture.invalid/source".into(),
                headers: BTreeMap::new(),
                live: true,
            },
            output: Output {
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
            .is_ok());
        workers.credentials.revoke(BOOTSTRAP, &p.key.id).unwrap();
        assert!(workers
            .media_target(&viewers[1].id, &token.token, 18)
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
