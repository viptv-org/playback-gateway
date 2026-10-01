use super::*;

impl PlaybackManager {
    #[allow(clippy::too_many_arguments)]
    pub async fn start_with_selection(
        &self,
        url: String,
        headers: HashMap<String, String>,
        position: f64,
        capabilities: Option<Capabilities>,
        force: bool,
        live: bool,
        provider_permit: Option<OwnedSemaphorePermit>,
        selection: TrackSelection,
    ) -> Result<PlaybackResponse, String> {
        self.prepare_with_selection(
            url,
            headers,
            position,
            capabilities,
            force,
            live,
            provider_permit,
            selection,
        )
        .await?
        .start()
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn prepare_with_selection(
        &self,
        url: String,
        headers: HashMap<String, String>,
        position: f64,
        capabilities: Option<Capabilities>,
        force: bool,
        live: bool,
        provider_permit: Option<OwnedSemaphorePermit>,
        selection: TrackSelection,
    ) -> Result<PreparedPlayback<'_>, String> {
        self.prepare_with_scope(
            url,
            headers,
            position,
            capabilities,
            force,
            live,
            provider_permit,
            selection,
            None,
        )
        .await
    }

    /// Scope is caller-authorized opaque namespace text, never product identity.
    /// URL, credential headers and egress route additionally bind the ingest.
    #[allow(clippy::too_many_arguments)]
    pub async fn prepare_shared_with_selection(
        &self,
        url: String,
        headers: HashMap<String, String>,
        position: f64,
        capabilities: Option<Capabilities>,
        force: bool,
        live: bool,
        selection: TrackSelection,
        scope: String,
    ) -> Result<PreparedPlayback<'_>, String> {
        self.prepare_with_scope(
            url,
            headers,
            position,
            capabilities,
            force,
            live,
            None,
            selection,
            Some(scope),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_with_scope(
        &self,
        url: String,
        headers: HashMap<String, String>,
        position: f64,
        capabilities: Option<Capabilities>,
        force: bool,
        live: bool,
        provider_permit: Option<OwnedSemaphorePermit>,
        selection: TrackSelection,
        scope: Option<String>,
    ) -> Result<PreparedPlayback<'_>, String> {
        let reason = selection.conversion_reason.as_deref();
        if reason.is_some_and(|value| {
            !matches!(
                value,
                "container" | "video-codec" | "audio-codec" | "rendering" | "performance"
            )
        }) {
            return Err("Invalid media conversion reason".into());
        }
        let force_video = force && reason != Some("audio-codec");
        let force_audio = reason == Some("audio-codec") || force && reason.is_none();
        if selection
            .audio_track_index
            .is_some_and(|index| index > 65535)
        {
            return Err("Requested input audio track index is out of range".into());
        }
        if selection
            .subtitle_track_index
            .is_some_and(|index| index > 65535)
        {
            return Err("Requested input subtitle track index is out of range".into());
        }
        if live && position != 0.0 {
            return Err("Live playback does not support offset seeking".into());
        }
        let _lifecycle = self.lifecycle.read().await;
        if self.slots.is_closed() {
            return Err("Playback is shutting down".into());
        }
        // Never propagate parser/provider/subprocess errors: they may contain credentials.
        let validated = crate::validate_url(&url).map_err(|_| "Invalid playback URL".to_owned())?;
        if !matches!(validated.scheme(), "http" | "https")
            || !validated.username().is_empty()
            || validated.password().is_some()
            || validated.fragment().is_some()
        {
            return Err("Invalid playback URL".into());
        }
        if !position.is_finite() || !(0.0..=604800.0).contains(&position) {
            return Err("Invalid playback position".into());
        }
        let header_block = header_block(&headers)?;
        let caps = capabilities.unwrap_or_default();
        if caps.browser.as_ref().is_some_and(|b| !b.valid()) {
            return Err("Invalid browser capability report".into());
        }
        if !force
            && scope.is_none()
            && caps.direct_play
            && caps.browser.as_ref().is_some_and(|b| b.inspect_original)
            && std::env::var("PLAYBACK_BROWSER_PREPARATION").as_deref() == Ok("1")
            && selection.audio_track_index.is_none()
            && selection.subtitle_track_index.is_none()
        {
            let permit = self
                .preparation_slots
                .clone()
                .try_acquire_owned()
                .map_err(|_| MSG_PLAYBACK_CAPACITY.to_owned())?;
            let permits = Arc::new(InputPermits {
                _playback: permit.into(),
                _provider: provider_permit,
            });
            let started = Instant::now();
            let direct =
                direct::Direct::prepare(validated.clone(), &headers, "auto", permits.clone())
                    .await?;
            let format = direct.format.clone();
            let id = Uuid::new_v4().to_string();
            let capability = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            let filename = if format == "hls" {
                "index.m3u8"
            } else {
                "source.bin"
            };
            let response = PlaybackResponse {
                id: id.clone(),
                url: format!("/media/{id}/{capability}/{filename}"),
                format,
                mode: "direct".into(),
                video_mode: "copy".into(),
                audio_mode: "copy".into(),
                position,
                live,
                duration: 0.0,
                audio_tracks: Vec::new(),
                subtitle_tracks: Vec::new(),
                selected_audio: None,
                selected_subtitle: None,
                subtitles_supported: false,
            };
            return Ok(PreparedPlayback::new(None, async move {
                let _lifecycle = _lifecycle;
                permits.admit_output(&self.slots)?;
                self.sessions.lock().await.insert(
                    id,
                    Session {
                        caption_track: None,
                        ingest: None,
                        stream: None,
                        _source_probe: None,
                        direct: Some(direct),
                        capability,
                        dir: PathBuf::new(),
                        child: None,
                        touched: Instant::now(),
                        stable_target_duration: false,
                        target_duration: None,
                        supervised_live: false,
                        throttle: Throttle::default(),
                        permits,
                        cleanup_tasks: self.cleanup_tasks.clone(),
                    },
                );
                tracing::info!(
                    connection_ms = started.elapsed().as_millis() as u64,
                    mode = "client-inspection",
                    "Original media prepared without ffprobe"
                );
                Ok(response)
            }));
        }
        let (width, height) = dimensions(&caps)?;
        // A scoped progressive request declares the decoders it will use for
        // the original codecs; it never falls back to the managed H264/AAC path.
        let progressive_request = scope.is_some()
            && live
            && caps.direct_files == Some(true)
            && caps.direct_hls == Some(false);
        if !progressive_request && (!caps.h264 || !caps.aac) {
            return Err("H264 and AAC playback support is required".into());
        }
        let permit = self
            .preparation_slots
            .clone()
            .try_acquire_owned()
            // Distinct from a provider-connection limit: this is the server's own
            // session budget (PLAYBACK_MAX_SESSIONS), which needs the viewer to stop
            // something rather than to retry. It is reported as 503, not 429.
            .map_err(|_| MSG_PLAYBACK_CAPACITY.to_owned())?;
        let permits = Arc::new(InputPermits {
            _playback: permit.into(),
            _provider: provider_permit,
        });
        self.initialize().await?;
        let probe_started = Instant::now();
        let probe = self
            .cached_probe_scoped(
                validated.as_str(),
                &header_block,
                live,
                Some(permits.clone()),
                scope.as_deref(),
            )
            .await
            .ok_or_else(|| {
                tracing::warn!(live, "Playback source inspection failed");
                "Could not inspect source video safely; try another stream".to_owned()
            })?;
        let probe_ms = probe_started.elapsed().as_millis() as u64;
        let shared_scope = scope.clone().filter(|_| {
            live && caps.direct_hls != Some(false) && ingest::replay_indices(&probe).is_some()
        });
        // The capability envelope gates only the managed/transcode path. Original
        // delivery is decided below from the client's declared decoders, so an
        // HDR, wide-gamut or otherwise unusual source is still playable whenever
        // the client can demux and decode it.
        let selected_input = probe.select_audio(&selection)?;
        let caption_index = selection.subtitle_track_index.or_else(|| {
            let language = selection.preferred_subtitle_language.as_deref()?;
            probe
                .streams
                .iter()
                .filter(|s| s.codec_type.as_deref() == Some("subtitle") && s.text_subtitle())
                .filter(|s| {
                    s.language().is_some_and(|actual| {
                        normalize_audio_language(&actual) == normalize_audio_language(language)
                    })
                })
                .take(32)
                .find_map(|s| s.index.filter(|i| *i <= 65535))
        });
        let selected_caption = probe.select_subtitle(caption_index)?;
        let mut audio_tracks = probe.tracks("audio");
        let mut subtitle_tracks = probe.tracks("subtitle");
        for track in audio_tracks.iter_mut().chain(subtitle_tracks.iter_mut()) {
            if let Some(stream) = probe
                .streams
                .iter()
                .find(|s| s.index == Some(track.input_index))
            {
                if let Some(title) = stream
                    .tags
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case("title"))
                    .map(|(_, value)| value)
                {
                    let title =
                        crate::source_display_text(title, 128, validated.as_str(), &headers);
                    if !title.is_empty() {
                        track.title = title;
                    }
                }
            }
            track.selected = selected_input.and_then(|s| s.index) == Some(track.input_index)
                || selected_caption.and_then(|s| s.index) == Some(track.input_index);
        }
        let selected_audio = selected_input.map(|audio| SelectedAudio {
            input_index: audio.index.expect("selection validates stream index"),
            output_index: 0,
            output_audio_ordinal: 0,
            output_stream_index: 1,
            language: audio.language(),
            language_status: audio.language_status().into(),
            disposition: audio.disposition.as_ref().map(ProbeDisposition::public),
            title: audio_tracks
                .iter()
                .find(|t| t.selected)
                .map(|t| t.title.clone())
                .unwrap_or_default(),
        });
        let selected_subtitle = selected_caption.map(|caption| SelectedSubtitle {
            input_index: caption.index.expect("selection validates stream index"),
            output_index: 0,
            // HLS captions are a separate single-stream WebVTT rendition.
            output_stream_index: 0,
            language: caption.language(),
            language_status: caption.language_status().into(),
            disposition: caption.disposition.as_ref().map(ProbeDisposition::public),
            title: subtitle_tracks
                .iter()
                .find(|t| t.selected)
                .map(|t| t.title.clone())
                .unwrap_or_default(),
        });
        if !live
            && probe
                .duration()
                .is_some_and(|duration| position >= duration)
        {
            return Err("Playback position is past the end of this source".into());
        }
        if progressive_request {
            let Some(container) =
                progressive::container(&probe, &caps, selected_input, &selection, live)
            else {
                // No plan: the coordinator reports `unsupported_output`.
                return Ok(PreparedPlayback::new(None, async {
                    Err("Progressive output unsupported".to_owned())
                }));
            };
            let video = probe.video()?;
            let audio_mode = if selected_input.is_some() {
                "copy"
            } else {
                "none"
            };
            let plan = OutputPlan {
                mode: "remux".into(),
                format: container.format().into(),
                video_mode: "copy".into(),
                audio_mode: audio_mode.into(),
                video_codec: video.codec_name.clone().unwrap_or_default(),
                audio_codec: selected_input.and_then(|s| s.codec_name.clone()),
                width: video.width.unwrap_or(0),
                height: video.height.unwrap_or(0),
                audio_track: selected_audio.as_ref().map(|s| s.input_index),
                subtitle_track: None,
                position_micros: 0,
                segmentation: "progressive".into(),
                // One reader per viewer; viewers share the ingest, not the output.
                shareable: false,
                shared_ingest: true,
            };
            let job = progressive::Progressive {
                container,
                scope: scope.expect("progressive requests are scoped"),
                source: validated.to_string(),
                headers: header_block,
                probe: probe.clone(),
                permits,
                audio_index: selected_audio.as_ref().map(|s| s.input_index),
                response: PlaybackResponse {
                    id: String::new(),
                    url: String::new(),
                    format: container.format().into(),
                    mode: "remux".into(),
                    video_mode: "copy".into(),
                    audio_mode: audio_mode.into(),
                    position: 0.0,
                    live: true,
                    duration: 0.0,
                    audio_tracks,
                    subtitles_supported: false,
                    subtitle_tracks,
                    selected_audio,
                    selected_subtitle: None,
                },
            };
            return Ok(PreparedPlayback::new(Some(plan), async move {
                let _lifecycle = _lifecycle;
                self.start_progressive(job).await
            }));
        }
        // Original delivery is opt-in, uses inspected tracks, and keeps the
        // provider reservation owned by this session and in-flight reads.
        if shared_scope.is_none()
            && !force
            && caps.direct_play
            && std::env::var("PLAYBACK_DIRECT_PLAY").as_deref() != Ok("0")
        {
            if let Some(format) = direct_format(&probe, &caps, selected_input, &selection) {
                if let Ok(transport) =
                    direct::Direct::prepare(validated.clone(), &headers, format, permits.clone())
                        .await
                {
                    let plan = OutputPlan {
                        mode: "direct".into(),
                        format: format.into(),
                        video_mode: "copy".into(),
                        audio_mode: if selected_input.is_some() {
                            "copy"
                        } else {
                            "none"
                        }
                        .into(),
                        video_codec: probe.video()?.codec_name.clone().unwrap_or_default(),
                        audio_codec: selected_input.and_then(|s| s.codec_name.clone()),
                        width: probe.video()?.width.unwrap_or(0),
                        height: probe.video()?.height.unwrap_or(0),
                        audio_track: selected_input.and_then(|s| s.index),
                        subtitle_track: None,
                        position_micros: (position * 1_000_000.0).round() as u64,
                        segmentation: "original".into(),
                        shareable: true,
                        shared_ingest: false,
                    };
                    let id = Uuid::new_v4().to_string();
                    let capability =
                        format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
                    let filename = if format == "hls" {
                        "index.m3u8".to_owned()
                    } else {
                        // Original files are served under their own container
                        // name; the client demuxes them itself.
                        format!("source.{format}")
                    };
                    // A native file can only retain one default audio track here;
                    // arbitrary track changes deliberately return to managed HLS.
                    let response = PlaybackResponse {
                        id: id.clone(),
                        url: format!("/media/{id}/{capability}/{filename}"),
                        format: format.into(),
                        mode: "direct".into(),
                        video_mode: "copy".into(),
                        audio_mode: if selected_input.is_some() {
                            "copy"
                        } else {
                            "none"
                        }
                        .into(),
                        position,
                        live,
                        duration: if live {
                            0.0
                        } else {
                            probe.duration().unwrap_or(0.0)
                        },
                        audio_tracks,
                        subtitles_supported: subtitle_tracks.iter().any(|track| track.supported),
                        subtitle_tracks,
                        selected_audio,
                        selected_subtitle: None,
                    };
                    return Ok(PreparedPlayback::new(Some(plan), async move {
                        let _lifecycle = _lifecycle;
                        permits.admit_output(&self.slots)?;
                        self.sessions.lock().await.insert(
                            id,
                            Session {
                                caption_track: None,
                                ingest: None,
                                stream: None,
                                _source_probe: Some(probe.clone()),
                                direct: Some(transport),
                                capability,
                                dir: PathBuf::new(),
                                child: None,
                                touched: Instant::now(),
                                stable_target_duration: false,
                                target_duration: None,
                                supervised_live: false,
                                throttle: Throttle::default(),
                                permits,
                                cleanup_tasks: self.cleanup_tasks.clone(),
                            },
                        );
                        tracing::info!(
                            mode = "direct",
                            format,
                            probe_ms,
                            "Playback preparation completed"
                        );
                        Ok(response)
                    }));
                }
                tracing::info!(
                    reason = "original_transport_unavailable",
                    "Using managed playback fallback"
                );
            }
        }
        // Managed output re-encodes into the envelope the browser declared, so the
        // inspected source must be inside it. Original delivery above is exempt:
        // there the client's own decoders, not this policy, decide.
        probe.ensure_supported()?;
        // Encoders never upscale. Normalize excess capability headroom before
        // computing transforms so equivalent output requests share one plan.
        let width = width.min(probe.video()?.width.unwrap_or(width)) / 2 * 2;
        let height = height.min(probe.video()?.height.unwrap_or(height)) / 2 * 2;
        let hdr_transfer = probe.hdr_transfer()?;
        let hdr = hdr_transfer.is_some();
        let interlaced = probe.interlaced();
        let mut transforms = Vec::new();
        // A GPU tone mapper replaces only the HDR step; deinterlacing stays on the CPU.
        let mut deinterlace = None;
        if interlaced || hdr {
            let filters = self.available_filters().await?;
            if interlaced {
                let filter = if filters.contains("bwdif") {
                    "bwdif"
                } else if filters.contains("yadif") {
                    "yadif"
                } else {
                    return Err(
                        "Interlaced playback requires the bwdif or yadif FFmpeg filter".into(),
                    );
                };
                let filter =
                    format!("{filter}=mode=send_frame:parity=auto:deint=all,setfield=prog");
                transforms.push(filter.clone());
                deinterlace = Some(filter);
            }
            if hdr {
                if !["zscale", "tonemap", "sidedata"]
                    .iter()
                    .all(|name| filters.contains(*name))
                {
                    return Err("HDR10/HLG conversion requires FFmpeg zscale, tonemap and sidedata filters; select an SDR source".into());
                }
                transforms.push(hdr_filter(width, height));
            }
        }
        if !hdr {
            transforms.push(scale_filter(width, height));
        }
        let duration = if live { None } else { probe.duration() };
        if duration.is_some_and(|duration| position >= duration) {
            return Err("Playback position is past the end of this source".into());
        }
        // Qualified browser copy delivery reports the preceding keyframe origin;
        // the client consumes preroll instead of requiring video encoding for seek.
        let continuous = !caps.direct_play
            && selected_subtitle.is_none()
            && caps.browser.as_ref().is_some_and(|b| b.fmp4);
        let video = probe.video()?;
        // Sharing replaces original HLS transport, not the native decoder gate.
        // Preserve previously compatible native HEVC instead of forcing H264.
        let native_hevc_copy = shared_scope.is_some()
            && video.codec_name.as_deref() == Some("hevc")
            && direct_format(&probe, &caps, selected_input, &selection) == Some("hls");
        let hevc_copy = native_hevc_copy
            || (continuous || live && caps.browser.as_ref().is_some_and(|b| b.fmp4))
                && !hdr
                && !interlaced
                && video.codec_name.as_deref() == Some("hevc")
                && matches!(video.profile.as_deref(), Some("Main" | "Main 10"))
                && matches!(video.pix_fmt.as_deref(), Some("yuv420p" | "yuv420p10le"))
                && conservative_frame_rate(video.avg_frame_rate.as_deref())
                && caps.browser.as_ref().is_some_and(|b| {
                    b.engines.iter().any(|e| {
                        e.codec == "hevc"
                            && e.evidence != "unknown"
                            && matches!(e.engine.as_str(), "native" | "mse")
                            && video.level.is_some_and(|level| {
                                level > 0 && level <= e.max_level.unwrap_or(150)
                            })
                            && e.bit_depth.unwrap_or(8)
                                >= if video.pix_fmt.as_deref() == Some("yuv420p10le") {
                                    10
                                } else {
                                    8
                                }
                            && compat::frame_rate_within(
                                video.avg_frame_rate.as_deref(),
                                e.max_frame_rate.unwrap_or(30),
                            )
                            && video.width.zip(e.max_width).is_some_and(|(v, m)| v <= m)
                            && video.height.zip(e.max_height).is_some_and(|(v, m)| v <= m)
                    })
                });
        let compatible_video = probe.compatible_video(width, height, H264_COPY_LEVEL) || hevc_copy;
        let seek_origin = if !force_video && continuous && compatible_video {
            self.copy_seek_position(
                validated.as_str(),
                &header_block,
                position,
                probe.format["start_time"]
                    .as_str()
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.0),
                permits.clone(),
                probe.relaxed_hls_segments,
            )
            .await
        } else {
            None
        };
        let position = seek_origin.unwrap_or(position);
        let copy_video =
            !force_video && compatible_video && (position == 0.0 || seek_origin.is_some());
        // Copy audio independently at zero offset. After input-side seeking,
        // decoded audio is required to keep MPEGTS/WebVTT on the same clock.
        let copy_audio = !force_audio
            && (position == 0.0 || seek_origin.is_some())
            && probe.compatible_audio_stream(selected_input);
        let remux = copy_video && copy_audio;
        let has_audio = selected_input.is_some();
        let continuous_output = continuous && copy_video;
        let fmp4_hls = (!copy_video || hevc_copy)
            && selected_subtitle.is_none()
            && caps.browser.as_ref().is_some_and(|browser| browser.fmp4);
        let plan = OutputPlan {
            mode: if remux { "remux" } else { "transcode" }.into(),
            format: if continuous_output { "fmp4" } else { "hls" }.into(),
            video_mode: if copy_video { "copy" } else { "encode" }.into(),
            audio_mode: if !has_audio {
                "none"
            } else if copy_audio {
                "copy"
            } else {
                "encode"
            }
            .into(),
            video_codec: if copy_video {
                video.codec_name.clone().unwrap_or_default()
            } else {
                "h264".into()
            },
            audio_codec: if !has_audio {
                None
            } else if copy_audio {
                selected_input.and_then(|s| s.codec_name.clone())
            } else {
                Some("aac".into())
            },
            width: if copy_video {
                video.width.unwrap_or(0)
            } else {
                width
            },
            height: if copy_video {
                video.height.unwrap_or(0)
            } else {
                height
            },
            audio_track: selected_audio.as_ref().map(|s| s.input_index),
            subtitle_track: selected_subtitle.as_ref().map(|s| s.input_index),
            position_micros: (position * 1_000_000.0).round() as u64,
            segmentation: format!(
                "{}:{}",
                if continuous_output {
                    "continuous"
                } else if fmp4_hls {
                    "fmp4"
                } else {
                    "mpegts"
                },
                if copy_video && caps.browser.is_none() {
                    "split"
                } else {
                    "keyframes"
                }
            ),
            shareable: !continuous_output,
            shared_ingest: shared_scope.is_some(),
        };
        let hdr_transfer = hdr_transfer.map(str::to_owned);
        Ok(PreparedPlayback::new(Some(plan), async move {
            let _lifecycle = _lifecycle;
            permits.admit_output(&self.slots)?;
            let hdr_transfer = hdr_transfer.as_deref();
            let id = Uuid::new_v4().to_string();
            let capability = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            // An absolute output path keeps relative config roots independent of FFmpeg's cwd.
            tokio::fs::create_dir_all(&self.config.root)
                .await
                .map_err(|_| "Media storage unavailable".to_owned())?;
            let root = tokio::fs::canonicalize(&self.config.root)
                .await
                .map_err(|_| "Media storage unavailable".to_owned())?;
            let dir = root.join(&id);
            let mut session = Session {
                caption_track: selected_subtitle
                    .as_ref()
                    .map(|subtitle| subtitle.input_index),
                ingest: None,
                stream: None,
                _source_probe: Some(probe.clone()),
                direct: None,
                capability: capability.clone(),
                dir: dir.clone(),
                child: None,
                touched: Instant::now(),
                stable_target_duration: true,
                target_duration: None,
                supervised_live: false,
                throttle: if live {
                    Throttle::default()
                } else {
                    Throttle::on_demand()
                },
                permits,
                cleanup_tasks: self.cleanup_tasks.clone(),
            };
            tokio::fs::create_dir(&dir)
                .await
                .map_err(|_| "Media storage unavailable".to_owned())?;
            if let Some(scope) = &shared_scope {
                session.ingest = Some(
                    self.shared_ingest(scope, validated.as_str(), &header_block, probe.clone())
                        .await?,
                );
            }
            let accel = if copy_video {
                hardware::Accel::default()
            } else {
                self.accel().await
            };
            let mut pipeline =
                hardware::plan(copy_video, accel, probe.video()?, hdr_transfer, interlaced);
            let tone_map = hardware::tone_map(accel.vaapi, hdr_transfer);
            let engine_started = Instant::now();
            loop {
                let mut cmd = Command::new(&self.config.ffmpeg);
                cmd.kill_on_drop(true)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);
                let shared_captions = session
                    .ingest
                    .as_ref()
                    .is_some_and(|ingest| ingest.captions.is_some());
                if shared_captions {
                    cmd.arg("-copyts");
                }
                if pipeline.vaapi() {
                    hardware::vaapi_device_args(
                        &mut cmd,
                        self.vaapi_path().expect("validated device"),
                        pipeline == hardware::Pipeline::VaapiDecode,
                    );
                } else if pipeline.accelerated() {
                    hardware::device_args(
                        &mut cmd,
                        self.qsv_device.as_ref().expect("validated device"),
                    );
                    if pipeline == hardware::Pipeline::QsvDecode {
                        cmd.args(["-hwaccel", "qsv", "-hwaccel_output_format", "qsv"]);
                        // FFmpeg 5.x needs the explicit QSV decoder to keep frames on GPU.
                        let decoder = if probe.video()?.codec_name.as_deref() == Some("hevc") {
                            "hevc_qsv"
                        } else {
                            "h264_qsv"
                        };
                        cmd.args(["-c:v", decoder]);
                    }
                }
                if let Some(ingest) = &session.ingest {
                    contain_media_process(&mut cmd);
                    // Trusted generated playlist only. Provider protocols and
                    // credentials cannot leak into local output readers.
                    cmd.args([
                        "-protocol_whitelist",
                        "file",
                        "-allowed_extensions",
                        // FFmpeg 9 applies this allowlist to live playlist
                        // reloads as well as segments. Both are gateway-owned
                        // files; network protocols remain disabled above.
                        ingest.extensions(),
                        "-live_start_index",
                        "-2",
                    ]);
                } else {
                    input_args(&mut cmd, &header_block);
                    relaxed_hls_args(&mut cmd, probe.relaxed_hls_segments);
                }
                // Input is never paced to realtime. A live input already arrives in
                // realtime, and `-re` only withheld the upstream's initial burst (an
                // HLS source starts three segments behind its edge), which pinned
                // browsers to a one-segment buffer that stuttered on any jitter.
                // Jellyfin's M3U tuners, Threadfin and ErsatzTV read live input the
                // same way. On-demand encoding is paced by its viewer (`Throttle`).
                if position > 0.0 {
                    cmd.arg("-ss").arg(format!("{position:.6}"));
                }
                cmd.arg("-i");
                if let Some(ingest) = &session.ingest {
                    cmd.arg(&ingest.playlist);
                } else {
                    cmd.arg(validated.as_str());
                }
                cmd.args(["-map", "0:v:0"]);
                if shared_captions {
                    cmd.args(["-hls_segment_options", "mpegts_copyts=1"]);
                }
                if let Some(audio) = &selected_audio {
                    let index = match &session.ingest {
                        Some(ingest) => ingest.index(audio.input_index)?,
                        None => audio.input_index,
                    };
                    cmd.arg("-map").arg(format!("0:{index}"));
                }
                if let Some(subtitle) = selected_subtitle.as_ref().filter(|_| !shared_captions) {
                    cmd.arg("-map").arg(format!("0:{}", subtitle.input_index));
                    cmd.args(["-c:s", "webvtt", "-max_interleave_delta", "1000000"]);
                } else {
                    cmd.arg("-sn");
                }
                cmd.args(["-dn", "-map_metadata", "-1", "-map_chapters", "-1"]);
                if let Some(audio) = &selected_audio {
                    cmd.arg("-metadata:s:a:0")
                        .arg(if audio.language_status == "tagged_english" {
                            "language=eng"
                        } else {
                            "language=und"
                        });
                }
                if copy_video {
                    cmd.args(["-c:v", "copy"]);
                    if hevc_copy && (continuous_output || fmp4_hls) {
                        cmd.args(["-tag:v", "hvc1"]);
                    }
                } else {
                    let gop = (HLS_SEGMENT_SECONDS * 30).to_string();
                    let force_key_frames = format!(
                "expr:gte(t,if(eq(n_forced,0),0,{HLS_INITIAL_SEGMENT_SECONDS}+(n_forced-1)*{HLS_SEGMENT_SECONDS}))"
            );
                    let filter = match pipeline {
                        hardware::Pipeline::QsvDecode => {
                            hardware::scale(probe.video()?, width, height)
                        }
                        hardware::Pipeline::QsvEncode => format!(
                            "{},format=nv12,hwupload=extra_hw_frames=64",
                            transforms.join(",")
                        ),
                        hardware::Pipeline::VaapiDecode | hardware::Pipeline::VaapiEncode => {
                            hardware::vaapi_filter(
                                pipeline,
                                tone_map,
                                probe.video()?,
                                width,
                                height,
                                deinterlace.as_deref(),
                                &transforms.join(","),
                            )
                        }
                        _ => transforms.join(","),
                    };
                    cmd.arg("-vf").arg(filter);
                    if hdr {
                        cmd.args([
                            "-color_primaries",
                            "bt709",
                            "-color_trc",
                            "bt709",
                            "-colorspace",
                            "bt709",
                            "-color_range",
                            "tv",
                        ]);
                    }
                    if pipeline.vaapi() {
                        // Same envelope as Quick Sync. `-level:v 4` is h264_vaapi's
                        // named constant for level 4.0 (a literal 4.0 parses as 4).
                        cmd.args([
                            "-c:v",
                            "h264_vaapi",
                            "-profile:v",
                            "main",
                            "-level:v",
                            "4",
                            "-rc_mode",
                            "VBR",
                            "-b:v",
                            "4000k",
                            "-maxrate",
                            "5000k",
                            "-bufsize",
                            "10000k",
                            "-bf",
                            "0",
                            "-fpsmax",
                            "30",
                            "-g",
                            &gop,
                            // VAAPI encodes every forced I-frame as an IDR.
                            "-force_key_frames",
                            &force_key_frames,
                        ]);
                    } else if pipeline.accelerated() {
                        cmd.args([
                            "-c:v",
                            "h264_qsv",
                            "-preset",
                            "veryfast",
                            "-profile:v",
                            "main",
                            "-level:v",
                            "4.0",
                            "-b:v",
                            "4000k",
                            "-maxrate",
                            "5000k",
                            "-bufsize",
                            "10000k",
                            "-look_ahead",
                            "0",
                            "-async_depth",
                            "1",
                            "-bf",
                            "0",
                            "-fpsmax",
                            "30",
                            "-g",
                            &gop,
                            "-forced_idr",
                            "1",
                            "-force_key_frames",
                            &force_key_frames,
                        ]);
                    } else {
                        // No `-tune zerolatency`: ultrafast already disables B-frames,
                        // lookahead and mbtree, so the tune mainly swapped frame threads
                        // for sliced threads. That saves a few frames of encoder delay,
                        // which segmented HLS never sees, and cost throughput (measured
                        // 10% on 4 threads with FFmpeg 5.1, 43% on 20) and efficiency.
                        cmd.args([
                            "-c:v",
                            "libx264",
                            "-preset",
                            "ultrafast",
                            "-profile:v",
                            "main",
                            "-level:v",
                            "4.0",
                            "-pix_fmt",
                            "yuv420p",
                            "-crf",
                            "23",
                            "-maxrate",
                            "5000k",
                            "-bufsize",
                            "10000k",
                            "-fpsmax",
                            "30",
                            "-g",
                            &gop,
                            "-keyint_min",
                            &gop,
                            "-sc_threshold",
                            "0",
                            "-flags",
                            "+cgop",
                            "-x264-params",
                            "open-gop=0",
                            "-forced-idr",
                            "1",
                            "-force_key_frames",
                            &force_key_frames,
                        ]);
                    }
                }
                if has_audio {
                    if copy_audio {
                        cmd.args(["-c:a", "copy"]);
                    } else {
                        cmd.args(["-c:a", "aac", "-b:a", "128k", "-ac", "2", "-ar", "48000"]);
                    }
                }
                if continuous && copy_video {
                    cmd.args([
                        "-f",
                        "mp4",
                        "-movflags",
                        "frag_keyframe+empty_moov+default_base_moof",
                        "-frag_duration",
                        "2000000",
                    ]);
                    cmd.args(["-progress", "pipe:2", "-stats_period", "0.1"])
                        .arg("pipe:1")
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped());
                    let mut child = cmd
                        .spawn()
                        .map_err(|_| "Playback engine unavailable".to_owned())?;
                    let output = child.stdout.take().ok_or("Media output unavailable")?;
                    let progress = child.stderr.take().ok_or("Media progress unavailable")?;
                    let stream =
                        fmp4::Stream::new(output, progress, position, session.permits.clone());
                    session.child = Some(child);
                    session.stream = Some(stream.clone());
                    session.throttle = if live {
                        Throttle::default()
                    } else {
                        Throttle::on_demand()
                    };
                    stream.ready().await?;
                    let _ = tokio::fs::remove_dir(&dir).await;
                    session.dir = PathBuf::new();
                    self.sessions.lock().await.insert(id.clone(), session);
                    tracing::info!(
                        probe_ms,
                        engine_ready_ms = engine_started.elapsed().as_millis() as u64,
                        video_mode = "copy",
                        "Streaming fragmented MP4 ready"
                    );
                    return Ok(PlaybackResponse {
                        id: id.clone(),
                        url: format!("/media/{id}/{capability}/stream.mp4"),
                        format: "fmp4".into(),
                        mode: if remux { "remux" } else { "transcode" }.into(),
                        video_mode: "copy".into(),
                        audio_mode: if !has_audio {
                            "none"
                        } else if copy_audio {
                            "copy"
                        } else {
                            "encode"
                        }
                        .into(),
                        position,
                        live,
                        duration: duration.unwrap_or(0.0),
                        audio_tracks,
                        subtitle_tracks,
                        selected_audio,
                        selected_subtitle: None,
                        subtitles_supported: false,
                    });
                }
                if let Some(subtitle) = selected_subtitle.as_ref().filter(|_| !shared_captions) {
                    let language = subtitle.language.as_deref().unwrap_or("und");
                    let audio_map = if selected_audio.is_some() { "a:0," } else { "" };
                    // Keep AV and WebVTT on the same post-seek timestamp clock. Without
                    // copyts, the nested MPEGTS muxer adds a private offset absent from VTT.
                    cmd.args(["-hls_segment_options", "mpegts_copyts=1", "-var_stream_map"])
                        .arg(format!(
                            "v:0,{audio_map}s:0,sgroup:subs,language:{language}"
                        ));
                }
                let initial_segment_seconds = HLS_INITIAL_SEGMENT_SECONDS.to_string();
                let segment_seconds = HLS_SEGMENT_SECONDS.to_string();
                let list_size = (HLS_WINDOW_SECONDS / HLS_SEGMENT_SECONDS).to_string();
                let delete_threshold = (HLS_DELETE_GRACE_SECONDS / HLS_SEGMENT_SECONDS).to_string();
                cmd.args([
                    "-max_muxing_queue_size",
                    "1024",
                    "-f",
                    "hls",
                    "-hls_init_time",
                    &initial_segment_seconds,
                ]);
                let hls_flags = if copy_video {
                    if caps.browser.is_some() {
                        "delete_segments+temp_file+independent_segments"
                    } else {
                        "delete_segments+temp_file+split_by_time"
                    } // Preserve the legacy delivery contract during rollout.
                } else {
                    "delete_segments+temp_file"
                };
                let fmp4_hls = (!copy_video || hevc_copy)
                    && selected_subtitle.is_none()
                    && caps.browser.as_ref().is_some_and(|browser| browser.fmp4);
                if fmp4_hls {
                    cmd.args([
                        "-hls_segment_type",
                        "fmp4",
                        "-hls_fmp4_init_filename",
                        "init.mp4",
                    ]);
                }
                cmd.args([
                    "-hls_time",
                    &segment_seconds,
                    "-hls_list_size",
                    &list_size,
                    "-hls_delete_threshold",
                    &delete_threshold,
                    "-hls_flags",
                    hls_flags,
                    "-hls_segment_filename",
                ]);
                cmd.arg(dir.join(if fmp4_hls {
                    "segment-%09d.m4s"
                } else {
                    "segment-%09d.ts"
                }))
                .arg(dir.join("index.m3u8"));
                session.child = Some(
                    cmd.spawn()
                        .map_err(|_| "Playback engine unavailable".to_owned())?,
                );
                // Do not return a URL until both the playlist and first segment exist.
                let ready = timeout(Duration::from_secs(if pipeline.accelerated() { 12 } else { 30 }), async {
            loop {
                if !cache_safe(&dir, false).await {
                    return false;
                }
                if let Some(child) = session.child.as_mut() {
                    if let Ok(Some(status)) = child.try_wait() {
                        if !status.success() {
                            tracing::warn!(exit_code = ?status.code(), "Playback engine startup failed");
                            return false;
                        }
                        return if shared_captions {hls::shared_caption_ready(&dir,selected_subtitle.as_ref()).await} else {playback_ready(&dir, selected_subtitle.as_ref()).await};
                    }
                }
                if if shared_captions {hls::shared_caption_ready(&dir,selected_subtitle.as_ref()).await} else {playback_ready(&dir, selected_subtitle.as_ref()).await} {
                    return true;
                }
                sleep(Duration::from_millis(150)).await;
            }
        })
        .await
        .unwrap_or(false);
                if ready {
                    break;
                }
                if let Some(next) = pipeline.fallback() {
                    if let Some(mut child) = session.child.take() {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                    }
                    // A failed attempt may have emitted a partial playlist. Never reuse it.
                    tokio::fs::remove_dir_all(&dir)
                        .await
                        .map_err(|_| "Playback cleanup failed")?;
                    tokio::fs::create_dir(&dir)
                        .await
                        .map_err(|_| "Playback storage unavailable")?;
                    tracing::warn!(from = ?pipeline, to = ?next, "Retrying playback pipeline");
                    pipeline = next;
                    continue;
                }
                if !ready {
                    session.cleanup().await;
                    return Err(
                        "Playback could not start; try forced transcoding or another stream".into(),
                    );
                }
            }
            let engine_ready_ms = engine_started.elapsed().as_millis() as u64;
            tracing::info!(
                probe_ms,
                engine_ready_ms,
                encoder = pipeline.encoder(),
                pipeline = ?pipeline,
                tone_map = ?if pipeline.vaapi() {
                    tone_map
                } else {
                    hdr.then_some(hardware::ToneMap::Software)
                },
                live,
                video_mode = if copy_video { "copy" } else { "encode" },
                audio_mode = if !has_audio {
                    "none"
                } else if copy_audio {
                    "copy"
                } else {
                    "encode"
                },
                "Playback preparation completed"
            );
            session.touched = Instant::now();
            self.sessions.lock().await.insert(id.clone(), session);
            Ok(PlaybackResponse {
                url: format!(
                    "/media/{id}/{capability}/{}",
                    if selected_subtitle.is_some() {
                        "master.m3u8"
                    } else {
                        "index.m3u8"
                    }
                ),
                id,
                format: "hls".into(),
                mode: if remux { "remux" } else { "transcode" }.into(),
                video_mode: if copy_video { "copy" } else { "encode" }.into(),
                audio_mode: if !has_audio {
                    "none"
                } else if copy_audio {
                    "copy"
                } else {
                    "encode"
                }
                .into(),
                position,
                live,
                duration: duration.unwrap_or(0.0),
                audio_tracks,
                subtitles_supported: subtitle_tracks.iter().any(|track| track.supported),
                subtitle_tracks,
                selected_audio,
                selected_subtitle,
            })
        }))
    }
}
