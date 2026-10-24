use super::*;

// A one-second initial segment reduces player startup; steady-state stays at
// two seconds. Copied HLS cuts at keyframes so each fragment starts decodably.
pub(super) const HLS_INITIAL_SEGMENT_SECONDS: u32 = 1;
pub(super) const HLS_SEGMENT_SECONDS: u32 = 2;
pub(super) const HLS_WINDOW_SECONDS: u32 = 120;
pub(super) const HLS_DELETE_GRACE_SECONDS: u32 = 60;
/// A video-only first fragment makes some existing readers permanently choose
/// no audio. Keep the selected audio's first packet inside that fragment when
/// the inspected streams begin at different timestamps. No re-encoding is needed.
pub(super) fn initial_segment_seconds(probe: &Probe, audio_index: Option<u32>) -> u32 {
    let parse = |stream: &ProbeStream| {
        stream
            .start_time
            .as_deref()?
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
    };
    let delay = probe
        .video()
        .ok()
        .and_then(|video| {
            let audio = probe
                .streams
                .iter()
                .find(|s| s.index == audio_index && s.codec_type.as_deref() == Some("audio"))?;
            Some(parse(audio)? - parse(video)?)
        })
        .unwrap_or(0.0);
    if delay > 0.25 {
        (delay + 1.0).ceil().clamp(1.0, 30.0) as u32
    } else {
        HLS_INITIAL_SEGMENT_SECONDS
    }
}

// An on-demand encoder runs far faster than realtime (a remux or a 1080p
// ultrafast encode measured 10-50x) and can finish a short input inside a
// single throttle tick. A rolling playlist window then publishes a completed
// VOD whose prefix has already slid out — a viewer starting at zero receives
// only the tail (ROK-MEDIA-20261024-MKV-COPY). VOD therefore lists every
// produced segment and never slides; like Plex's transcoder throttle buffer,
// suspend the encoder once it runs this far past the furthest segment a
// viewer fetched, resume it as the viewer catches up, and reclaim watched
// prefix files HLS_DELETE_GRACE_SECONDS behind that fetch. The pause point is
// bounded by how often the encoder is checked (HLS_THROTTLE_TICK) plus what
// its socket buffered while suspended (up to ~390x realtime during the
// one-second segments), so the viewer's next segment is always listed.
#[cfg(test)]
const HLS_WINDOW_SEGMENTS: u64 = (HLS_WINDOW_SECONDS / HLS_SEGMENT_SECONDS) as u64;
#[cfg(test)]
pub(super) const HLS_THROTTLE_AHEAD_SEGMENTS: u64 = HLS_WINDOW_SEGMENTS / 3;
#[cfg(test)]
pub(super) const HLS_THROTTLE_RESUME_SEGMENTS: u64 = HLS_WINDOW_SEGMENTS / 6;
pub(super) const HLS_THROTTLE_TICK: Duration = Duration::from_millis(100);
// A resumed input may first reconnect upstream; the stall watchdog waits this
// long after a resume before it expects playlist progress again.
const HLS_THROTTLE_RESUME_GRACE: Duration = Duration::from_secs(20);

