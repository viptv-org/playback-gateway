use super::*;

pub struct PlaybackManager {
    pub(super) config: Config,
    pub(super) slots: Arc<Semaphore>,
    pub(super) sessions: Mutex<HashMap<String, Session>>,
    pub(super) probe_cache: Mutex<HashMap<[u8; 32], ProbeCacheEntry>>,
    pub(super) probe_flights: Mutex<HashMap<[u8; 32], std::sync::Weak<Mutex<()>>>>,
    pub(super) lifecycle: RwLock<()>,
    pub(super) initialized: OnceCell<()>,
    pub(super) filters: OnceCell<HashSet<String>>,
    pub(super) qsv_device: Option<PathBuf>,
    pub(super) qsv_ready: OnceCell<bool>,
    pub(super) vaapi_device: Option<PathBuf>,
    pub(super) vaapi_ready: OnceCell<hardware::Vaapi>,
    pub(super) cleanup_tasks: CleanupTasks,
}
pub struct SampleLimits {
    pub seconds: u64,
    pub startup_seconds: u64,
    pub budget_seconds: u64,
    pub max_bytes: usize,
}

impl PlaybackManager {
    pub fn new(config: Config) -> Arc<Self> {
        Self::new_with_qsv(config, None)
    }

    pub fn new_with_qsv(config: Config, qsv_device: Option<PathBuf>) -> Arc<Self> {
        Self::new_with_hardware(config, qsv_device, None)
    }

    /// `vaapi_device` enables VAAPI transcoding (any vendor). A Quick Sync
    /// device alone is also checked for VAAPI HDR tone mapping, since it is
    /// the same render node.
    pub fn new_with_hardware(
        config: Config,
        qsv_device: Option<PathBuf>,
        vaapi_device: Option<PathBuf>,
    ) -> Arc<Self> {
        let interval = config
            .ttl
            .min(Duration::from_secs(1))
            .max(Duration::from_millis(100));
        let manager = Arc::new(Self {
            slots: Arc::new(Semaphore::new(config.max_sessions)),
            config,
            sessions: Mutex::new(HashMap::new()),
            probe_cache: Mutex::new(HashMap::new()),
            probe_flights: Mutex::new(HashMap::new()),
            lifecycle: RwLock::new(()),
            initialized: OnceCell::new(),
            filters: OnceCell::new(),
            qsv_device,
            qsv_ready: OnceCell::new(),
            vaapi_device,
            vaapi_ready: OnceCell::new(),
            cleanup_tasks: Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let weak = Arc::downgrade(&manager);
        tokio::spawn(async move {
            if let Some(manager) = weak.upgrade() {
                let _lifecycle = manager.lifecycle.read().await;
                let _ = manager.accel().await;
                if manager.initialize().await.is_err() {
                    tracing::warn!("Playback startup cleanup could not finish");
                }
            }
            loop {
                sleep(interval).await;
                let Some(manager) = weak.upgrade() else { break };
                manager.reap().await;
            }
        });
        // Separate from the reaper: how far an encoder overshoots its pause
        // point is bounded by how often it is checked, not by the session TTL.
        let weak = Arc::downgrade(&manager);
        tokio::spawn(async move {
            loop {
                sleep(HLS_THROTTLE_TICK).await;
                let Some(manager) = weak.upgrade() else { break };
                manager.throttle().await;
            }
        });
        manager
    }

    /// Suspend on-demand encoders that ran too far ahead of their viewers and
    /// resume them as the viewers catch up.
    pub(super) async fn throttle(&self) {
        let _lifecycle = self.lifecycle.read().await;
        let mut sessions = self.sessions.lock().await;
        for session in sessions.values_mut() {
            if !session.throttle.enabled {
                continue;
            }
            let Some(child) = session.child.as_mut() else {
                continue;
            };
            if !matches!(child.try_wait(), Ok(None)) {
                continue;
            }
            if let Some(stream) = &session.stream {
                let (produced, consumed) = stream.times();
                session.throttle.apply_ahead(child, produced - consumed);
                continue;
            }
            if let Ok(playlist) = tokio::fs::read_to_string(session.dir.join("index.m3u8")).await {
                session.throttle.apply_playlist(child, &playlist);
            }
        }
    }

    pub(super) async fn initialize(&self) -> Result<(), String> {
        self.initialized
            .get_or_try_init(|| cleanup_orphans(&self.config.root))
            .await
            .map(|_| ())
    }

    pub(super) async fn qsv_available(&self) -> bool {
        *self
            .qsv_ready
            .get_or_init(|| async {
                let Some(device) = &self.qsv_device else {
                    return false;
                };
                let ready = hardware::usable(&self.config.ffmpeg, device).await;
                tracing::info!(ready, "Intel Quick Sync startup check");
                ready
            })
            .await
    }
    /// The device the VAAPI pipelines open: the explicit VAAPI device, else the
    /// Quick Sync render node (checked for VPP tone mapping only).
    pub(super) fn vaapi_path(&self) -> Option<&PathBuf> {
        self.vaapi_device.as_ref().or(self.qsv_device.as_ref())
    }

    pub(super) async fn vaapi_available(&self) -> hardware::Vaapi {
        *self
            .vaapi_ready
            .get_or_init(|| async {
                let Some(device) = self.vaapi_path() else {
                    return hardware::Vaapi::default();
                };
                let ready =
                    hardware::vaapi(&self.config.ffmpeg, device, self.vaapi_device.is_some()).await;
                tracing::info!(
                    encode = ready.encode,
                    tonemap = ready.tonemap,
                    placebo = ready.placebo,
                    "VAAPI startup check"
                );
                ready
            })
            .await
    }

    pub(super) async fn accel(&self) -> hardware::Accel {
        hardware::Accel {
            qsv: self.qsv_available().await,
            vaapi: self.vaapi_available().await,
        }
    }

    pub fn acceleration_status(&self) -> &'static str {
        if self.qsv_device.is_none() && self.vaapi_device.is_none() {
            return "software";
        }
        match (self.qsv_ready.get(), self.vaapi_ready.get()) {
            (Some(true), _) => "qsv",
            (_, Some(vaapi)) if vaapi.encode => "vaapi",
            (None, _) if self.qsv_device.is_some() => "checking",
            (_, None) if self.vaapi_device.is_some() => "checking",
            _ => "software_fallback",
        }
    }

    /// Where HDR10/HLG sources are tone mapped: the Intel VPP, Vulkan
    /// (libplacebo), or the CPU.
    pub fn tone_mapping_status(&self) -> &'static str {
        if self.vaapi_path().is_none() {
            return "software";
        }
        match self.vaapi_ready.get() {
            None => "checking",
            Some(vaapi) if vaapi.tonemap => "vaapi",
            Some(vaapi) if vaapi.placebo => "libplacebo",
            Some(_) => "software",
        }
    }

