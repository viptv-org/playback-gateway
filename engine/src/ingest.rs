//! Private live replay. Only the ingest process has network protocols enabled;
//! output readers can open generated local files, never upstream child URLs.
use super::*;
use std::{collections::BTreeMap, sync::Weak};

#[cfg(test)]
const REPLAY_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplayKind {
    Ts,
    Matroska,
}
impl ReplayKind {
    fn for_probe(probe: &Probe) -> Self {
        if probe
            .streams
            .iter()
            .find(|s| s.is_video())
            .is_some_and(|s| s.codec_name.as_deref() == Some("ffv1"))
            || probe.streams.iter().any(|s| {
                s.codec_type.as_deref() == Some("audio") && s.codec_name.as_deref() == Some("flac")
            })
        {
            Self::Matroska
        } else {
            Self::Ts
        }
    }
    fn extensions(self) -> &'static str {
        match self {
            Self::Ts => "ts,m3u8",
            Self::Matroska => "mkv,m3u8",
        }
    }
}

fn matroska_segment_number(file: &str) -> Option<u64> {
    let digits = file.strip_prefix("segment-")?.strip_suffix(".mkv")?;
    (digits.len() >= 9 && digits.len() <= 16 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

// Orphan cleanup recognizes private generated files without advertising them
// as viewer-facing media resources.
pub(super) fn generated_replay_file(file: &str) -> bool {
    matroska_segment_number(file).is_some()
}

// The segment muxer limits its playlist, not files. Retain three evicted
// segments for already-reading outputs; never delete an unlisted/open new file.
async fn matroska_window(dir: &std::path::Path) -> Result<bool, ()> {
    let text = match tokio::fs::read_to_string(dir.join("index.m3u8")).await {
        Ok(text) if text.len() <= 16 * 1024 => text,
        Ok(_) => return Err(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(()),
    };
    let names: Vec<_> = text
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    if !text.starts_with("#EXTM3U") {
        return Err(());
    }
    if names.is_empty() {
        return Ok(false);
    }
    if names.len() > 12 {
        return Err(());
    }
    let numbers: Vec<_> = names
        .iter()
        .map(|name| matroska_segment_number(name).ok_or(()))
        .collect::<Result<_, _>>()?;
    if numbers.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(());
    }
    let keep = numbers[0].saturating_sub(3);
    let mut files = tokio::fs::read_dir(dir).await.map_err(|_| ())?;
    while let Some(file) = files.next_entry().await.map_err(|_| ())? {
        let name = file.file_name();
        if name
            .to_str()
            .and_then(matroska_segment_number)
            .is_some_and(|number| number < keep)
        {
            match tokio::fs::remove_file(file.path()).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(()),
            }
        }
    }
    for name in names {
        if !tokio::fs::metadata(dir.join(name))
            .await
            .is_ok_and(|meta| meta.is_file() && meta.len() > 0)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn within_budget(dir: &std::path::Path, limit: u64) -> bool {
    let Ok(mut files) = tokio::fs::read_dir(dir).await else {
        return false;
    };
    let mut bytes = 0u64;
    loop {
        match files.next_entry().await {
            Ok(Some(file)) => {
                let metadata = match file.metadata().await {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(_) => return false,
                };
                bytes = bytes.saturating_add(metadata.len());
                if bytes > limit {
                    return false;
                }
            }
            Ok(None) => return true,
            Err(_) => return false,
        }
    }
}

pub(super) struct SharedIngest {
    pub(super) playlist: PathBuf,
    indices: BTreeMap<u32, u32>,
    kind: ReplayKind,
    running: Arc<std::sync::atomic::AtomicBool>,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    cleanup_tasks: CleanupTasks,
}
impl SharedIngest {
    pub(super) fn extensions(&self) -> &'static str {
        self.kind.extensions()
    }
    pub(super) fn index(&self, original: u32) -> Result<u32, String> {
        self.indices
            .get(&original)
            .copied()
            .ok_or_else(|| "Replay track unavailable".into())
    }
    pub(super) fn running(&self) -> bool {
        self.running.load(std::sync::atomic::Ordering::Acquire)
    }
}
impl Drop for SharedIngest {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(task) = self.task.take() {
            let mut tasks = self.cleanup_tasks.lock().unwrap_or_else(|e| e.into_inner());
            tasks.retain(|task| !task.is_finished());
            tasks.push(task);
        }
    }
}

/// Replay retains the first real video and every audio track in a deterministic
/// order. Subtitles currently require the original source pipeline; they must
/// not be silently discarded or have their original indices guessed.
pub(super) fn replay_indices(probe: &Probe) -> Option<BTreeMap<u32, u32>> {
    if probe
        .streams
        .iter()
        .any(|s| s.codec_type.as_deref() == Some("subtitle"))
    {
        return None;
    }
    let video = probe.streams.iter().find(|s| s.is_video())?;
    if !matches!(
        video.codec_name.as_deref(),
        Some("h264" | "hevc" | "mpeg2video" | "ffv1")
    ) {
        return None;
    }
    let mut indices = BTreeMap::from([(video.index?, 0)]);
    let mut audio: Vec<_> = probe
        .streams
        .iter()
        .filter(|s| s.codec_type.as_deref() == Some("audio"))
        .collect();
    if audio.len() > 32 {
        return None;
    }
    audio.sort_by_key(|s| s.index);
    for (ordinal, stream) in audio.into_iter().enumerate() {
        if !matches!(
            stream.codec_name.as_deref(),
            Some("aac" | "mp2" | "mp3" | "ac3" | "eac3" | "flac")
        ) {
            return None;
        }
        if indices.insert(stream.index?, ordinal as u32 + 1).is_some() {
            return None;
        }
    }
    Some(indices)
}

#[derive(Clone)]
pub(super) struct IngestEntry {
    ingest: Weak<SharedIngest>,
    terminated: Arc<std::sync::atomic::AtomicBool>,
}
pub(super) type IngestRegistry = Mutex<HashMap<[u8; 32], IngestEntry>>;
pub(super) type IngestFlights = Mutex<HashMap<[u8; 32], Weak<Mutex<()>>>>;

struct StartingIngest {
    session: Option<Session>,
    terminated: Arc<std::sync::atomic::AtomicBool>,
    cleanup_tasks: CleanupTasks,
}
impl Drop for StartingIngest {
    fn drop(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        let terminated = self.terminated.clone();
        let task = tokio::spawn(async move {
            session.cleanup().await;
            terminated.store(true, std::sync::atomic::Ordering::Release);
        });
        let mut tasks = self.cleanup_tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
    }
}

impl PlaybackManager {
    pub(super) async fn shared_ingest(
        &self,
        scope: &str,
        source: &str,
        headers: &str,
        probe: Arc<Probe>,
    ) -> Result<Arc<SharedIngest>, String> {
        let indices = replay_indices(&probe).ok_or("Source replay unsupported")?;
        let kind = ReplayKind::for_probe(&probe);
        let replay_max_bytes = self.replay_max_bytes;
        let mut digest = Sha256::new();
        for value in [scope, source, headers] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
        let key: [u8; 32] = digest.finalize().into();
        let flight = {
            let mut flights = self.ingest_flights.lock().await;
            flights.retain(|_, value| value.strong_count() > 0);
            if let Some(flight) = flights.get(&key).and_then(Weak::upgrade) {
                flight
            } else {
                let flight = Arc::new(Mutex::new(()));
                flights.insert(key, Arc::downgrade(&flight));
                flight
            }
        };
        let _flight = flight.lock().await;
        let entry = self.ingests.lock().await.get(&key).cloned();
        if let Some(existing) = entry.as_ref().and_then(|entry| entry.ingest.upgrade()) {
            if existing.indices != indices || existing.kind != kind || !existing.running() {
                // A dead generation remains owned until every output is reaped.
                return Err("Input cleanup pending".into());
            }
            return Ok(existing);
        }
        if entry.is_some_and(|entry| !entry.terminated.load(std::sync::atomic::Ordering::Acquire)) {
            return Err("Input cleanup pending".into());
        }
        let permit = self
            .ingest_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| MSG_PLAYBACK_CAPACITY)?;
        let permits = Arc::new(InputPermits {
            _playback: permit.into(),
            _provider: None,
        });
        let root = tokio::fs::canonicalize(&self.config.root)
            .await
            .map_err(|_| "Media storage unavailable")?;
        let dir = root.join(Uuid::new_v4().to_string());
        tokio::fs::create_dir(&dir)
            .await
            .map_err(|_| "Media storage unavailable")?;
        let playlist = dir.join("index.m3u8");
        let session = Session {
            stream: None,
            _source_probe: Some(probe.clone()),
            direct: None,
            ingest: None,
            capability: String::new(),
            dir: dir.clone(),
            child: None,
            touched: Instant::now(),
            stable_target_duration: false,
            target_duration: None,
            supervised_live: true,
            throttle: Throttle::default(),
            permits,
            cleanup_tasks: self.cleanup_tasks.clone(),
        };
        let terminated = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut startup = StartingIngest {
            session: Some(session),
            terminated: terminated.clone(),
            cleanup_tasks: self.cleanup_tasks.clone(),
        };
        {
            let mut ingests = self.ingests.lock().await;
            ingests.retain(|_, entry| {
                entry.ingest.strong_count() > 0
                    || !entry.terminated.load(std::sync::atomic::Ordering::Acquire)
            });
            ingests.insert(
                key,
                IngestEntry {
                    ingest: Weak::new(),
                    terminated: terminated.clone(),
                },
            );
        }
        let session = startup.session.as_mut().expect("startup session");
        let mut cmd = Command::new(&self.config.ffmpeg);
        cmd.kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);
        input_args(&mut cmd, headers);
        cmd.arg("-i").arg(source);
        let mut ordered: Vec<_> = indices.iter().collect();
        ordered.sort_by_key(|(_, replay)| **replay);
        for (original, _) in ordered {
            cmd.arg("-map").arg(format!("0:{original}"));
        }
        cmd.args([
            "-c",
            "copy",
            "-sn",
            "-dn",
            "-map_metadata",
            "-1",
            "-map_chapters",
            "-1",
        ]);
        match kind {
            ReplayKind::Ts => {
                cmd.args([
                    "-f",
                    "hls",
                    "-hls_time",
                    "2",
                    "-hls_list_size",
                    "12",
                    "-hls_delete_threshold",
                    "3",
                    "-hls_flags",
                    "delete_segments+temp_file+independent_segments",
                    "-hls_segment_filename",
                ])
                .arg(dir.join("segment-%09d.ts"))
                .arg(&playlist);
            }
            ReplayKind::Matroska => {
                cmd.args([
                    "-f",
                    "segment",
                    "-segment_format",
                    "matroska",
                    "-segment_time",
                    "2",
                    "-segment_list_type",
                    "m3u8",
                    "-segment_list_size",
                    "12",
                    "-segment_list_flags",
                    "+live",
                    "-segment_list",
                ])
                .arg(&playlist)
                .arg(dir.join("segment-%09d.mkv"));
            }
        }
        session.child = Some(cmd.spawn().map_err(|_| "Playback engine unavailable")?);
        let ready = timeout(Duration::from_secs(20), async {
            loop {
                if !matches!(session.child.as_mut().map(|c| c.try_wait()), Some(Ok(None))) {
                    return false;
                }
                if !within_budget(&dir, replay_max_bytes).await {
                    return false;
                }
                let ready = match kind {
                    ReplayKind::Ts => playback_ready(&dir, None).await,
                    ReplayKind::Matroska => match matroska_window(&dir).await {
                        Ok(ready) => ready,
                        Err(()) => return false,
                    },
                };
                if ready {
                    return true;
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap_or(false);
        if !ready {
            return Err("Source replay unavailable".into());
        }
        // Inspect the generated replay before trusting original-to-local indices.
        // This is a local read, with provider protocols and headers absent.
        let mut inspect = Command::new(&self.config.ffprobe);
        contain_media_process(&mut inspect);
        inspect
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        inspect
            .args([
                "-v",
                "error",
                "-protocol_whitelist",
                "file",
                "-allowed_extensions",
                kind.extensions(),
                "-show_streams",
                "-of",
                "json",
                "-i",
            ])
            .arg(&playlist);
        let child = inspect
            .spawn()
            .map_err(|_| "Replay inspection unavailable")?;
        let mut guard = ProbeChild {
            child: Some(child),
            permits: Some(session.permits.clone()),
            cleanup_tasks: self.cleanup_tasks.clone(),
        };
        let stdout = guard
            .child
            .as_mut()
            .and_then(|child| child.stdout.take())
            .ok_or("Replay inspection unavailable")?;
        let facts = timeout(Duration::from_secs(5), async {
            tokio::select! {
                result = probe_output(stdout, 1024 * 1024) => result,
                _ = async {
                    loop {
                        if kind == ReplayKind::Matroska && matroska_window(&dir).await.is_err() { break; }
                        if !within_budget(&dir,replay_max_bytes).await { break; }
                        sleep(Duration::from_millis(100)).await;
                    }
                } => Err(crate::probe::ProbeFailure::Oversized),
            }
        })
        .await;
        let success = timeout(
            Duration::from_secs(2),
            guard.child.as_mut().expect("inspection child").wait(),
        )
        .await;
        guard.reap().await;
        let replay: Probe = match facts {
            Ok(Ok(bytes)) => {
                serde_json::from_slice(&bytes).map_err(|_| "Invalid replay metadata")?
            }
            _ => return Err("Replay inspection unavailable".into()),
        };
        if !matches!(success, Ok(Ok(status)) if status.success())
            || !indices.iter().all(|(original, local)| {
                let source = probe.streams.iter().find(|s| s.index == Some(*original));
                let target = replay.streams.iter().find(|s| s.index == Some(*local));
                matches!((source, target), (Some(a), Some(b)) if
                    a.codec_type == b.codec_type && a.codec_name == b.codec_name
                    && a.width == b.width && a.height == b.height && a.channels == b.channels)
            })
        {
            return Err("Replay stream mapping changed".into());
        }
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let alive = running.clone();
        let (cancel, mut cancelled) = tokio::sync::oneshot::channel();
        let mut session = startup.session.take().expect("prepared ingest session");
        // Count bytes as well as segments: a hostile source with rare keyframes
        // must not grow a nominally time-bounded replay without a storage limit.
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut cancelled => break,
                    _ = sleep(Duration::from_millis(100)) => {}
                }
                if !matches!(session.child.as_mut().map(|c| c.try_wait()), Some(Ok(None))) {
                    break;
                }
                if kind == ReplayKind::Matroska && matroska_window(&session.dir).await.is_err() {
                    break;
                }
                if !within_budget(&session.dir, replay_max_bytes).await {
                    break;
                }
            }
            alive.store(false, std::sync::atomic::Ordering::Release);
            session.cleanup().await;
            terminated.store(true, std::sync::atomic::Ordering::Release);
        });
        let ingest = Arc::new(SharedIngest {
            playlist,
            indices,
            kind,
            running,
            cancel: Some(cancel),
            task: Some(task),
            cleanup_tasks: self.cleanup_tasks.clone(),
        });
        let mut ingests = self.ingests.lock().await;
        ingests.get_mut(&key).expect("registered ingest").ingest = Arc::downgrade(&ingest);
        ingests.retain(|_, entry| {
            entry.ingest.strong_count() > 0
                || !entry.terminated.load(std::sync::atomic::Ordering::Acquire)
        });
        Ok(ingest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe() -> Probe {
        serde_json::from_value(serde_json::json!({"streams": [
            {"index":9,"codec_type":"audio","codec_name":"aac"},
            {"index":5,"codec_type":"video","codec_name":"h264"},
            {"index":3,"codec_type":"audio","codec_name":"ac3"},
            {"index":12,"codec_type":"data","codec_name":"bin_data"}
        ]}))
        .unwrap()
    }

    #[test]
    fn sparse_original_indices_map_video_then_sorted_audio_without_product_fields() {
        assert_eq!(
            replay_indices(&probe()).unwrap(),
            BTreeMap::from([(5, 0), (3, 1), (9, 2)])
        );
    }

    #[test]
    fn mpeg2_video_uses_lossless_ts_replay_without_changing_decoder_eligibility() {
        let mut facts = probe();
        facts.streams[1].codec_name = Some("mpeg2video".into());
        assert_eq!(replay_indices(&facts), replay_indices(&probe()));
    }

    #[test]
    fn verified_ffv1_or_flac_select_private_matroska_with_the_same_sparse_indices() {
        let mut facts = probe();
        assert_eq!(ReplayKind::for_probe(&facts).extensions(), "ts,m3u8");
        facts.streams[1].codec_name = Some("ffv1".into());
        assert_eq!(ReplayKind::for_probe(&facts).extensions(), "mkv,m3u8");
        assert_eq!(replay_indices(&facts), replay_indices(&probe()));
        facts = probe();
        facts.streams[0].codec_name = Some("flac".into());
        assert_eq!(ReplayKind::for_probe(&facts).extensions(), "mkv,m3u8");
        assert_eq!(replay_indices(&facts), replay_indices(&probe()));
    }

    #[tokio::test]
    async fn matroska_cleanup_retains_list_grace_and_open_files_and_rejects_unowned_paths() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..20 {
            tokio::fs::write(
                root.path().join(format!("segment-{index:09}.mkv")),
                b"fixture",
            )
            .await
            .unwrap();
        }
        let manifest = format!(
            "#EXTM3U\n{}",
            (7..19)
                .map(|index| format!("segment-{index:09}.mkv\n"))
                .collect::<String>()
        );
        tokio::fs::write(root.path().join("index.m3u8"), &manifest)
            .await
            .unwrap();
        assert_eq!(matroska_window(root.path()).await, Ok(true));
        for index in 0..20 {
            assert_eq!(
                root.path().join(format!("segment-{index:09}.mkv")).exists(),
                index >= 4
            );
        }
        for bad in [
            "../unowned.mkv",
            "segment-000000019.mkv\nsegment-000000007.mkv",
            "https://fixture.invalid/private.mkv",
        ] {
            tokio::fs::write(root.path().join("index.m3u8"), format!("#EXTM3U\n{bad}\n"))
                .await
                .unwrap();
            assert_eq!(matroska_window(root.path()).await, Err(()));
            assert!(root.path().join("segment-000000004.mkv").exists());
        }
    }

    #[tokio::test]
    async fn matroska_logical_budget_counts_an_open_unlisted_segment() {
        let root = tempfile::tempdir().unwrap();
        let file = tokio::fs::File::create(root.path().join("segment-000000019.mkv"))
            .await
            .unwrap();
        file.set_len(REPLAY_BYTES + 1).await.unwrap();
        assert!(!within_budget(root.path(), REPLAY_BYTES).await);
        assert!(within_budget(root.path(), REPLAY_BYTES + 1).await);
    }

    #[tokio::test]
    async fn crashed_private_matroska_is_cleaned_but_foreign_files_remain() {
        let root = tempfile::tempdir().unwrap();
        let generated = root.path().join(Uuid::new_v4().to_string());
        let foreign = root.path().join(Uuid::new_v4().to_string());
        for dir in [&generated, &foreign] {
            tokio::fs::create_dir(dir).await.unwrap();
            for file in [
                "index.m3u8",
                "segment-000000000.mkv",
                "segment-000000001.mkv.tmp",
            ] {
                tokio::fs::write(dir.join(file), b"owned fixture")
                    .await
                    .unwrap();
            }
        }
        tokio::fs::write(foreign.join("notes.txt"), b"foreign fixture")
            .await
            .unwrap();
        lease::cleanup_orphans(root.path()).await.unwrap();
        assert!(!generated.exists());
        assert!(foreign.join("notes.txt").exists());
        assert!(foreign.join("segment-000000000.mkv").exists());
        assert!(
            media_type("segment-000000000.mkv").is_none(),
            "private cleanup must not enable public Matroska resources"
        );
    }

    #[test]
    fn subtitle_unknown_codec_missing_and_duplicate_index_require_original_pipeline() {
        let mut facts = probe();
        facts.streams[3].codec_type = Some("subtitle".into());
        assert!(replay_indices(&facts).is_none());
        facts = probe();
        facts.streams[0].codec_name = Some("opus".into());
        assert!(replay_indices(&facts).is_none());
        facts = probe();
        facts.streams[0].index = None;
        assert!(replay_indices(&facts).is_none());
        facts = probe();
        facts.streams[0].index = Some(5);
        assert!(replay_indices(&facts).is_none());
        facts = probe();
        for index in 100..133 {
            let mut stream = facts.streams[0].clone();
            stream.index = Some(index);
            facts.streams.push(stream);
        }
        assert!(replay_indices(&facts).is_none());
    }

    #[tokio::test]
    async fn replay_budget_counts_partial_segments_and_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        assert!(within_budget(root.path(), REPLAY_BYTES).await);
        let file = tokio::fs::File::create(root.path().join("segment-000000000.ts.tmp"))
            .await
            .unwrap();
        file.set_len(REPLAY_BYTES + 1).await.unwrap();
        assert!(!within_budget(root.path(), REPLAY_BYTES).await);
        assert!(within_budget(root.path(), REPLAY_BYTES + 1).await);
        assert!(!within_budget(&root.path().join("missing"), REPLAY_BYTES).await);
    }
}