/// Consumption-paced on-demand encoding. Live inputs are never suspended:
/// they arrive in realtime and an upstream socket must keep being read.
#[derive(Default)]
pub(super) struct Throttle {
    pub(super) enabled: bool,
    /// Furthest AV segment a viewer fetched.
    requested: u64,
    paused: bool,
    resumed: Option<Instant>,
    timeline: HashMap<u64, f64>,
    produced_seconds: f64,
    consumed_seconds: f64,
    last_segment: Option<u64>,
    deleted_below: u64,
    pending_deletions: Vec<String>,
    last_ahead: f64,
}
impl Throttle {
    pub(super) fn on_demand() -> Self {
        Self {
            enabled: true,
            ..Default::default()
        }
    }
    pub(super) fn observe(&mut self, file: &str) {
        if let Some(segment) = segment_number(file) {
            self.requested = self.requested.max(segment);
            if let Some(end) = self.timeline.get(&segment) {
                self.consumed_seconds = self.consumed_seconds.max(*end);
            }
        }
    }
    /// Actual EXTINF durations, including shorter initial and irregular copied GOPs.
    ///
    /// VOD playlists list every produced segment, so this also queues prefix
    /// cleanup: files whose media ends more than HLS_DELETE_GRACE_SECONDS
    /// behind the viewer's furthest fetch are reclaimed. Live sessions never
    /// run the throttle; their producer keeps the rolling window and deletes
    /// what slides out, exactly as before.
    pub(super) fn apply_playlist(&mut self, playlist: &str) {
        let mut duration = 0.0;
        let mut listed: Vec<(u64, &str)> = Vec::new();
        for line in playlist.lines() {
            if let Some(value) = line.strip_prefix("#EXTINF:") {
                duration = value
                    .split(',')
                    .next()
                    .and_then(|v| v.parse::<f64>().ok())
                    .filter(|v| v.is_finite() && *v > 0.0)
                    .unwrap_or(0.0);
            } else if let Some(index) = segment_number(line) {
                if self.last_segment.is_none_or(|last| index > last) {
                    self.produced_seconds += duration;
                    self.timeline.insert(index, self.produced_seconds);
                    self.last_segment = Some(index);
                }
                if index <= self.requested {
                    if let Some(end) = self.timeline.get(&index) {
                        self.consumed_seconds = self.consumed_seconds.max(*end);
                    }
                }
                listed.push((index, line));
                duration = 0.0;
            }
        }
        // Keep the whole timeline: a completed long feature is still only a
        // few thousand entries, and a late fetch of any listed segment must
        // resolve its end time or prefix cleanup would stall forever.
        // Reclaim the contiguous watched prefix. A file leaves only after its
        // media ended a full grace period behind the furthest fetch, so a
        // viewer's buffered media is never pulled from under it.
        let below = self.consumed_seconds - f64::from(HLS_DELETE_GRACE_SECONDS);
        for (index, file) in listed {
            if index < self.deleted_below {
                continue;
            }
            let Some(&end) = self.timeline.get(&index) else {
                break;
            };
            if end >= below {
                break;
            }
            self.pending_deletions.push(file.to_owned());
            self.deleted_below = index + 1;
        }
        self.last_ahead = self.produced_seconds - self.consumed_seconds;
    }
    pub(super) fn ahead(&self) -> f64 {
        self.last_ahead
    }
    /// Prefix segment files ready to reclaim; names come from the producer's
    /// own playlist and the caller still rejects path separators.
    pub(super) fn take_pending_deletions(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_deletions)
    }
    pub(super) fn apply_ahead(&mut self, child: &Child, ahead: f64) {
        if !self.paused && ahead > 40.0 {
            self.paused = suspend(child, true);
        } else if self.paused && ahead <= 20.0 && suspend(child, false) {
            self.paused = false;
            self.resumed = Some(Instant::now());
        }
    }
    /// While suspended, or just resumed, the playlist legitimately stops moving.
    pub(super) fn holding(&self) -> bool {
        self.paused
            || self
                .resumed
                .is_some_and(|at| at.elapsed() < HLS_THROTTLE_RESUME_GRACE)
    }
    /// Some(true) to suspend, Some(false) to resume, for the newest listed segment.
    #[cfg(test)]
    pub(super) fn transition(&self, newest: u64) -> Option<bool> {
        let ahead = newest.saturating_sub(self.requested);
        if !self.paused && ahead > HLS_THROTTLE_AHEAD_SEGMENTS {
            Some(true)
        } else if self.paused && ahead <= HLS_THROTTLE_RESUME_SEGMENTS {
            Some(false)
        } else {
            None
        }
    }
    /// Suspend or resume the encoder for the newest segment it has listed.
    #[cfg(test)]
    pub(super) fn apply(&mut self, child: &Child, newest: u64) {
        match self.transition(newest) {
            Some(true) => self.paused = suspend(child, true),
            Some(false) if suspend(child, false) => {
                self.paused = false;
                self.resumed = Some(Instant::now());
            }
            _ => {}
        }
    }
}

/// SIGSTOP/SIGCONT the encoder. A stopped process still dies on SIGKILL, so
/// every existing cleanup path keeps working while it is suspended.
#[cfg(unix)]
fn suspend(child: &Child, stop: bool) -> bool {
    let Some(pid) = child.id().and_then(|pid| libc::pid_t::try_from(pid).ok()) else {
        return false;
    };
    // SAFETY: kill(2) on our own unreaped child's pid; no memory is shared.
    unsafe { libc::kill(pid, if stop { libc::SIGSTOP } else { libc::SIGCONT }) == 0 }
}
#[cfg(not(unix))]
fn suspend(_: &Child, _: bool) -> bool {
    false
}

