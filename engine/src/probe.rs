use super::*;

const PROBE_STDOUT_LIMIT: usize = 1024 * 1024;
const PROBE_STDERR_LIMIT: usize = 64 * 1024;

pub(super) const PROBE_CACHE_TTL: Duration = Duration::from_secs(120);
/// Live sources keep their stream identity while content rolls, so a short
/// reuse window absorbs channel hopping without serving long-stale metadata.
pub(super) const LIVE_PROBE_CACHE_TTL: Duration = Duration::from_secs(30);
const PROBE_CACHE_CAP: usize = 128;

#[derive(Deserialize)]
struct ProbeDocument {
    #[serde(flatten)]
    metadata: Probe,
    #[serde(default)]
    frames: Vec<ProbeFrame>,
}

#[derive(Deserialize)]
struct ProbeFrame {
    stream_index: Option<u32>,
    interlaced_frame: Option<u8>,
    top_field_first: Option<u8>,
}

fn decoded_field_order(bytes: &[u8]) -> Result<Probe, ProbeFailure> {
    let mut document: ProbeDocument =
        serde_json::from_slice(bytes).map_err(|_| ProbeFailure::InvalidJson)?;
    for video in document
        .metadata
        .streams
        .iter_mut()
        .filter(|s| s.is_video())
    {
        let frames: Vec<_> = document
            .frames
            .iter()
            .filter(|f| f.stream_index.is_some() && f.stream_index == video.index)
            .collect();
        if let Some(frame) = frames.iter().find(|f| f.interlaced_frame == Some(1)) {
            video.field_order = Some(
                if frame.top_field_first == Some(1) {
                    "tt"
                } else {
                    "bb"
                }
                .into(),
            );
        } else if matches!(video.field_order.as_deref(), None | Some("unknown"))
            && frames.iter().any(|f| f.interlaced_frame == Some(0))
        {
            video.field_order = Some("progressive".into());
        }
    }
    Ok(document.metadata)
}

fn hls_path(url: &str) -> bool {
    url::Url::parse(url).ok().is_some_and(|url| {
        let path = url.path().to_ascii_lowercase();
        path.ends_with(".m3u8") || path.ends_with(".m3u")
    })
}

fn hls_field_order_missing(probe: &Probe) -> bool {
    probe.format["format_name"]
        .as_str()
        .is_some_and(|f| f.split(',').any(|f| f == "hls"))
        && probe.video().is_ok_and(|v| {
            v.codec_name.as_deref() == Some("h264")
                && matches!(v.field_order.as_deref(), None | Some("unknown"))
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProbeFailure {
    Spawn,
    OutputRead,
    Oversized,
    /// Metadata that exceeded the stdout budget, which a smaller entry set can fix.
    OversizedOutput,
    InvalidJson,
    Protocol,
    /// FFmpeg refused an HLS segment by its name before reading its bytes.
    SegmentExtension,
    /// An HLS segment was typefound as a still image (a disguise prefix).
    ImageSegment,
    Http(u16),
    Network,
    Timeout,
    Exit,
}
impl ProbeFailure {
    pub(super) fn retryable(self) -> bool {
        matches!(
            self,
            Self::Http(404 | 408 | 429 | 500..=599) | Self::Network | Self::Timeout
        )
    }
}

// Own the child across every await, including cancellation during kill/wait. Diagnostics
// never escape memory; only the closed failure enum (and recognized HTTP code) is logged.
pub(super) struct ProbeChild {
    pub(super) child: Option<Child>,
    pub(super) permits: Option<Arc<InputPermits>>,
    pub(super) cleanup_tasks: CleanupTasks,
}
impl ProbeChild {
    pub(super) async fn reap(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        self.child.take();
    }
}
impl Drop for ProbeChild {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = child.start_kill();
        let permits = self.permits.take();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let task = runtime.spawn(async move {
                let _ = child.wait().await;
                drop(permits);
            });
            let mut tasks = self.cleanup_tasks.lock().unwrap_or_else(|e| e.into_inner());
            tasks.retain(|task| !task.is_finished());
            tasks.push(task);
        }
    }
}

pub(super) async fn probe_output(
    reader: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
) -> Result<Vec<u8>, ProbeFailure> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| ProbeFailure::OutputRead)?;
    if bytes.len() > limit {
        return Err(ProbeFailure::Oversized);
    }
    Ok(bytes)
}

