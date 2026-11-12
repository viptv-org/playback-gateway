use super::*;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

pub const MAX_AUDIO_SECONDS: usize = 120;
const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_PCM_BYTES: usize = (MAX_AUDIO_SECONDS + 1) * 32_000;

impl PlaybackManager {
    pub async fn retained_subtitle_activity(
        &self,
        id: &str,
        vad: &std::path::Path,
    ) -> Result<Value, String> {
        let _slot = self
            .subtitle_analysis
            .clone()
            .try_acquire_owned()
            .map_err(|_| "analysis_busy")?;
        let dir = {
            let sessions = self.sessions.lock().await;
            let session = sessions
                .get(id)
                .filter(|s| s.touched.elapsed() < self.config.ttl)
                .ok_or("analysis_expired")?;
            if !session.throttle.enabled
                || session.direct.is_some()
                || session.stream.is_some()
                || session.ingest.is_some()
            {
                return Err("retained_vod_audio_unavailable".into());
            }
            session.dir.clone()
        };
        if let Some((_, frames)) = self
            .subtitle_activity_cache
            .lock()
            .await
            .as_ref()
            .filter(|(job, _)| job == id)
        {
            return Ok(observation_json(frames));
        }
        let playlist = tokio::fs::read_to_string(dir.join("index.m3u8"))
            .await
            .map_err(|_| "retained_vod_audio_unavailable")?;
        let names = segment_names(&playlist)?;
        let mut bytes = Vec::new();
        for name in names {
            let path = dir.join(name);
            let meta = tokio::fs::symlink_metadata(&path)
                .await
                .map_err(|_| "analysis_gap")?;
            if !meta.is_file()
                || meta.len() > 8 * 1024 * 1024
                || bytes.len() + meta.len() as usize > MAX_INPUT_BYTES
            {
                return Err("analysis_byte_budget".into());
            }
            let file = tokio::fs::File::open(path)
                .await
                .map_err(|_| "analysis_gap")?;
            let mut segment = Vec::new();
            file.take(8 * 1024 * 1024 + 1)
                .read_to_end(&mut segment)
                .await
                .map_err(|_| "analysis_gap")?;
            if segment.len() > 8 * 1024 * 1024 || bytes.len() + segment.len() > MAX_INPUT_BYTES {
                return Err("analysis_byte_budget".into());
            }
            bytes.extend_from_slice(&segment);
        }
        let mut probe = Command::new(&self.config.ffprobe);
        probe.args([
            "-v",
            "error",
            "-protocol_whitelist",
            "pipe",
            "-f",
            "mpegts",
            "-read_intervals",
            "%+0.1",
            "-show_entries",
            "format=start_time",
            "-of",
            "json",
            "-i",
            "pipe:0",
        ]);
        let info = run(probe, &bytes, 16 * 1024, 16 * 1024).await?;
        let info: Value = serde_json::from_slice(&info.0).map_err(|_| "analysis_timestamps")?;
        let origin: f64 = info["format"]["start_time"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .filter(|v: &f64| v.is_finite())
            .ok_or("analysis_timestamps")?;
        let mut decode = Command::new(&self.config.ffmpeg);
        decode.args(["-hide_banner","-loglevel","info","-nostdin","-threads","1","-filter_threads","1","-protocol_whitelist","pipe","-f","mpegts","-copyts","-i","pipe:0","-map","0:a:0","-vn","-sn","-dn","-af","aformat=sample_fmts=s16:sample_rates=16000:channel_layouts=mono,atrim=duration=120,asetnsamples=n=320:p=0,ashowinfo","-f","s16le","pipe:1"]);
        let (mut pcm, log) = run(decode, &bytes, MAX_PCM_BYTES, 2 * 1024 * 1024).await?;
        pcm.truncate(pcm.len() / 640 * 640);
        drop(bytes);
        let vad_command = Command::new(vad);
        let (decisions, _) = run(vad_command, &pcm, 8192, 16 * 1024).await?;
        let frames = String::from_utf8(log).map_err(|_| "analysis_timestamps")?;
        let times = frame_times(&frames, origin)?;
        if times.len() != decisions.len() || times.len() < 1000 || pcm.len() != times.len() * 640 {
            return Err("analysis_incomplete".into());
        }
        let frames = times
            .into_iter()
            .zip(decisions)
            .map(|(time, speech)| {
                if speech > 1 {
                    return Err("analysis_vad_protocol");
                }
                Ok((time, speech == 1))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let result = observation_json(&frames);
        *self.subtitle_activity_cache.lock().await = Some((id.to_owned(), frames));
        Ok(result)
    }
}

fn observation_json(frames: &[(i64, bool)]) -> Value {
    json!(frames.iter().map(|(time, speech)| json!({"startUs":time,"endUs":time + 20_000,"activity":if *speech {"speech"} else {"silence"}})).collect::<Vec<_>>())
}
fn segment_names(playlist: &str) -> Result<Vec<&str>, String> {
    if playlist.len() > 256 * 1024
        || playlist.contains("#EXT-X-DISCONTINUITY")
        || playlist.contains("#EXT-X-MAP")
        || playlist.contains("#EXT-X-KEY")
    {
        return Err("analysis_timeline_unavailable".into());
    }
    let mut names = Vec::new();
    let mut duration = 0.0;
    let mut pending = None;
    for line in playlist.lines() {
        if let Some(raw) = line.strip_prefix("#EXTINF:") {
            pending = raw
                .split(',')
                .next()
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v > 0.0 && *v <= 10.0);
        } else if !line.is_empty() && !line.starts_with('#') {
            let index = hls::segment_number(line).filter(|_| line.ends_with(".ts"));
            if index != Some(names.len() as u64) {
                return Err("analysis_gap".into());
            }
            duration += pending.take().ok_or("analysis_timestamps")?;
            if duration > MAX_AUDIO_SECONDS as f64 {
                break;
            }
            names.push(line);
        }
    }
    if duration < 62.0 || names.is_empty() || names.len() > 120 {
        return Err("analysis_observing".into());
    }
    Ok(names)
}
fn frame_times(log: &str, origin: f64) -> Result<Vec<i64>, String> {
    let mut times = Vec::new();
    for line in log.lines().filter(|l| l.contains("Parsed_ashowinfo")) {
        let field = |key: &str| line.split_whitespace().find_map(|s| s.strip_prefix(key));
        let samples = field("nb_samples:")
            .and_then(|s| s.parse::<u32>().ok())
            .ok_or("analysis_timestamps")?;
        if samples != 320 {
            continue;
        }
        let time = field("pts_time:")
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .ok_or("analysis_timestamps")?;
        let mut time = ((time - origin) * 1_000_000.0).round() as i64;
        if time < -1_000 || times.last().is_some_and(|last: &i64| time < *last + 19_000) {
            return Err("analysis_timestamps".into());
        }
        if let Some(last) = times.last() {
            if time <= *last + 21_000 {
                time = *last + 20_000;
            }
        }
        times.push(time.max(0));
        if times.len() > 6000 {
            return Err("analysis_frame_budget".into());
        }
    }
    Ok(times)
}
pub async fn run(
    mut command: Command,
    input: &[u8],
    max_out: usize,
    max_error: usize,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    contain_media_process(&mut command);
    let mut child = command
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| "analysis_worker_unavailable")?;
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let result = timeout(Duration::from_secs(15), async {
        let (write, out, err, status) = tokio::join!(
            async {
                let _ = stdin.write_all(input).await;
                drop(stdin);
            },
            probe_output(stdout, max_out),
            probe_output(stderr, max_error),
            child.wait()
        );
        let _ = write;
        if !status.is_ok_and(|s| s.success()) {
            return Err("analysis_worker_failed".into());
        }
        Ok((
            out.map_err(|_| "analysis_output_budget")?,
            err.map_err(|_| "analysis_output_budget")?,
        ))
    })
    .await
    .unwrap_or_else(|_| Err("analysis_timeout".into()));
    if result.is_err() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_gaps_remote_resources_discontinuities_and_short_samples() {
        for value in [
            "#EXTM3U\n#EXTINF:40,\nhttps://provider/x.ts",
            "#EXTM3U\n#EXTINF:2,\nsegment-000000001.ts",
            "#EXTM3U\n#EXT-X-DISCONTINUITY\n",
            "#EXTM3U\n#EXTINF:2,\nsegment-000000000.ts",
        ] {
            assert!(segment_names(value).is_err());
        }
        let list = (0..31)
            .map(|i| format!("#EXTINF:2,\nsegment-{i:09}.ts\n"))
            .collect::<String>();
        assert_eq!(segment_names(&list).unwrap().len(), 31);
    }
    #[test]
    fn preserves_timestamp_gaps_and_origin() {
        let log="[Parsed_ashowinfo_1] pts_time:1.4 nb_samples:320\n[Parsed_ashowinfo_1] pts_time:2.4 nb_samples:320";
        assert_eq!(frame_times(log, 1.4).unwrap(), vec![0, 1_000_000]);
    }
}

#[cfg(test)]
mod fixture_tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires owned HLS fixture and pinned real FFmpeg/ffprobe/VAD binaries"]
    async fn real_retained_audio_is_local_bounded_and_reuses_activity() {
        let ffmpeg = PathBuf::from(std::env::var("PLAYBACK_TEST_FFMPEG").unwrap());
        let ffprobe = PathBuf::from(std::env::var("PLAYBACK_TEST_FFPROBE").unwrap());
        let vad = PathBuf::from(std::env::var("PLAYBACK_SYNC_VAD").unwrap());
        let fixture = PathBuf::from(std::env::var("PLAYBACK_SYNC_FIXTURE").unwrap());
        let root = tempfile::tempdir().unwrap();
        let manager = PlaybackManager::new(Config {
            ffmpeg,
            ffprobe,
            root: root.path().join("media"),
            max_sessions: 1,
            ttl: Duration::from_secs(120),
        });
        manager.initialize().await.unwrap();
        let dir = root.path().join("retained");
        tokio::fs::create_dir(&dir).await.unwrap();
        for file in std::fs::read_dir(&fixture).unwrap().map(Result::unwrap) {
            let name = file.file_name();
            let name = name.to_str().unwrap();
            if name == "index.m3u8" || hls::segment_number(name).is_some() {
                tokio::fs::copy(file.path(), dir.join(name)).await.unwrap();
            }
        }
        let id = "fixture";
        manager.sessions.lock().await.insert(
            id.into(),
            Session {
                caption_track: None,
                ingest: None,
                stream: None,
                _source_probe: None,
                sanitizer: None,
                direct: None,
                capability: "private".into(),
                dir: dir.clone(),
                child: None,
                touched: Instant::now(),
                stable_target_duration: false,
                target_duration: None,
                supervised_live: false,
                throttle: Throttle::on_demand(),
                permits: Arc::new(InputPermits {
                    _playback: manager.slots.clone().try_acquire_owned().unwrap().into(),
                    _provider: None,
                }),
                cleanup_tasks: manager.cleanup_tasks.clone(),
            },
        );
        let observations = manager.retained_subtitle_activity(id, &vad).await.unwrap();
        let frames = observations.as_array().unwrap();
        assert!((2900..=6000).contains(&frames.len()));
        assert!(frames
            .windows(2)
            .all(|f| f[0]["endUs"].as_i64().unwrap() <= f[1]["startUs"].as_i64().unwrap() + 100));
        let cached = manager
            .retained_subtitle_activity(id, std::path::Path::new("missing-vad"))
            .await
            .unwrap();
        assert_eq!(observations, cached);
        if let Ok(output) = std::env::var("PLAYBACK_SYNC_OBSERVATIONS") {
            std::fs::write(output, serde_json::to_vec(&observations).unwrap()).unwrap();
        }
        assert_eq!(manager.active_count().await, 1);
        assert_eq!(manager.available_output_slots(), 0);
        manager.shutdown().await;
    }
}