pub(super) fn segment_number(file: &str) -> Option<u64> {
    if !matches!(media_type(file), Some("video/mp2t" | "video/iso.segment")) {
        return None;
    }
    file.strip_prefix("segment-")?
        .rsplit_once('.')?
        .0
        .parse()
        .ok()
}

/// Newest AV segment the encoder has completed and listed.
#[cfg(test)]
pub(super) async fn newest_segment(dir: &std::path::Path) -> Option<u64> {
    let bytes = tokio::fs::read(dir.join("index.m3u8")).await.ok()?;
    if bytes.len() > 128 * 1024 {
        return None;
    }
    String::from_utf8_lossy(&bytes)
        .lines()
        .rev()
        .find_map(segment_number)
}

pub(super) fn dimensions(caps: &Capabilities) -> Result<(u32, u32), String> {
    if caps.max_width < 2 || caps.max_height < 2 {
        return Err("Invalid playback dimensions".into());
    }
    Ok((caps.max_width / 2 * 2, caps.max_height / 2 * 2))
}
pub(super) fn hdr_size(width: u32, height: u32) -> String {
    // Fit both axes, including portrait, without upscaling; round down to even pixels.
    format!(
        "w='trunc(min(iw,min({width},iw*{height}/ih))/2)*2':h='trunc(min(ih,min({height},ih*{width}/iw))/2)*2'"
    )
}
pub(super) fn hdr_filter(width: u32, height: u32) -> String {
    // zimg resizes BEFORE transfer conversion when both are in one zscale. Keep
    // explicit PQ/HLG -> float-linear RGB first to avoid averaging encoded light.
    // Fuse bounded linear-light resize with the linear BT709 primaries transform;
    // the expensive tone mapper and remaining conversions then run at target size.
    // Only FFmpeg 5.1-compatible options; preserve SDR tags and drop HDR side data.
    format!(
        "zscale=transfer=linear:npl=100,format=gbrpf32le,zscale={}:filter=bilinear:primaries=bt709,tonemap=tonemap=mobius:desat=2,zscale=transfer=bt709:matrix=bt709:range=limited,format=yuv420p,sidedata=mode=delete,setsar=1",
        hdr_size(width, height)
    )
}
pub(super) fn scale_filter(width: u32, height: u32) -> String {
    format!(
        "scale=w='min(iw,{width})':h='min(ih,{height})':force_original_aspect_ratio=decrease:force_divisible_by=2:flags=fast_bilinear,setsar=1"
    )
}
pub(super) fn stable_hls_target_duration(bytes: Vec<u8>) -> Vec<u8> {
    let mut playlist = match String::from_utf8(bytes) {
        Ok(playlist) => playlist,
        Err(error) => return error.into_bytes(),
    };
    let prefix = "#EXT-X-TARGETDURATION:";
    if let Some(start) = playlist.find(prefix) {
        let value_start = start + prefix.len();
        let value_end = playlist[value_start..]
            .find(['\r', '\n'])
            .map_or(playlist.len(), |offset| value_start + offset);
        let advertised = playlist[value_start..value_end].parse::<u32>().unwrap_or(0);
        if advertised < HLS_SEGMENT_SECONDS {
            playlist.replace_range(value_start..value_end, &HLS_SEGMENT_SECONDS.to_string());
        }
    }
    playlist.into_bytes()
}

/// An unfinished on-demand playlist has no ENDLIST yet. Generic HLS readers
/// otherwise treat it as live and choose a later segment before the application
/// can apply its VOD position. Tell existing readers to start at the first
/// available segment; explicit client start positions still take precedence.
/// This is a media-playlist hint, not a change to the control/session protocol.
pub(super) fn vod_start_playlist(bytes: Vec<u8>) -> Vec<u8> {
    let mut playlist = match String::from_utf8(bytes) {
        Ok(playlist) => playlist,
        Err(error) => return error.into_bytes(),
    };
    if !playlist.starts_with("#EXTM3U")
        || !playlist.lines().any(|line| line.starts_with("#EXTINF:"))
        || playlist
            .lines()
            .any(|line| line.starts_with("#EXT-X-START:"))
    {
        return playlist.into_bytes();
    }
    if let Some(header_end) = playlist.find('\n') {
        let hint = if playlist[..header_end].ends_with('\r') {
            "#EXT-X-START:TIME-OFFSET=0,PRECISE=YES\r\n"
        } else {
            "#EXT-X-START:TIME-OFFSET=0,PRECISE=YES\n"
        };
        playlist.insert_str(header_end + 1, hint);
    }
    playlist.into_bytes()
}