    pub async fn start(
        &self,
        url: String,
        headers: HashMap<String, String>,
        position: f64,
        capabilities: Option<Capabilities>,
        force: bool,
    ) -> Result<PlaybackResponse, String> {
        self.start_with_kind(url, headers, position, capabilities, force, false)
            .await
    }

    pub async fn start_with_kind(
        &self,
        url: String,
        headers: HashMap<String, String>,
        position: f64,
        capabilities: Option<Capabilities>,
        force: bool,
        live: bool,
    ) -> Result<PlaybackResponse, String> {
        self.start_with_permit(url, headers, position, capabilities, force, live, None)
            .await
    }

    /// The caller acquires provider capacity before entry. Keep it across probing,
    /// startup and the complete session lifetime; all error paths release it.
    #[allow(clippy::too_many_arguments)] // Preserve the public playback shim and explicit permit ownership.
    pub async fn start_with_permit(
        &self,
        url: String,
        headers: HashMap<String, String>,
        position: f64,
        capabilities: Option<Capabilities>,
        force: bool,
        live: bool,
        provider_permit: Option<OwnedSemaphorePermit>,
    ) -> Result<PlaybackResponse, String> {
        self.start_with_selection(
            url,
            headers,
            position,
            capabilities,
            force,
            live,
            provider_permit,
            TrackSelection {
                ..Default::default()
            },
        )
        .await
    }

    pub(super) async fn reap(&self) {
        let _lifecycle = self.lifecycle.read().await;
        let expired = {
            let mut sessions = self.sessions.lock().await;
            let mut ids = Vec::new();
            for (id, session) in sessions.iter_mut() {
                let (running, failed) = match session.child.as_mut().map(Child::try_wait) {
                    Some(Ok(Some(status))) => {
                        if let Some(stream) = &session.stream {
                            stream.complete(status.success());
                        }
                        if !status.success() {
                            // Never log raw stderr, arguments, URLs, or provider headers.
                            tracing::warn!(exit_code = ?status.code(), "Playback engine failed");
                        }
                        (false, !status.success())
                    }
                    Some(Ok(None)) => (true, false),
                    Some(Err(_)) => (false, true),
                    None => (false, false),
                };
                // A suspended encoder is idle by design, not stalled.
                let require_progress =
                    running && !session.supervised_live && !session.throttle.holding();
                if session.touched.elapsed() >= self.config.ttl
                    || failed
                    || (session.direct.is_none()
                        // A transport-less direct-URL session has no directory
                        // and no encoder: only its TTL retires it, and every
                        // heartbeat renews that TTL.
                        && !session.dir.as_os_str().is_empty()
                        && !cache_safe(&session.dir, require_progress).await)
                {
                    ids.push(id.clone());
                }
            }
            ids.into_iter()
                .filter_map(|id| sessions.remove(&id))
                .collect::<Vec<_>>()
        };
        for session in expired {
            session.cleanup().await;
        }
    }

    pub(super) async fn available_filters(&self) -> Result<&HashSet<String>, String> {
        self.filters
            .get_or_try_init(|| async {
                let mut child = Command::new(&self.config.ffmpeg)
                    .args(["-hide_banner", "-filters"])
                    .kill_on_drop(true)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|_| "Playback filter inspection unavailable".to_owned())?;
                let stdout = child.stdout.take().unwrap();
                let mut bytes = Vec::new();
                let result = timeout(Duration::from_secs(3), async {
                    stdout
                        .take(1024 * 1024 + 1)
                        .read_to_end(&mut bytes)
                        .await
                        .ok()?;
                    if bytes.len() > 1024 * 1024 {
                        return None;
                    }
                    child.wait().await.ok().filter(|s| s.success())
                })
                .await;
                if !matches!(result, Ok(Some(_))) {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    return Err("Playback filter inspection unavailable".to_owned());
                }
                Ok(String::from_utf8_lossy(&bytes)
                    .lines()
                    .filter_map(|line| {
                        let parts: Vec<_> = line.split_whitespace().collect();
                        (parts.len() >= 3 && parts[2].contains("->")).then(|| parts[1].to_owned())
                    })
                    .collect())
            })
            .await
    }
}
