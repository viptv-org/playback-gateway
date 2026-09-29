use super::*;

// Every process reading an input shares these reservations. Cancellation may
// detach cleanup, but admission stays closed until the final child is reaped.
pub(super) struct InputPermits {
    pub(super) _playback: std::sync::Mutex<OwnedSemaphorePermit>,
    pub(super) _provider: Option<OwnedSemaphorePermit>,
}
impl InputPermits {
    /// Swap bounded inspection admission for output admission only when an
    /// actual worker will start. Shared viewers can inspect cached facts even
    /// when every output slot is occupied, then drop their unused preparation.
    pub(super) fn admit_output(&self, slots: &Arc<Semaphore>) -> Result<(), String> {
        let permit = slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| MSG_PLAYBACK_CAPACITY.to_owned())?;
        *self
            ._playback
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = permit;
        Ok(())
    }
}

pub(super) struct Session {
    pub(super) stream: Option<Arc<fmp4::Stream>>,
    // Keep inspected VOD facts alive while this input is in use.
    pub(super) _source_probe: Option<Arc<Probe>>,
    pub(super) direct: Option<Arc<direct::Direct>>,
    pub(super) capability: String,
    pub(super) dir: PathBuf,
    pub(super) child: Option<Child>,
    pub(super) touched: Instant,
    pub(super) stable_target_duration: bool,
    pub(super) target_duration: Option<u32>,
    pub(super) supervised_live: bool,
    pub(super) throttle: Throttle,
    pub(super) permits: Arc<InputPermits>,
    pub(super) cleanup_tasks: CleanupTasks,
}
impl Session {
    pub(super) async fn cleanup(mut self) {
        if let Some(stream) = &self.stream {
            stream.close();
        }
        if let Some(direct) = &self.direct {
            direct
                .closed
                .store(true, std::sync::atomic::Ordering::Release);
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        self.child.take();
        if tokio::fs::remove_dir_all(&self.dir).await.is_ok() {
            self.dir = PathBuf::new();
        }
    }
}
// Also clean partially-started sessions when an HTTP request is cancelled.
impl Drop for Session {
    fn drop(&mut self) {
        if let Some(stream) = &self.stream {
            stream.close();
        }
        if let Some(direct) = &self.direct {
            direct
                .closed
                .store(true, std::sync::atomic::Ordering::Release);
        }
        let mut child = self.child.take();
        if let Some(child) = child.as_mut() {
            let _ = child.start_kill();
        }
        if self.dir.as_os_str().is_empty() && child.is_none() {
            return;
        }
        let dir = self.dir.clone();
        let permits = self.permits.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let task = runtime.spawn(async move {
                if let Some(mut child) = child {
                    let _ = child.wait().await;
                }
                if !dir.as_os_str().is_empty() {
                    let _ = tokio::fs::remove_dir_all(dir).await;
                }
                drop(permits);
            });
            let mut tasks = self.cleanup_tasks.lock().unwrap_or_else(|e| e.into_inner());
            tasks.retain(|task| !task.is_finished());
            tasks.push(task);
        }
    }
}

// Run exactly once before admitting a session. Dedicated single-owner root means
// pre-existing generated directories are crash leftovers, even if recently written.
pub(super) async fn cleanup_orphans(root: &std::path::Path) -> Result<(), String> {
    let mut entries = match tokio::fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("Media storage unavailable".into()),
    };
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|_| "Media storage unavailable".to_owned())?
    {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(id) = Uuid::parse_str(name) else {
            continue;
        };
        if id.get_version_num() != 4 || id.to_string() != name {
            continue;
        }
        if !entry
            .file_type()
            .await
            .map_err(|_| "Media storage unavailable".to_owned())?
            .is_dir()
        {
            continue;
        }
        let mut files = tokio::fs::read_dir(entry.path())
            .await
            .map_err(|_| "Media storage unavailable".to_owned())?;
        let mut generated_only = true;
        while let Some(file) = files
            .next_entry()
            .await
            .map_err(|_| "Media storage unavailable".to_owned())?
        {
            let filename = file.file_name();
            let known = filename.to_str().is_some_and(|name| {
                media_type(name.strip_suffix(".tmp").unwrap_or(name)).is_some()
            });
            if !known
                || !file
                    .file_type()
                    .await
                    .map_err(|_| "Media storage unavailable".to_owned())?
                    .is_file()
            {
                generated_only = false;
                break;
            }
        }
        if generated_only {
            tokio::fs::remove_dir_all(entry.path())
                .await
                .map_err(|_| "Media storage cleanup failed".to_owned())?;
        }
    }
    Ok(())
}

pub(super) fn input_args(cmd: &mut Command, headers: &str) {
    // The engine's explicit egress policy must not be bypassed by host shell
    // proxy exclusions or silently replaced by an inherited system proxy.
    for variable in [
        "no_proxy",
        "NO_PROXY",
        "http_proxy",
        "HTTP_PROXY",
        "https_proxy",
        "HTTPS_PROXY",
        "all_proxy",
        "ALL_PROXY",
    ] {
        cmd.env_remove(variable);
    }
    // Restrict nested playlists/redirects to network protocols, never local files or devices.
    cmd.args([
        "-protocol_whitelist",
        "http,https,httpproxy,tcp,tls,crypto",
        "-rw_timeout",
        "10000000",
        // Recover premature HTTP bodies at their byte offset, with short bounded backoff.
        // Deliberately omit reconnect_at_eof and reconnect_on_http_error: normal VOD
        // completion and authentication failures must not restart or retry forever.
        "-reconnect",
        "1",
        "-reconnect_streamed",
        "1",
        "-reconnect_delay_max",
        "2",
    ]);
    // This server-authored transport field is consumed here, never sent upstream.
    let mut public = String::new();
    for line in headers.split("\r\n").filter(|line| !line.is_empty()) {
        if let Some(proxy) = line.strip_prefix("x-playback-egress-proxy: ") {
            cmd.arg("-http_proxy").arg(proxy);
        } else {
            public.push_str(line);
            public.push_str("\r\n");
        }
    }
    if !public.is_empty() {
        cmd.arg("-headers").arg(public);
    }
}
pub(super) fn header_block(headers: &HashMap<String, String>) -> Result<String, String> {
    let mut pairs: Vec<_> = headers.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    let mut out = String::new();
    if pairs
        .iter()
        .filter(|(key, _)| !key.eq_ignore_ascii_case("x-playback-egress-proxy"))
        .count()
        > 32
    {
        return Err("Invalid playback headers".into());
    }
    for (key, value) in pairs {
        if key.is_empty()
            || key.len() > 128
            || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.len() > 4096
            || value.bytes().any(|b| b < 32 || b == 127)
        {
            return Err("Invalid playback headers".into());
        }
        out.push_str(key);
        out.push_str(": ");
        out.push_str(value);
        out.push_str("\r\n");
    }
    if out.len() > 16384 {
        return Err("Invalid playback headers".into());
    }
    Ok(out)
}

pub(super) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}