#[cfg(test)]
mod startup_hint_tests {
    use super::vod_start_playlist;

    #[test]
    fn unfinished_vod_starts_at_the_first_segment_without_waiting_for_eof() {
        let playlist = b"#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-TARGETDURATION:2\n#EXTINF:1,\nsegment-000000000.ts\n".to_vec();
        let result = vod_start_playlist(playlist);
        let text = std::str::from_utf8(&result).unwrap();
        assert!(text.starts_with("#EXTM3U\n#EXT-X-START:TIME-OFFSET=0,PRECISE=YES\n"));
        assert!(!text.contains("#EXT-X-ENDLIST"));
        assert_eq!(vod_start_playlist(result.clone()), result);
    }

    #[test]
    fn master_playlists_and_explicit_start_hints_are_preserved() {
        for playlist in [
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000000\nindex.m3u8\n",
            "#EXTM3U\n#EXT-X-START:TIME-OFFSET=12\n#EXTINF:2,\nsegment-000000000.ts\n",
        ] {
            assert_eq!(
                vod_start_playlist(playlist.as_bytes().to_vec()),
                playlist.as_bytes()
            );
        }
    }
}

pub(super) fn media_type(file: &str) -> Option<&'static str> {
    if file == "init.mp4" {
        return Some("video/mp4");
    }
    if matches!(file, "index.m3u8" | "master.m3u8" | "index_vtt.m3u8") {
        return Some("application/vnd.apple.mpegurl");
    }
    // Original containers are handed over whole; the WebCodecs client demuxes
    // them, so a neutral type is both accurate and preferable to a guess.
    if file.starts_with("source.") {
        return Some(match file.rsplit('.').next() {
            Some("mp4") => "video/mp4",
            Some("mov") => "video/quicktime",
            Some("mkv") => "video/x-matroska",
            Some("webm") => "video/webm",
            Some("ts") => "video/mp2t",
            _ => "application/octet-stream",
        });
    }
    if let Some(digits) = file
        .strip_prefix("index")
        .and_then(|f| f.strip_suffix(".vtt"))
    {
        return (!digits.is_empty()
            && digits.len() <= 16
            && digits.bytes().all(|b| b.is_ascii_digit()))
        .then_some("text/vtt");
    }
    let digits = file
        .strip_prefix("segment-")?
        .strip_suffix(".ts")
        .or_else(|| file.strip_prefix("segment-")?.strip_suffix(".m4s"))?;
    if digits.len() >= 9 && digits.len() <= 16 && digits.bytes().all(|b| b.is_ascii_digit()) {
        Some(if file.ends_with(".m4s") {
            "video/iso.segment"
        } else {
            "video/mp2t"
        })
    } else {
        None
    }
}

// HLS list_size bounds completed segments, NOT an open segment waiting for a
// keyframe. Poll even while leases are refreshed. These are watchdog limits,
// not hard disk quotas: deploy the media root on a quota-limited filesystem.
pub(super) async fn cache_safe(dir: &std::path::Path, require_progress: bool) -> bool {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return false;
    };
    let mut bytes = 0u64;
    let mut count = 0usize;
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(_) => return false,
        };
        count += 1;
        let Ok(meta) = entry.metadata().await else {
            continue;
        }; // deletion race
        bytes = bytes.saturating_add(meta.len());
        // The configured 120s window plus 60s delete grace can retain up to
        // 180 two-second AV/VTT files; leave a small margin for playlists.
        if count > 256 || meta.len() > 32 * 1024 * 1024 || bytes > 384 * 1024 * 1024 {
            tracing::warn!("Playback cache watchdog limit exceeded");
            return false;
        }
    }
    if require_progress {
        let fresh = tokio::fs::metadata(dir.join("index.m3u8"))
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age <= Duration::from_secs(20));
        if !fresh {
            tracing::warn!("Playback playlist stalled; closing session");
            return false;
        }
    }
    true
}

pub(super) async fn playback_ready(
    dir: &std::path::Path,
    subtitle: Option<&SelectedSubtitle>,
) -> bool {
    if !playlist_ready(dir).await {
        return false;
    }
    let Some(subtitle) = subtitle else {
        return true;
    };
    let Ok(playlist) = tokio::fs::read_to_string(dir.join("index_vtt.m3u8")).await else {
        return false;
    };
    let mut caption_ready = false;
    for file in playlist
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        if media_type(file) == Some("text/vtt")
            && tokio::fs::metadata(dir.join(file))
                .await
                .is_ok_and(|m| m.len() > 0)
        {
            caption_ready = true;
            break;
        }
    }
    if !caption_ready {
        return false;
    }
    subtitle_master(dir, subtitle).await
}

