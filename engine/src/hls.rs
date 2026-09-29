use super::*;

// A one-second initial segment reduces player startup; steady-state stays at
// two seconds. Qualified browser copy uses continuous fragmented MP4. Legacy
// copied HLS retains split-by-time during the independent browser rollout.
pub(super) const HLS_INITIAL_SEGMENT_SECONDS: u32 = 1;
pub(super) const HLS_SEGMENT_SECONDS: u32 = 2;
pub(super) const HLS_WINDOW_SECONDS: u32 = 120;
pub(super) const HLS_DELETE_GRACE_SECONDS: u32 = 60;

// An on-demand encoder runs far faster than realtime (a remux or a 1080p
// ultrafast encode measured 10-50x), while the rolling playlist lists only
// HLS_WINDOW_SECONDS and deletes files HLS_DELETE_GRACE_SECONDS later. Left
// alone, the window slid past a realtime viewer within seconds and the
// segments it still needed were deleted before it fetched them: a web player
// then skipped, stuttered or jumped to the playlist edge. Like Plex's
// transcoder throttle buffer, suspend the encoder once it is this far past
// the furthest segment a viewer fetched, and resume it as the viewer catches
// up. The pause point sits a third into the advertised window: the encoder
// is only checked every HLS_THROTTLE_TICK, and the remaining two thirds absorb
// what it produces in between (up to ~390x realtime during the one-second
// segments) plus what its socket buffered while suspended, so the viewer's
// next segment stays listed; files also outlive the listing by the grace.
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
    pub(super) fn apply_playlist(&mut self, child: &Child, playlist: &str) {
        let mut duration = 0.0;
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
                duration = 0.0;
            }
        }
        if let Some(last) = self.last_segment {
            self.timeline
                .retain(|index, _| *index >= last.saturating_sub(256));
        }
        let ahead = self.produced_seconds - self.consumed_seconds;
        self.apply_ahead(child, ahead);
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
    if media_type(file) != Some("video/mp2t") {
        return None;
    }
    file.strip_prefix("segment-")?
        .strip_suffix(".ts")?
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
    Ok((
        caps.max_width / 2 * 2,
        caps.max_height / 2 * 2,
    ))
}
pub(super) fn hdr_size(width: u32, height: u32) -> String {
    // Fit both axes, including portrait, without upscaling; round down to even pixels.
    format!("w='trunc(min(iw,min({width},iw*{height}/ih))/2)*2':h='trunc(min(ih,min({height},ih*{width}/iw))/2)*2'")
}
pub(super) fn hdr_filter(width: u32, height: u32) -> String {
    // zimg resizes BEFORE transfer conversion when both are in one zscale. Keep
    // explicit PQ/HLG -> float-linear RGB first to avoid averaging encoded light.
    // Fuse bounded linear-light resize with the linear BT709 primaries transform;
    // the expensive tone mapper and remaining conversions then run at target size.
    // Only FFmpeg 5.1-compatible options; preserve SDR tags and drop HDR side data.
    format!("zscale=transfer=linear:npl=100,format=gbrpf32le,zscale={}:filter=bilinear:primaries=bt709,tonemap=tonemap=mobius:desat=2,zscale=transfer=bt709:matrix=bt709:range=limited,format=yuv420p,sidedata=mode=delete,setsar=1", hdr_size(width, height))
}
pub(super) fn scale_filter(width: u32, height: u32) -> String {
    format!("scale=w='min(iw,{width})':h='min(ih,{height})':force_original_aspect_ratio=decrease:force_divisible_by=2:flags=fast_bilinear,setsar=1")
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
    let master = format!("#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"Subtitles\",DEFAULT=YES,AUTOSELECT=YES,LANGUAGE=\"{language}\",URI=\"index_vtt.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH={bandwidth},SUBTITLES=\"subs\"\nindex.m3u8\n");
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