pub(super) fn probe_failure(stderr: &[u8]) -> ProbeFailure {
    let text = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if text.contains("allowed_segment_extensions") || text.contains("mismatches allowed extensions")
    {
        return ProbeFailure::SegmentExtension;
    }
    if text.contains("hls")
        && [
            "jpeg_pipe",
            "png_pipe",
            "gif_pipe",
            "bmp_pipe",
            "webp_pipe",
            "image2",
        ]
        .iter()
        .any(|s| text.contains(s))
    {
        return ProbeFailure::ImageSegment;
    }
    if [
        "not on whitelist",
        "protocol not found",
        "protocol not allowed",
        "operation not permitted",
    ]
    .iter()
    .any(|s| text.contains(s))
    {
        return ProbeFailure::Protocol;
    }
    let mut http = None;
    for prefix in [
        "http error ",
        "server returned ",
        "http/1.1 ",
        "http/1.0 ",
        "http/2 ",
    ] {
        for (_, tail) in text
            .match_indices(prefix)
            .map(|(i, p)| (i, &text[i + p.len()..]))
        {
            if let Some(code) = tail
                .get(..3)
                .filter(|s| {
                    s.bytes().all(|b| b.is_ascii_digit())
                        && tail
                            .as_bytes()
                            .get(3)
                            .is_none_or(|b| b.is_ascii_whitespace())
                })
                .and_then(|s| s.parse::<u16>().ok())
                .filter(|c| (400..=599).contains(c))
            {
                // A permanent status anywhere must not be masked by a later transient one.
                if !ProbeFailure::Http(code).retryable() {
                    return ProbeFailure::Http(code);
                }
                http = Some(code);
            }
        }
    }
    if let Some(code) = http {
        return ProbeFailure::Http(code);
    }
    if [
        "connection timed out",
        "connection reset",
        "operation timed out",
        "i/o timeout",
    ]
    .iter()
    .any(|s| text.contains(s))
    {
        return ProbeFailure::Network;
    }
    ProbeFailure::Exit
}

/// FFmpeg could not read an HLS source's segments as media: their names were
/// refused, or they were typefound as still images.
pub(super) fn disguise_signal(result: &Result<Probe, ProbeFailure>) -> bool {
    match result {
        Err(ProbeFailure::SegmentExtension | ProbeFailure::ImageSegment) => true,
        Ok(probe) => probe.image_only_hls(),
        Err(_) => false,
    }
}

pub(super) struct ProbeCacheEntry {
    pub(super) probe: Arc<Probe>,
    pub(super) inserted: Instant,
    /// Live entries expire on the shorter live window; VOD metadata is stable.
    pub(super) live: bool,
}
// Drop expired entries; anything a caller still holds stays for reuse.
fn prune_probe_cache(cache: &mut HashMap<[u8; 32], ProbeCacheEntry>, now: Instant) {
    cache.retain(|_, entry| {
        let ttl = if entry.live {
            LIVE_PROBE_CACHE_TTL
        } else {
            PROBE_CACHE_TTL
        };
        Arc::strong_count(&entry.probe) > 1 || now.duration_since(entry.inserted) < ttl
    });
}

