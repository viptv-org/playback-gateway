//! Bounded decoded cue carryover. Only the shared ingest reads the provider;
//! caption renditions read this cache and their own generated AV segments.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

const BLOCK_BYTES: usize = 32 * 1024;
const CACHE_BYTES: usize = 1024 * 1024;
const CACHE_CUES: usize = 8192;
const READER_GRACE_MICROS: u64 = 180 * 1_000_000;

struct Cue {
    id: u64,
    track: u32,
    start: u64,
    end: u64,
    text: String,
}
#[derive(Default)]
struct Cues {
    entries: Vec<Cue>,
    bytes: usize,
    next_id: u64,
    latest: u64,
}
#[derive(Default)]
pub(super) struct Captions {
    cues: std::sync::Mutex<Cues>,
    failed: AtomicBool,
}
impl Captions {
    pub(super) fn healthy(&self) -> bool {
        !self.failed.load(Ordering::Acquire)
    }
    fn add(&self, track: u32, block: &[u8]) -> Result<(), ()> {
        let text = std::str::from_utf8(block).map_err(|_| ())?;
        if text.trim().is_empty() || text.starts_with("WEBVTT") {
            return Ok(());
        }
        let mut lines = text.lines();
        let timing = lines.next().ok_or(())?;
        let timing = if timing.contains(" --> ") {
            timing
        } else {
            lines.next().ok_or(())?
        };
        let (start, end) = timing.split_once(" --> ").ok_or(())?;
        let start = timestamp(start).ok_or(())?;
        let end = timestamp(end.split_whitespace().next().ok_or(())?).ok_or(())?;
        if end <= start {
            return Err(());
        }
        let text = lines.collect::<Vec<_>>().join("\n");
        let mut cues = self.cues.lock().map_err(|_| ())?;
        cues.latest = cues.latest.max(start);
        let oldest = cues.latest.saturating_sub(READER_GRACE_MICROS);
        cues.entries.retain(|cue| cue.end >= oldest);
        cues.bytes = cues.entries.iter().map(|cue| cue.text.len() + 128).sum();
        if cues.entries.iter().any(|cue| {
            cue.track == track && cue.start == start && cue.end == end && cue.text == text
        }) {
            return Ok(());
        }
        if cues.entries.len() >= CACHE_CUES || cues.bytes + text.len() + 128 > CACHE_BYTES {
            return Err(());
        }
        let id = cues.next_id;
        cues.next_id = cues.next_id.checked_add(1).ok_or(())?;
        cues.bytes += text.len() + 128;
        cues.entries.push(Cue {
            id,
            track,
            start,
            end,
            text,
        });
        Ok(())
    }
    pub(super) fn render(&self, track: u32, start: u64, end: u64) -> Result<Vec<u8>, String> {
        if !self.healthy() {
            return Err("Caption replay unavailable".into());
        }
        let cues = self.cues.lock().map_err(|_| "Caption replay unavailable")?;
        let mut text = "WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:0\n\n".to_owned();
        for cue in &cues.entries {
            if cue.track == track && cue.start < end && cue.end > start {
                text.push_str(&format!(
                    "cue-{}\n{} --> {}\n{}\n\n",
                    cue.id,
                    clock(cue.start),
                    clock(cue.end),
                    cue.text
                ));
            }
        }
        Ok(text.into_bytes())
    }
}
fn timestamp(text: &str) -> Option<u64> {
    let mut parts = text.rsplit(':');
    let seconds = parts.next()?;
    let minutes: u64 = parts.next()?.parse().ok()?;
    let hours: u64 = parts.next().map(str::parse).transpose().ok()?.unwrap_or(0);
    if parts.next().is_some() || minutes >= 60 {
        return None;
    }
    let (seconds, millis) = seconds.split_once('.')?;
    let seconds: u64 = seconds.parse().ok()?;
    if seconds >= 60 || millis.len() != 3 {
        return None;
    }
    let millis: u64 = millis.parse().ok()?;
    hours
        .checked_mul(3600)?
        .checked_add(minutes * 60 + seconds)?
        .checked_mul(1_000_000)?
        .checked_add(millis * 1000)
}
fn clock(micros: u64) -> String {
    let millis = micros / 1000;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        millis / 3_600_000,
        millis / 60_000 % 60,
        millis / 1000 % 60,
        millis % 1000
    )
}

