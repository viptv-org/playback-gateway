//! Progressive live delivery as a same-container lossless remux of the shared
//! private replay. Every viewer owns one copy-only FFmpeg reader that can open
//! only gateway-generated local files; one authenticated ingest feeds them all.
use super::*;
use ingest::ReplayKind;

/// A slow reader may leave its bounded channel full for at most this long.
pub(super) const PROGRESSIVE_EVICT_AFTER: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Container {
    MpegTs,
    Matroska,
}
impl Container {
    pub(super) fn format(self) -> &'static str {
        match self {
            Self::MpegTs => "mpegts",
            Self::Matroska => "matroska",
        }
    }
    pub(super) fn file(self) -> &'static str {
        match self {
            Self::MpegTs => "stream.ts",
            Self::Matroska => "stream.mkv",
        }
    }
    pub(super) fn content_type(self) -> &'static str {
        match self {
            Self::MpegTs => "video/mp2t",
            Self::Matroska => "video/x-matroska",
        }
    }
}

/// The qualified subset. The input container must already be the private
/// replay's container family, so the output is the same container remuxed
/// without encoding. Everything else is explicitly unsupported, never
/// silently converted or switched to HLS.
pub(super) fn container(
    probe: &Probe,
    caps: &Capabilities,
    audio: Option<&ProbeStream>,
    selection: &TrackSelection,
    live: bool,
) -> Option<Container> {
    if !live
        || selection.subtitle_track_index.is_some()
        || selection.preferred_subtitle_language.is_some()
        || ingest::replay_indices(probe).is_none()
        || probe.interlaced()
    {
        return None;
    }
    let names = probe.format.get("format_name")?.as_str()?;
    let input = if names.split(',').any(|name| name == "mpegts") {
        ReplayKind::Ts
    } else if names.split(',').any(|name| name == "matroska") {
        ReplayKind::Matroska
    } else {
        return None;
    };
    if ReplayKind::for_probe(probe) != input {
        return None;
    }
    let video = probe.video().ok()?;
    let fits = |value: Option<u32>, limit: u32| value.is_some_and(|v| v >= 2 && v <= limit);
    if !fits(video.width, caps.max_width) || !fits(video.height, caps.max_height) {
        return None;
    }
    let video_codecs = caps.direct_video_codecs.as_deref().unwrap_or_default();
    let audio_codecs = caps.direct_audio_codecs.as_deref().unwrap_or_default();
    if !direct_gate::declared_codec(video_codecs, video.codec_name.as_deref())
        || audio
            .is_some_and(|s| !direct_gate::declared_codec(audio_codecs, s.codec_name.as_deref()))
    {
        return None;
    }
    Some(match input {
        ReplayKind::Ts => Container::MpegTs,
        ReplayKind::Matroska => Container::Matroska,
    })
}

/// Copy-only reader arguments: local replay files only, start near the live
/// edge at a segment (keyframe) boundary, one video and the selected audio.
pub(super) fn reader_args(
    cmd: &mut Command,
    extensions: &str,
    playlist: &std::path::Path,
    audio: Option<u32>,
    container: Container,
) {
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    cmd.args([
        "-protocol_whitelist",
        "file",
        "-allowed_extensions",
        extensions,
        "-live_start_index",
        "-2",
        "-i",
    ])
    .arg(playlist);
    cmd.args(["-map", "0:v:0"]);
    if let Some(audio) = audio {
        cmd.arg("-map").arg(format!("0:{audio}"));
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
        "-f",
        container.format(),
        "-progress",
        "pipe:2",
        "-stats_period",
        "0.5",
        "pipe:1",
    ]);
}

pub(super) struct Progressive {
    pub(super) container: Container,
    pub(super) scope: String,
    pub(super) source: String,
    pub(super) headers: String,
    pub(super) probe: Arc<Probe>,
    pub(super) permits: Arc<InputPermits>,
    pub(super) audio_index: Option<u32>,
    pub(super) response: PlaybackResponse,
}