/// Readers reload an unfinished playlist at its advertised target duration,
/// including VOD still being produced. A shorter initial buffer can starve
/// before that reload. VOD needs one full interval. Live needs a second to cover
/// the upstream and replay HLS readers' independent reload cycles.
/// Completed short streams need no additional buffering.
pub(super) struct PublishedBuffer {
    pub(super) published_at: Option<std::time::SystemTime>,
}

pub(super) async fn startup_buffer(dir: &std::path::Path, live: bool) -> Option<PublishedBuffer> {
    use tokio::io::AsyncReadExt;
    // Read and stat the same published inode. A producer can replace the path
    // again before an external sampler sees it; that newer mtime would hide
    // the actual readiness interval.
    let mut file = tokio::fs::File::open(dir.join("index.m3u8")).await.ok()?;
    let published_at = file.metadata().await.ok().and_then(|m| m.modified().ok());
    let mut playlist = String::new();
    file.read_to_string(&mut playlist).await.ok()?;
    if playlist.lines().any(|line| line == "#EXT-X-ENDLIST") {
        return Some(PublishedBuffer { published_at });
    }
    let target = playlist
        .lines()
        .find_map(|line| {
            line.strip_prefix("#EXT-X-TARGETDURATION:")?
                .parse::<f64>()
                .ok()
        })
        .filter(|n| n.is_finite() && *n > 0.0);
    let Some(target) = target else {
        return None;
    };
    // The delivery path advertises at least the steady-state target, even
    // while FFmpeg's one-second initial segments still advertise one second.
    let target = target.max(f64::from(HLS_SEGMENT_SECONDS));
    let mut pending = 0.0;
    let mut duration = 0.0;
    for line in playlist.lines() {
        if let Some(value) = line.strip_prefix("#EXTINF:") {
            pending = value
                .split(',')
                .next()
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|n| n.is_finite() && *n > 0.0)
                .unwrap_or(0.0);
        } else if !line.is_empty() && !line.starts_with('#') {
            if !matches!(media_type(line), Some("video/mp2t" | "video/iso.segment"))
                || !tokio::fs::metadata(dir.join(line))
                    .await
                    .is_ok_and(|m| m.len() > 0)
            {
                return None;
            }
            duration += pending;
            pending = 0.0;
        }
    }
    (duration + 0.000001 >= target * if live { 2.0 } else { 1.0 })
        .then_some(PublishedBuffer { published_at })
}

#[cfg(test)]
async fn startup_buffer_ready(dir: &std::path::Path, live: bool) -> bool {
    startup_buffer(dir, live).await.is_some()
}

#[cfg(test)]
mod live_buffer_tests {
    use super::*;

    #[test]
    fn first_fragment_contains_the_selected_delayed_audio_without_using_other_tracks() {
        let probe: Probe = serde_json::from_value(serde_json::json!({"streams":[
            {"index":0,"codec_type":"video","codec_name":"h264","start_time":"1.483422"},
            {"index":1,"codec_type":"audio","codec_name":"aac","start_time":"2.629089"},
            {"index":2,"codec_type":"audio","codec_name":"aac","start_time":"1.400000"},
            {"index":3,"codec_type":"audio","codec_name":"aac","start_time":"999.0"}
        ]}))
        .unwrap();
        assert_eq!(initial_segment_seconds(&probe, Some(1)), 3);
        assert_eq!(initial_segment_seconds(&probe, Some(2)), 1);
        assert_eq!(initial_segment_seconds(&probe, None), 1);
        assert_eq!(initial_segment_seconds(&probe, Some(3)), 30);
    }