impl PlaybackManager {
    /// Demux only a bounded packet sample at the source's indexed seek point.
    /// Returning None retains the accurate decode path; no guessed preroll.
    pub(super) async fn copy_seek_position(
        &self,
        url: &str,
        headers: &str,
        position: f64,
        origin: f64,
        permits: Arc<InputPermits>,
        sanitizer: Option<&sanitize::HlsSanitizer>,
    ) -> Option<f64> {
        if position == 0.0 {
            return Some(0.0);
        }
        let mut cmd = Command::new(&self.config.ffprobe);
        cmd.args(["-v", "error"]);
        let input = source_input(&mut cmd, url, headers, sanitizer);
        cmd.args([
            "-select_streams",
            "v:0",
            "-read_intervals",
            &format!("{position:.6}%+#16"),
            "-show_packets",
            "-show_entries",
            "packet=pts_time,flags",
            "-of",
            "json",
            "-i",
            &input,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
        let mut child = cmd.spawn().ok()?;
        let stdout = child.stdout.take()?;
        let mut owned = ProbeChild {
            child: Some(child),
            permits: Some(permits),
            cleanup_tasks: self.cleanup_tasks.clone(),
        };
        let result = timeout(Duration::from_secs(5), async {
            let bytes = probe_output(stdout, 64 * 1024).await.ok()?;
            if !owned.child.as_mut()?.wait().await.ok()?.success() {
                return None;
            }
            let parsed: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            parsed["packets"]
                .as_array()?
                .iter()
                .filter(|p| p["flags"].as_str().is_some_and(|f| f.contains('K')))
                .filter_map(|p| p["pts_time"].as_str()?.parse::<f64>().ok())
                .map(|timestamp| timestamp - origin)
                .filter(|v| v.is_finite() && *v >= 0.0 && *v <= position)
                .max_by(f64::total_cmp)
        })
        .await
        .ok()
        .flatten();
        owned.reap().await;
        result
    }
    /// One inspection per source identity. Live and VOD entries share this
    /// bounded cache, told apart by the live discriminator in the digest, so a
    /// channel hop inside the short live TTL no longer pays a fresh ffprobe
    /// (with retries, up to ~10s) before playback can start, while VOD keeps
    /// the longer window its stable metadata allows.
    pub(super) async fn cached_probe(
        &self,
        url: &str,
        headers: &str,
        live: bool,
        permits: Option<Arc<InputPermits>>,
    ) -> Option<Arc<Probe>> {
        self.cached_probe_scoped(url, headers, live, permits, None)
            .await
    }
    pub(super) async fn cached_probe_scoped(
        &self,
        url: &str,
        headers: &str,
        live: bool,
        permits: Option<Arc<InputPermits>>,
        scope: Option<&str>,
    ) -> Option<Arc<Probe>> {
        let mut digest = Sha256::new();
        if let Some(scope) = scope {
            digest.update((scope.len() as u64).to_be_bytes());
            digest.update(scope.as_bytes());
        }
        digest.update(url.as_bytes());
        digest.update([0]);
        digest.update(headers.as_bytes());
        digest.update([live as u8]);
        let key: [u8; 32] = digest.finalize().into();
        let now = Instant::now();
        {
            let mut cache = self.probe_cache.lock().await;
            prune_probe_cache(&mut cache, now);
            if let Some(entry) = cache.get(&key) {
                tracing::info!(live, "Source inspection cache hit");
                return Some(entry.probe.clone());
            }
        }
        let flight = {
            let mut flights = self.probe_flights.lock().await;
            flights.retain(|_, flight| flight.strong_count() > 0);
            if let Some(flight) = flights.get(&key).and_then(std::sync::Weak::upgrade) {
                flight
            } else {
                let flight = Arc::new(Mutex::new(()));
                flights.insert(key, Arc::downgrade(&flight));
                flight
            }
        };
        let _flight = flight.lock().await;
        if let Some(entry) = self.probe_cache.lock().await.get(&key) {
            tracing::info!(live, "Source inspection coalesced");
            return Some(entry.probe.clone());
        }
        tracing::info!(live, "Source inspection cache miss");
        let probe = Arc::new(self.probe(url, headers, permits).await?);
        let mut cache = self.probe_cache.lock().await;
        prune_probe_cache(&mut cache, now);
        if cache.len() >= PROBE_CACHE_CAP {
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, entry)| (Arc::strong_count(&entry.probe) > 1, entry.inserted))
                .map(|(key, _)| *key)
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(
            key,
            ProbeCacheEntry {
                probe: probe.clone(),
                inserted: Instant::now(),
                live,
            },
        );
        Some(probe)
    }

    pub(super) async fn probe(
        &self,
        url: &str,
        headers: &str,
        permits: Option<Arc<InputPermits>>,
    ) -> Option<Probe> {
        let mut unparsable = false;
        for attempt in 0..2 {
            let attempt_started = Instant::now();
            match self
                .probe_attempt(url, headers, permits.clone(), false)
                .await
            {
                Ok(probe) => return Some(probe),
                Err(category) => {
                    tracing::warn!(
                        ?category,
                        attempt,
                        probe_ms = attempt_started.elapsed().as_millis() as u64,
                        "Source probe failed"
                    );
                    unparsable |= matches!(
                        category,
                        ProbeFailure::InvalidJson | ProbeFailure::OversizedOutput
                    );
                    if attempt == 1 || !category.retryable() {
                        break;
                    }
                    // Caller-owned playback/provider permits remain held during this delay.
                    sleep(Duration::from_millis(1500)).await;
                }
            }
        }
        if !unparsable {
            return None;
        }
        // Unreadable metadata is usually a noisy or oversized response rather
        // than a dead source. Ask again for the smallest sufficient field set
        // instead of refusing something the server can still deliver.
        match self.probe_attempt(url, headers, permits, true).await {
            Ok(probe) => {
                tracing::info!("Reduced source probe succeeded");
                Some(probe)
            }
            Err(category) => {
                tracing::warn!(?category, "Reduced source probe failed");
                None
            }
        }
    }

    pub(super) async fn probe_attempt(
        &self,
        url: &str,
        headers: &str,
        permits: Option<Arc<InputPermits>>,
        reduced: bool,
    ) -> Result<Probe, ProbeFailure> {
        let sample_frames = hls_path(url);
        let mut direct = self
            .probe_command(url, headers, permits.clone(), reduced, None, sample_frames)
            .await;
        // Ordinary HLS uses a single process for metadata and a bounded keyframe
        // sample. Extensionless HLS is discovered from actual demuxer metadata.
        if !sample_frames && direct.as_ref().is_ok_and(hls_field_order_missing) {
            direct = self
                .probe_command(url, headers, permits.clone(), reduced, None, true)
                .await;
        }
        if !disguise_signal(&direct) {
            return direct;
        }
        // Refused segment names (FFmpeg 7.1+) and image-typefound segments
        // are read again through a loopback sanitizer that strips the
        // disguise. It lives only for this probe; readers start their own.
        let Ok(sanitizer) =
            sanitize::HlsSanitizer::start(url, &sanitize::header_map(headers), permits.clone())
                .await
        else {
            tracing::debug!("Disguised HLS sanitizer could not prepare the source");
            return direct;
        };
        match self
            .probe_command(url, headers, permits, reduced, Some(&sanitizer), true)
            .await
        {
            Ok(mut probe) if !probe.image_only_hls() => {
                probe.disguised_hls = true;
                tracing::info!("Disguised HLS source probed through the sanitizer");
                Ok(probe)
            }
            Ok(_) => direct,
            Err(failure) => match direct {
                Ok(probe) => Ok(probe),
                Err(_) => Err(failure),
            },
        }
    }

    async fn probe_command(
        &self,
        url: &str,
        headers: &str,
        permits: Option<Arc<InputPermits>>,
        reduced: bool,
        sanitizer: Option<&sanitize::HlsSanitizer>,
        sample_frames: bool,
    ) -> Result<Probe, ProbeFailure> {
        let mut cmd = Command::new(&self.config.ffprobe);
        cmd.kill_on_drop(true)
            .stdin(Stdio::null())
            .stderr(Stdio::piped());
        cmd.args(["-v", "error"]);
        let input = source_input(&mut cmd, url, headers, sanitizer);
        // The reduced form keeps every field the delivery decision needs, including
        // SDR/HDR transfer evidence, while shrinking a response that failed to parse.
        let entries = if reduced {
            "format=duration,format_name,start_time:stream=index,codec_type,codec_name,start_time,width,height,pix_fmt,channels,avg_frame_rate,color_transfer"
        } else {
            "format=duration,format_name,start_time:stream=index,codec_type,codec_name,start_time,width,height,pix_fmt,sample_aspect_ratio,profile,level,channels,avg_frame_rate,r_frame_rate,color_transfer,field_order:stream_tags=language,title:stream_disposition=default,comment,hearing_impaired,visual_impaired,forced,attached_pic"
        };
        let entries = if sample_frames {
            cmd.args([
                "-threads",
                "1",
                "-skip_frame",
                "nokey",
                "-read_intervals",
                "%+#8",
                "-show_frames",
            ]);
            format!(
                "{entries}:frame=stream_index,interlaced_frame,top_field_first:frame_tags=:frame_side_data="
            )
        } else {
            entries.to_owned()
        };
        cmd.args([
            "-analyzeduration",
            "5000000",
            "-probesize",
            "5000000",
            "-show_entries",
            &entries,
            "-of",
            "json",
            "-i",
            &input,
        ]);
        cmd.stdout(Stdio::piped());
        let child = cmd.spawn().map_err(|_| ProbeFailure::Spawn)?;
        let mut owned = ProbeChild {
            child: Some(child),
            permits,
            cleanup_tasks: self.cleanup_tasks.clone(),
        };
        let child = owned.child.as_mut().ok_or(ProbeFailure::Spawn)?;
        let stdout = child.stdout.take().ok_or(ProbeFailure::OutputRead)?;
        let stderr = child.stderr.take().ok_or(ProbeFailure::OutputRead)?;
        let result = timeout(Duration::from_secs(10), async {
            // Read both pipes concurrently. A limit violation short-circuits all readers
            // and kills the child rather than draining attacker-controlled output forever.
            tokio::try_join!(
                async {
                    probe_output(stdout, PROBE_STDOUT_LIMIT).await.map_err(
                        |failure| match failure {
                            ProbeFailure::Oversized => ProbeFailure::OversizedOutput,
                            other => other,
                        },
                    )
                },
                probe_output(stderr, PROBE_STDERR_LIMIT),
                async { child.wait().await.map_err(|_| ProbeFailure::Exit) },
            )
        })
        .await;
        owned.reap().await;
        let (bytes, stderr, status) = result.map_err(|_| ProbeFailure::Timeout)??;
        if !status.success() {
            // Failed ffprobe commonly emits an empty JSON object (or no stdout).
            // Malformed output is not evidence of a transient upstream input failure.
            if bytes.iter().any(|b| !b.is_ascii_whitespace()) {
                serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&bytes)
                    .map_err(|_| ProbeFailure::InvalidJson)?;
            }
            // The capability never survives into anything a caller could log.
            let stderr = match sanitizer {
                Some(sanitizer) => sanitizer
                    .redact(&String::from_utf8_lossy(&stderr))
                    .into_bytes(),
                None => stderr,
            };
            return Err(probe_failure(&stderr));
        }
        decoded_field_order(&bytes)
    }
}

#[cfg(test)]
mod frame_evidence_tests {
    use super::*;

    #[test]
    fn hls_frame_evidence_prevents_interlaced_h264_copy_without_losing_other_tracks() {
        let probe = decoded_field_order(br#"{"streams":[{"index":4,"codec_type":"video","codec_name":"h264"},{"index":9,"codec_type":"audio","codec_name":"aac"}],"format":{"format_name":"hls"},"frames":[{"stream_index":9},{"stream_index":4,"interlaced_frame":1,"top_field_first":1}]}"#).unwrap();
        assert!(probe.interlaced());
        assert_eq!(probe.streams.len(), 2);
    }

    #[test]
    fn progressive_frame_evidence_does_not_erase_known_interlaced_metadata() {
        let probe = decoded_field_order(br#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264","field_order":"tt"}],"frames":[{"stream_index":0,"interlaced_frame":0}]}"#).unwrap();
        assert!(probe.interlaced());
        let probe = decoded_field_order(br#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"}],"frames":[{"stream_index":0,"interlaced_frame":0}]}"#).unwrap();
        assert_eq!(
            probe.video().unwrap().field_order.as_deref(),
            Some("progressive")
        );
    }
}