impl PlaybackManager {
    pub(super) async fn start_progressive(
        &self,
        job: Progressive,
    ) -> Result<PlaybackResponse, String> {
        let Progressive {
            container,
            scope,
            source,
            headers,
            probe,
            permits,
            audio_index,
            mut response,
        } = job;
        permits.admit_output(&self.slots)?;
        tokio::fs::create_dir_all(&self.config.root)
            .await
            .map_err(|_| "Media storage unavailable".to_owned())?;
        let ingest = self
            .shared_ingest(&scope, &source, &headers, probe.clone())
            .await?;
        let audio = audio_index.map(|index| ingest.index(index)).transpose()?;
        let id = Uuid::new_v4().to_string();
        let capability = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let mut cmd = Command::new(&self.config.ffmpeg);
        contain_media_process(&mut cmd);
        cmd.kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        reader_args(
            &mut cmd,
            ingest.extensions(),
            &ingest.playlist,
            audio,
            container,
        );
        let mut child = cmd
            .spawn()
            .map_err(|_| "Playback engine unavailable".to_owned())?;
        let output = child.stdout.take().ok_or("Media output unavailable")?;
        let progress = child.stderr.take().ok_or("Media progress unavailable")?;
        let stream = fmp4::Stream::with_transport(
            output,
            progress,
            0.0,
            permits.clone(),
            (container.file(), container.content_type()),
            Some(PROGRESSIVE_EVICT_AFTER),
        );
        // The session owns the reader before any await, so a cancelled start
        // still kills it and releases the ingest reference.
        let session = Session {
            caption_track: None,
            ingest: Some(ingest),
            stream: Some(stream.clone()),
            _source_probe: Some(probe),
            direct: None,
            capability: capability.clone(),
            dir: PathBuf::new(),
            child: Some(child),
            touched: Instant::now(),
            stable_target_duration: false,
            target_duration: None,
            supervised_live: false,
            throttle: Throttle::default(),
            permits,
            cleanup_tasks: self.cleanup_tasks.clone(),
        };
        stream.ready().await?;
        self.sessions.lock().await.insert(id.clone(), session);
        tracing::info!(format = container.format(), "Progressive remux ready");
        response.url = format!("/media/{id}/{capability}/{}", container.file());
        response.id = id;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn probe(format: &str, video: &str, audio: &str) -> Probe {
        serde_json::from_value(serde_json::json!({
            "streams": [
                {"index":0,"codec_type":"video","codec_name":video,"width":1920,"height":1080,"field_order":"progressive"},
                {"index":1,"codec_type":"audio","codec_name":audio,"channels":2}
            ],
            "format": {"format_name": format}
        }))
        .unwrap()
    }
    fn caps() -> Capabilities {
        Capabilities {
            max_width: 1920,
            max_height: 1080,
            direct_files: Some(true),
            direct_hls: Some(false),
            direct_video_codecs: Some(vec!["h264".into(), "ffv1".into()]),
            direct_audio_codecs: Some(vec!["aac".into(), "flac".into()]),
            ..Capabilities::default()
        }
    }
    fn eligible(
        probe: &Probe,
        caps: &Capabilities,
        selection: &TrackSelection,
        live: bool,
    ) -> Option<Container> {
        container(probe, caps, probe.streams.get(1), selection, live)
    }

    #[test]
    fn mpegts_and_matroska_inputs_keep_their_container_family() {
        let none = TrackSelection::default();
        assert_eq!(
            eligible(&probe("mpegts", "h264", "aac"), &caps(), &none, true),
            Some(Container::MpegTs)
        );
        assert_eq!(
            eligible(
                &probe("matroska,webm", "h264", "flac"),
                &caps(),
                &none,
                true
            ),
            Some(Container::Matroska)
        );
        assert_eq!(Container::MpegTs.file(), "stream.ts");
        assert_eq!(Container::Matroska.content_type(), "video/x-matroska");
    }

    #[test]
    fn unsupported_inputs_fail_explicitly_instead_of_changing_container() {
        let none = TrackSelection::default();
        // On-demand, other containers, and container/replay family mismatches.
        assert_eq!(
            eligible(&probe("mpegts", "h264", "aac"), &caps(), &none, false),
            None
        );
        assert_eq!(
            eligible(&probe("hls", "h264", "aac"), &caps(), &none, true),
            None
        );
        assert_eq!(
            eligible(&probe("mov,mp4,m4a", "h264", "aac"), &caps(), &none, true),
            None
        );
        assert_eq!(
            eligible(&probe("matroska,webm", "h264", "aac"), &caps(), &none, true),
            None
        );
        assert_eq!(
            eligible(&probe("mpegts", "h264", "flac"), &caps(), &none, true),
            None
        );
        // Codecs outside the private replay or the client's declared decoders.
        assert_eq!(
            eligible(&probe("mpegts", "vp9", "aac"), &caps(), &none, true),
            None
        );
        let mut narrow = caps();
        narrow.direct_audio_codecs = Some(vec!["opus".into()]);
        assert_eq!(
            eligible(&probe("mpegts", "h264", "aac"), &narrow, &none, true),
            None
        );
        narrow = caps();
        narrow.max_width = 1280;
        assert_eq!(
            eligible(&probe("mpegts", "h264", "aac"), &narrow, &none, true),
            None
        );
        // Subtitle selections are never carried by this byte stream.
        let subtitles = TrackSelection {
            subtitle_track_index: Some(2),
            ..TrackSelection::default()
        };
        assert_eq!(
            eligible(&probe("mpegts", "h264", "aac"), &caps(), &subtitles, true),
            None
        );
    }

    #[test]
    fn reader_is_copy_only_local_and_starts_at_a_live_segment_boundary() {
        let mut cmd = Command::new("ffmpeg");
        reader_args(
            &mut cmd,
            "ts,m3u8",
            std::path::Path::new("/media/x/index.m3u8"),
            Some(2),
            Container::MpegTs,
        );
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let joined = args.join(" ");
        assert!(joined.contains("-protocol_whitelist file -allowed_extensions ts,m3u8"));
        assert!(joined.contains("-live_start_index -2 -i /media/x/index.m3u8"));
        assert!(joined.contains("-map 0:v:0 -map 0:2 -c copy -sn -dn"));
        assert!(joined.contains("-f mpegts"));
        assert!(!args
            .iter()
            .any(|a| a.starts_with("-c:v") || a == "-vf" || a == "-ss"));
    }

    #[tokio::test]
    async fn slow_reader_is_evicted_without_blocking_its_producer_forever() {
        let (mut writer, output) = tokio::io::duplex(64 * 1024);
        let (_progress_writer, progress) = tokio::io::duplex(1024);
        let slots = Arc::new(Semaphore::new(1));
        let permits = Arc::new(InputPermits {
            _playback: slots.clone().try_acquire_owned().unwrap().into(),
            _provider: None,
        });
        let stream = fmp4::Stream::with_transport(
            output,
            progress,
            0.0,
            permits,
            ("stream.ts", "video/mp2t"),
            Some(Duration::from_millis(200)),
        );
        // Never consumed: fill the bounded channel and the pipe.
        let producer = tokio::spawn(async move {
            let block = vec![0x47u8; 64 * 1024];
            loop {
                if writer.write_all(&block).await.is_err() {
                    break;
                }
            }
        });
        stream.ready().await.unwrap();
        timeout(Duration::from_secs(5), async {
            while !stream.evicted() {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("slow reader evicted");
        // The producer side is released (pipe closed) rather than pinned.
        timeout(Duration::from_secs(5), producer)
            .await
            .expect("producer released")
            .unwrap();
        // The bounded buffer drains, then reports eviction instead of a clean end.
        let response = stream.clone().serve(axum::http::Method::GET).await.unwrap();
        assert_eq!(response.headers()["accept-ranges"], "none");
        assert_eq!(response.headers()["content-type"], "video/mp2t");
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await;
        assert!(
            body.is_err(),
            "eviction must not look like a complete stream"
        );
        drop(slots);
    }
}
