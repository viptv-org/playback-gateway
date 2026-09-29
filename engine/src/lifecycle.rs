use super::*;
use axum::http::header;
use axum::response::IntoResponse;

/// Streams a session media file in bounded chunks. Returning the concrete
/// stream type pins the macro's error type to `std::io::Error`, which
/// `Body::from_stream` accepts directly.
fn file_stream(
    file: tokio::fs::File,
) -> impl futures::Stream<Item = Result<axum::body::Bytes, std::io::Error>> {
    async_stream::try_stream! {
        use tokio::io::AsyncReadExt;
        let mut file = file;
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            yield axum::body::Bytes::copy_from_slice(&buffer[..read]);
        }
    }
}

impl PlaybackManager {
    pub async fn heartbeat(&self, id: &str) -> bool {
        self.heartbeat_at(id, None).await
    }
    pub async fn heartbeat_at(&self, id: &str, position: Option<f64>) -> bool {
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get_mut(id) {
            if session.touched.elapsed() < self.config.ttl {
                session.touched = Instant::now();
                if let (Some(stream), Some(position)) = (&session.stream, position) {
                    stream.observe(position);
                }
                return true;
            }
        }
        false
    }
    /// Call after draining HTTP requests, before shutting down the Tokio runtime.
    /// Dropping the manager kills children but only schedules best-effort deletion.
    pub async fn shutdown(&self) {
        self.slots.close();
        let _lifecycle = self.lifecycle.write().await;
        if self.initialize().await.is_err() {
            tracing::warn!("Playback startup cleanup could not finish");
        }
        let sessions = std::mem::take(&mut *self.sessions.lock().await);
        for (_, session) in sessions {
            session.cleanup().await;
        }
        let tasks =
            std::mem::take(&mut *self.cleanup_tasks.lock().unwrap_or_else(|e| e.into_inner()));
        for task in tasks {
            let _ = task.await;
        }
    }
    pub async fn stop(&self, id: &str) -> bool {
        let _lifecycle = self.lifecycle.read().await;
        let session = self.sessions.lock().await.remove(id);
        if let Some(session) = session {
            session.cleanup().await;
            true
        } else {
            false
        }
    }
    pub fn session_ttl(&self) -> Duration {
        self.config.ttl
    }
    pub fn is_shutting_down(&self) -> bool {
        self.slots.is_closed()
    }
    /// Includes successful EOF: a live input ending needs recovery even when
    /// its final cached playlist is still readable.
    pub async fn input_running(&self, id: &str) -> bool {
        let mut sessions = self.sessions.lock().await;
        let Some(session) = sessions.get_mut(id) else {
            return false;
        };
        if let Some(direct) = &session.direct {
            return !direct.closed.load(std::sync::atomic::Ordering::Acquire)
                && !direct.failed.load(std::sync::atomic::Ordering::Acquire);
        }
        matches!(
            session.child.as_mut().map(|child| child.try_wait()),
            Some(Ok(None))
        )
    }
    /// Sequential delivery has handed data to a bounded, paused client body.
    /// Do not diagnose an upstream stall while no upstream read is requested.
    pub async fn input_backpressured(&self, id: &str) -> bool {
        self.sessions.lock().await.get(id).is_some_and(|session| {
            session
                .stream
                .as_ref()
                .is_some_and(|stream| stream.backpressured())
                || session.direct.as_ref().is_some_and(|direct| {
                    direct
                        .waiting_consumers
                        .load(std::sync::atomic::Ordering::Acquire)
                        > 0
                })
        })
    }
    /// Hand progress policy to the owned live supervisor; quota/TTL watchdogs remain active.
    pub async fn supervise_live(&self, id: &str) {
        if let Some(session) = self.sessions.lock().await.get_mut(id) {
            session.supervised_live = true;
        }
    }
    /// Completed media sequence, independent of client playback position or picture content.
    pub async fn live_progress(&self, id: &str) -> Option<String> {
        let (dir, direct) = {
            let sessions = self.sessions.lock().await;
            let session = sessions.get(id)?;
            if let Some(stream) = &session.stream {
                return Some(format!("media-time:{:.3}", stream.times().0));
            }
            (session.dir.clone(), session.direct.clone())
        };
        if let Some(direct) = direct {
            return direct.progress().await;
        }
        let bytes = tokio::fs::read(dir.join("index.m3u8")).await.ok()?;
        if bytes.len() > 128 * 1024 {
            return None;
        }
        let playlist = String::from_utf8(bytes).ok()?;
        Some(
            playlist
                .lines()
                .filter(|line| {
                    line.starts_with("#EXT-X-MEDIA-SEQUENCE:")
                        || (!line.is_empty() && !line.starts_with('#'))
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
    /// A late on-demand viewer can reuse this origin only while its initial
    /// segments remain present in the bounded rolling playlist.
    pub async fn timeline_origin_available(&self, id: &str) -> bool {
        let dir = match self.sessions.lock().await.get(id) {
            Some(s) => {
                if s.direct.is_some() {
                    return true;
                }
                s.dir.clone()
            }
            None => return false,
        };
        let Ok(bytes) = tokio::fs::read(dir.join("index.m3u8")).await else {
            return false;
        };
        bytes.len() <= 128 * 1024
            && String::from_utf8_lossy(&bytes)
                .lines()
                .any(|line| line == "#EXT-X-MEDIA-SEQUENCE:0")
    }
    pub async fn active_count(&self) -> usize {
        self.sessions
            .lock()
            .await
            .values()
            .filter(|s| s.touched.elapsed() < self.config.ttl)
            .count()
    }
    /// Snapshot live session identifiers for pruning authorization leases.
    /// No media capabilities or upstream URLs are exposed.
    pub async fn active_ids(&self) -> Vec<String> {
        self.sessions
            .lock()
            .await
            .iter()
            .filter(|(_, session)| session.touched.elapsed() < self.config.ttl)
            .map(|(id, _)| id.clone())
            .collect()
    }
    /// A timed-out preparation may leave asynchronous kill/wait cleanup. The
    /// caller must bound this wait; capacity itself stays held until reaping.
    pub async fn settle_cancelled_inputs(&self) {
        loop {
            if self
                .cleanup_tasks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .all(|task| task.is_finished())
            {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
    }
    /// Decode a bounded sample through ffprobe's frame decoder. The upstream
    /// response is piped once and capped independently of subprocess output.
    pub async fn sample_media(
        &self,
        url: String,
        provider: OwnedSemaphorePermit,
        proxy: Option<String>,
        limits: SampleLimits,
    ) -> Result<serde_json::Value, String> {
        use tokio::io::AsyncWriteExt;
        let seconds = limits.seconds;
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "deferred_capacity")?;
        let permits = Arc::new(InputPermits {
            _playback: slot,
            _provider: Some(provider),
        });
        let client = crate::egress_proxy_builder(
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(limits.budget_seconds))
                .redirect(reqwest::redirect::Policy::limited(3)),
            proxy.as_deref(),
        )?
        .build()
        .map_err(|_| "request_failed")?;
        let start = Instant::now();
        let mut response = timeout(
            Duration::from_secs(limits.startup_seconds),
            client.get(url).send(),
        )
        .await
        .map_err(|_| "startup_timeout")?
        .map_err(|_| "network_failed")?;
        match response.status().as_u16() {
            200..=299 => {}
            401 | 403 => return Err("authentication_failed".into()),
            429 => return Err("rate_limited".into()),
            _ => return Err("network_failed".into()),
        }
        let mut child=Command::new(&self.config.ffprobe).args(["-v","error","-protocol_whitelist","pipe","-analyzeduration","5000000","-probesize","5000000","-read_intervals",&format!("%+{seconds}"),"-show_frames","-show_streams","-show_entries","frame=media_type,best_effort_timestamp_time:stream=codec_type,codec_name,width,height,channels","-of","json","-i","pipe:0"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn().map_err(|_|"decoder_unavailable")?;
        let mut input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let error = child.stderr.take().unwrap();
        let mut guard = ProbeChild {
            child: Some(child),
            permits: Some(permits),
            cleanup_tasks: self.cleanup_tasks.clone(),
        };
        let feeding = async {
            let mut bytes = 0usize;
            let mut first = None;
            loop {
                let chunk = if first.is_none() {
                    timeout(
                        Duration::from_secs(limits.startup_seconds).saturating_sub(start.elapsed()),
                        response.chunk(),
                    )
                    .await
                    .map_err(|_| "startup_timeout")?
                    .map_err(|_| "network_failed")?
                } else {
                    response.chunk().await.map_err(|_| "network_failed")?
                };
                let Some(chunk) = chunk else {
                    break;
                };
                if first.is_none() {
                    first = Some(start.elapsed().as_millis() as u64);
                }
                bytes = bytes.saturating_add(chunk.len());
                if bytes > limits.max_bytes {
                    return Err("sample_byte_limit");
                }
                if input.write_all(&chunk).await.is_err() {
                    break;
                }
            }
            drop(input);
            Ok::<_, &str>((bytes, first.unwrap_or(0)))
        };
        let sample=timeout(Duration::from_secs(limits.budget_seconds).saturating_sub(start.elapsed()),async {
            let (feeding,out,err,status)=tokio::join!(feeding,probe_output(output,2*1024*1024),probe_output(error,64*1024),guard.child.as_mut().unwrap().wait());
            let (bytes,startup)=feeding.map_err(str::to_owned)?;
            let out=out.map_err(|_|"invalid_media")?;let _=err;
            if !status.is_ok_and(|s|s.success()){return Err("invalid_media".to_owned());}
            let data:serde_json::Value=serde_json::from_slice(&out).map_err(|_|"invalid_media")?;
            let frames=data["frames"].as_array().ok_or("invalid_media")?;
            let timestamps=frames.iter().filter(|f|f["media_type"]=="video").filter_map(|f|f["best_effort_timestamp_time"].as_str()?.parse::<f64>().ok()).filter(|v|v.is_finite()).collect::<Vec<_>>();
            let advancing=timestamps.windows(2).filter(|v|v[1]>v[0]).count();
            let span=timestamps.last().zip(timestamps.first()).map(|(last,first)|last-first).unwrap_or(0.0);
            if advancing<8||span<(seconds as f64*0.7){return Err("media_not_advancing".to_owned());}
            let streams=data["streams"].as_array().ok_or("invalid_media")?;
            let video=streams.iter().find(|s|s["codec_type"]=="video").ok_or("invalid_media")?;
            let audio=streams.iter().find(|s|s["codec_type"]=="audio");
            Ok(serde_json::json!({"state":if audio.is_some(){"healthy"}else{"degraded"},"reason":if audio.is_some(){"decoded_advancing_media"}else{"audio_absent"},"startup_ms":startup,"sample_seconds":span,"sample_bytes":bytes,"video_codec":video["codec_name"],"width":video["width"],"height":video["height"],"audio_codec":audio.map(|s|s["codec_name"].clone()),"audio_channels":audio.map(|s|s["channels"].clone())}))
        }).await.unwrap_or_else(|_|Err("sample_timeout".into()));
        guard.reap().await;
        sample
    }
    pub async fn ffmpeg_available(&self) -> bool {
        let mut cmd = Command::new(&self.config.ffmpeg);
        cmd.arg("-version")
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        matches!(timeout(Duration::from_secs(3), cmd.status()).await, Ok(Ok(status)) if status.success())
    }
    pub async fn serve_original(
        &self,
        id: &str,
        capability: &str,
        file: &str,
        method: axum::http::Method,
        headers: axum::http::HeaderMap,
    ) -> Option<Result<axum::response::Response, String>> {
        let (direct, stream) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions.get_mut(id)?;
            if session.capability != capability || session.touched.elapsed() >= self.config.ttl {
                return Some(Err("Media expired".into()));
            }
            let direct = session.direct.clone();
            session.touched = Instant::now();
            (direct, session.stream.clone())
        };
        if file == "stream.mp4" {
            if let Some(stream) = stream {
                return Some(stream.serve(method).await);
            }
        }
        if file == "tracks.json" || file.starts_with("subtitle-") {
            return Some(self.serve_tracks(direct?, file, method, headers).await);
        }
        Some(direct?.serve(file, method, headers).await)
    }

    pub async fn serve(
        &self,
        id: &str,
        capability: &str,
        file: &str,
    ) -> Result<axum::response::Response, String> {
        let mime = media_type(file).ok_or_else(|| "Media not found".to_owned())?;
        let (path, stable_target_duration) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .get_mut(id)
                .filter(|s| {
                    s.touched.elapsed() < self.config.ttl
                        && constant_time_eq(s.capability.as_bytes(), capability.as_bytes())
                })
                .ok_or_else(|| "Media not found".to_owned())?;
            session.touched = Instant::now();
            // Every fetched segment moves the throttle's resume point forward.
            session.throttle.observe(file);
            (session.dir.join(file), session.stable_target_duration)
        };
        let metadata = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(|_| "Media not found".to_owned())?;
        if !metadata.is_file() || metadata.len() > 32 * 1024 * 1024 {
            return Err("Media not found".into());
        }
        let length = metadata.len();
        // Playlists and captions are rewritten (target duration, caption
        // clock) so they are read fully; they are bounded and small.
        if mime == "application/vnd.apple.mpegurl" || mime == "text/vtt" {
            let mut bytes = tokio::fs::read(&path)
                .await
                .map_err(|_| "Media not found".to_owned())?;
            if mime == "application/vnd.apple.mpegurl" && stable_target_duration {
                bytes = stable_hls_target_duration(bytes);
                let text = std::str::from_utf8(&bytes).map_err(|_| "Invalid HLS output")?;
                if let Some(advertised) = text.lines().find_map(|line| {
                    line.strip_prefix("#EXT-X-TARGETDURATION:")?
                        .parse::<u32>()
                        .ok()
                }) {
                    let target = {
                        let mut sessions = self.sessions.lock().await;
                        let session = sessions.get_mut(id).ok_or("Media expired")?;
                        *session.target_duration.get_or_insert(advertised)
                    };
                    if advertised > target {
                        return Err("Copied HLS GOP requires compatible delivery".into());
                    }
                    bytes = text
                        .replacen(
                            &format!("#EXT-X-TARGETDURATION:{advertised}"),
                            &format!("#EXT-X-TARGETDURATION:{target}"),
                            1,
                        )
                        .into_bytes();
                }
            }
            if mime == "text/vtt" && bytes.starts_with(b"WEBVTT\n") {
                // Caption-enabled MPEGTS uses copyts, so both renditions share clock0.
                let mut mapped =
                    b"WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:0\n\n".to_vec();
                mapped.extend_from_slice(&bytes[7..]);
                bytes = mapped;
            }
            return Ok((
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                bytes,
            )
                .into_response());
        }
        // Segments and init data stream straight from disk: no whole-file
        // buffer per request, while Content-Length stays authoritative.
        let file = tokio::fs::File::open(&path)
            .await
            .map_err(|_| "Media not found".to_owned())?;
        axum::http::Response::builder()
            .header(header::CONTENT_TYPE, mime)
            .header(header::CACHE_CONTROL, "no-store")
            .header(header::CONTENT_LENGTH, length)
            .body(axum::body::Body::from_stream(file_stream(file)))
            .map_err(|_| "Media not found".to_owned())
    }
}