    #[tokio::test]
    async fn live_readiness_requires_published_reload_buffer_but_accepts_short_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n".to_owned();
        for i in 0..3 {
            playlist.push_str(&format!("#EXTINF:2.002,\nsegment-{i:09}.ts\n"));
            tokio::fs::write(dir.path().join("index.m3u8"), &playlist)
                .await
                .unwrap();
            assert!(!startup_buffer_ready(dir.path(), true).await);
            tokio::fs::write(dir.path().join(format!("segment-{i:09}.ts")), b"media")
                .await
                .unwrap();
            assert_eq!(startup_buffer_ready(dir.path(), true).await, i >= 1);
        }
        tokio::fs::write(dir.path().join("index.m3u8"), "#EXTM3U\n#EXT-X-ENDLIST\n")
            .await
            .unwrap();
        assert!(startup_buffer_ready(dir.path(), true).await);
    }

    #[tokio::test]
    async fn one_second_initial_segments_use_the_delivered_two_second_target() {
        let dir = tempfile::tempdir().unwrap();
        let mut playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n".to_owned();
        for i in 0..6 {
            playlist.push_str(&format!("#EXTINF:1,\nsegment-{i:09}.ts\n"));
            tokio::fs::write(dir.path().join(format!("segment-{i:09}.ts")), b"media")
                .await
                .unwrap();
            tokio::fs::write(dir.path().join("index.m3u8"), &playlist)
                .await
                .unwrap();
            assert_eq!(startup_buffer_ready(dir.path(), true).await, i >= 3);
        }
    }

    #[tokio::test]
    async fn unfinished_vod_covers_one_reload_without_waiting_for_live_holdback() {
        let dir = tempfile::tempdir().unwrap();
        let mut playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n".to_owned();
        for i in 0..2 {
            playlist.push_str(&format!("#EXTINF:1,\nsegment-{i:09}.ts\n"));
            tokio::fs::write(dir.path().join(format!("segment-{i:09}.ts")), b"media")
                .await
                .unwrap();
            tokio::fs::write(dir.path().join("index.m3u8"), &playlist)
                .await
                .unwrap();
            assert_eq!(startup_buffer_ready(dir.path(), false).await, i == 1);
            assert!(!startup_buffer_ready(dir.path(), true).await);
        }
    }
}

pub(super) async fn shared_caption_ready(
    dir: &std::path::Path,
    subtitle: Option<&SelectedSubtitle>,
) -> bool {
    if !playlist_ready(dir).await {
        return false;
    }
    match subtitle {
        Some(subtitle) => subtitle_master(dir, subtitle).await,
        None => true,
    }
}

async fn subtitle_master(dir: &std::path::Path, subtitle: &SelectedSubtitle) -> bool {
    let Ok(av) = tokio::fs::read_to_string(dir.join("index.m3u8")).await else {
        return false;
    };
    let Some(segment) = av
        .lines()
        .find(|line| media_type(line) == Some("video/mp2t"))
    else {
        return false;
    };
    let Ok(info) = tokio::fs::metadata(dir.join(segment)).await else {
        return false;
    };
    let duration = av
        .lines()
        .find_map(|line| line.strip_prefix("#EXTINF:"))
        .and_then(|value| value.split(',').next())
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(HLS_SEGMENT_SECONDS as f64);
    let bandwidth =
        ((info.len() as f64 * 8.0 / duration * 1.25).ceil() as u64).clamp(64000, 128000000);
    let language = subtitle.language.as_deref().unwrap_or("und");
    // FFmpeg can omit STREAM-INF until a sparse subtitle's first packet arrives.
    // Publish our one-variant master from already ready local media instead. Its
    // bandwidth is an initial measured estimate, not a source/codec assertion.
    let master = format!(
        "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"Subtitles\",DEFAULT=YES,AUTOSELECT=YES,LANGUAGE=\"{language}\",URI=\"index_vtt.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH={bandwidth},SUBTITLES=\"subs\"\nindex.m3u8\n"
    );
    if tokio::fs::write(dir.join("master.m3u8.tmp"), master)
        .await
        .is_err()
    {
        return false;
    }
    tokio::fs::rename(dir.join("master.m3u8.tmp"), dir.join("master.m3u8"))
        .await
        .is_ok()
}

async fn playlist_ready(dir: &std::path::Path) -> bool {
    let Ok(playlist) = tokio::fs::read_to_string(dir.join("index.m3u8")).await else {
        return false;
    };
    if playlist.contains("#EXT-X-MAP:")
        && !tokio::fs::metadata(dir.join("init.mp4"))
            .await
            .is_ok_and(|m| m.len() > 0)
    {
        return false;
    }
    for line in playlist
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
    {
        if matches!(media_type(line), Some("video/mp2t" | "video/iso.segment"))
            && tokio::fs::metadata(dir.join(line))
                .await
                .is_ok_and(|m| m.len() > 0)
        {
            return true;
        }
    }
    false
}