/// The descriptor writers are inherited only by our own FFmpeg child. No path,
/// network listener or extra provider reader is needed for subtitle capture.
#[cfg(target_os = "linux")]
pub(super) struct Pipes {
    writers: Vec<std::os::unix::net::UnixStream>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
#[cfg(target_os = "linux")]
impl Pipes {
    pub(super) fn attach(
        cmd: &mut Command,
        probe: &Probe,
        captions: Arc<Captions>,
    ) -> Result<Self, String> {
        use std::os::fd::AsRawFd;
        let mut pipes = Self {
            writers: Vec::new(),
            tasks: Vec::new(),
        };
        for stream in probe
            .streams
            .iter()
            .filter(|s| s.codec_type.as_deref() == Some("subtitle"))
        {
            let track = stream.index.ok_or("Caption track unavailable")?;
            let (reader, writer) =
                std::os::unix::net::UnixStream::pair().map_err(|_| "Caption replay unavailable")?;
            reader
                .set_nonblocking(true)
                .map_err(|_| "Caption replay unavailable")?;
            let reader = tokio::net::UnixStream::from_std(reader)
                .map_err(|_| "Caption replay unavailable")?;
            cmd.args([
                "-map",
                &format!("0:{track}"),
                "-c:s",
                "webvtt",
                "-flush_packets",
                "1",
                "-f",
                "webvtt",
                &format!("pipe:{}", writer.as_raw_fd()),
            ]);
            let cache = captions.clone();
            pipes.tasks.push(tokio::spawn(async move {
                if capture(reader, track, &cache).await.is_err() {
                    cache.failed.store(true, Ordering::Release);
                }
            }));
            pipes.writers.push(writer);
        }
        let descriptors: Vec<_> = pipes.writers.iter().map(AsRawFd::as_raw_fd).collect();
        // SAFETY: this child hook only calls fcntl on live owned descriptors and
        // creates raw OS errors. It performs no allocation/locking after fork.
        unsafe {
            cmd.pre_exec(move || {
                for &fd in &descriptors {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        Ok(pipes)
    }
    pub(super) fn spawned(&mut self) {
        self.writers.clear();
    }
}
#[cfg(target_os = "linux")]
impl Drop for Pipes {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
#[cfg(target_os = "linux")]
async fn capture(
    mut reader: tokio::net::UnixStream,
    track: u32,
    cache: &Captions,
) -> Result<(), ()> {
    use tokio::io::AsyncReadExt;
    let mut buffer = Vec::new();
    let mut bytes = [0u8; 4096];
    loop {
        let count = reader.read(&mut bytes).await.map_err(|_| ())?;
        if count == 0 {
            return Err(());
        }
        buffer.extend_from_slice(&bytes[..count]);
        while let Some(end) = buffer.windows(2).position(|pair| pair == b"\n\n") {
            if end > BLOCK_BYTES {
                return Err(());
            }
            cache.add(track, &buffer[..end])?;
            buffer.drain(..end + 2);
        }
        if buffer.len() > BLOCK_BYTES {
            return Err(());
        }
    }
}

impl PlaybackManager {
    /// Probe only an authorized gateway-owned completed AV segment. Admission,
    /// bounded output and the existing child guard also apply to media requests.
    pub(super) async fn caption_segment(
        &self,
        path: &std::path::Path,
        captions: &Captions,
        track: u32,
    ) -> Result<Vec<u8>, String> {
        let permit = self
            .preparation_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "Caption replay unavailable")?;
        let permits = Arc::new(InputPermits {
            _playback: permit.into(),
            _provider: None,
        });
        let mut cmd = Command::new(&self.config.ffprobe);
        contain_media_process(&mut cmd);
        cmd.kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .args([
                "-v",
                "error",
                "-protocol_whitelist",
                "file",
                "-select_streams",
                "v:0",
                "-show_packets",
                "-show_entries",
                "packet=pts_time,duration_time",
                "-of",
                "json",
                "-i",
            ])
            .arg(path);
        let child = cmd.spawn().map_err(|_| "Caption replay unavailable")?;
        let mut guard = ProbeChild {
            child: Some(child),
            permits: Some(permits),
            cleanup_tasks: self.cleanup_tasks.clone(),
        };
        let stdout = guard
            .child
            .as_mut()
            .and_then(|child| child.stdout.take())
            .ok_or("Caption replay unavailable")?;
        let result = timeout(Duration::from_secs(3), async {
            let bytes = probe_output(stdout, 256 * 1024)
                .await
                .map_err(|_| "Caption replay unavailable")?;
            let status = guard
                .child
                .as_mut()
                .unwrap()
                .wait()
                .await
                .map_err(|_| "Caption replay unavailable")?;
            if !status.success() {
                return Err("Caption replay unavailable");
            }
            let facts: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|_| "Caption replay unavailable")?;
            let packets = facts["packets"]
                .as_array()
                .ok_or("Caption replay unavailable")?;
            let mut start = f64::INFINITY;
            let mut end = 0.0f64;
            for packet in packets {
                let time = packet["pts_time"]
                    .as_str()
                    .and_then(|t| t.parse::<f64>().ok())
                    .filter(|t| t.is_finite() && *t >= 0.0)
                    .ok_or("Caption replay unavailable")?;
                let duration = packet["duration_time"]
                    .as_str()
                    .and_then(|t| t.parse::<f64>().ok())
                    .filter(|t| t.is_finite() && *t > 0.0)
                    .ok_or("Caption replay unavailable")?;
                start = start.min(time);
                end = end.max(time + duration);
            }
            if !start.is_finite() || end <= start {
                return Err("Caption replay unavailable");
            }
            Ok((
                (start * 1_000_000.0).round() as u64,
                (end * 1_000_000.0).round() as u64,
            ))
        })
        .await;
        guard.reap().await;
        let (start, end) = result
            .map_err(|_| "Caption replay unavailable")?
            .map_err(str::to_owned)?;
        captions.render(track, start, end)
    }
}
