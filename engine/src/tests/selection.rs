use super::*;

#[tokio::test]
#[ignore = "requires real FFmpeg/libx264 and ffprobe; loopback fixtures only"]
async fn real_audio_selection_accepts_tagged_and_unknown_languages() {
    let ffmpeg = PathBuf::from(std::env::var("PLAYBACK_TEST_FFMPEG").unwrap());
    let ffprobe = PathBuf::from(std::env::var("PLAYBACK_TEST_FFPROBE").unwrap());
    let root = tempfile::tempdir().unwrap();
    for (case, languages) in [
        ("dual", vec!["ita", "eng"]),
        ("english", vec!["eng"]),
        ("italian", vec!["ita"]),
        ("unknown", vec!["und"]),
        ("required_italian", vec!["ita", "eng"]),
        ("required_english", vec!["ita", "eng"]),
        ("required_override", vec!["ita", "eng"]),
    ] {
        let fixture = root.path().join(format!("{case}.mp4"));
        let mut command = Command::new(&ffmpeg);
        command.args([
            "-v",
            "error",
            "-nostdin",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=160x90:rate=24",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000",
            "-map",
            "0:v",
        ]);
        for (i, language) in languages.iter().enumerate() {
            command.args(["-map", if *language == "ita" { "1:a" } else { "2:a" }]);
            command
                .arg(format!("-metadata:s:a:{i}"))
                .arg(format!("language={language}"));
        }
        command
            .args([
                "-t",
                "6",
                "-c:v",
                "libx264",
                "-threads",
                "2",
                "-pix_fmt",
                "yuv420p",
                "-profile:v",
                "main",
                "-level:v",
                "3.0",
                "-g",
                "24",
                "-c:a",
                "aac",
                "-ac",
                "2",
                "-movflags",
                "+faststart",
            ])
            .arg(&fixture);
        assert!(timeout(Duration::from_secs(30), command.status())
            .await
            .unwrap()
            .unwrap()
            .success());
        let bytes = tokio::fs::read(&fixture).await.unwrap();
        let router = axum::Router::new().route(
            "/fixture.mp4",
            axum::routing::get(move || {
                let b = bytes.clone();
                async move { b }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/fixture.mp4", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let media = root.path().join(format!("media-{case}"));
        let manager = PlaybackManager::new(Config {
            ffmpeg: ffmpeg.clone(),
            ffprobe: ffprobe.clone(),
            root: media.clone(),
            max_sessions: 1,
            ttl: Duration::from_secs(60),
        });
        let response = manager
            .start_with_selection(
                url.clone(),
                HashMap::new(),
                0.0,
                None,
                false,
                false,
                None,
                TrackSelection {
                    required_audio_index: match case {
                        "required_italian" | "required_override" => Some(0),
                        "required_english" => Some(1),
                        _ => None,
                    },
                    audio_track_index: (case == "required_override").then_some(2),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let selected = response.selected_audio.as_ref().unwrap();
        assert_eq!(selected.input_index, if matches!(case,"dual"|"required_english"|"required_override") { 2 } else { 1 });
        assert_eq!(selected.output_index, 0);
        assert_eq!(selected.output_audio_ordinal, 0);
        assert_eq!(selected.output_stream_index, 1);
        assert!(selected.disposition.is_some());
        assert_eq!(
            selected.language_status,
            match case {
                "italian" | "required_italian" => "tagged_non_english",
                "dual" | "english" | "required_english" | "required_override" => "tagged_english",
                "unknown" => "unknown",
                _ => unreachable!(),
            }
        );
        let capability = response.url.split('/').nth(3).unwrap();
        let playlist = String::from_utf8(
            axum::body::to_bytes(
                manager
                    .serve(&response.id, capability, "index.m3u8")
                    .await
                    .unwrap()
                    .into_body(),
                1024 * 1024,
            )
            .await
            .unwrap()
            .to_vec(),
        )
        .unwrap();
        let segment = playlist
            .lines()
            .find(|line| media_type(line) == Some("video/mp2t"))
            .unwrap();
        let output = Command::new(&ffmpeg)
            .args(["-v", "error", "-nostdin", "-i"])
            .arg(media.join(&response.id).join(segment))
            .args([
                "-map", "0:a:0", "-t", "1", "-ac", "1", "-ar", "8000", "-f", "s16le", "pipe:1",
            ])
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        let samples: Vec<i16> = output
            .stdout
            .as_chunks::<2>()
            .0
            .iter()
            .map(|s| i16::from_le_bytes(*s))
            .collect();
        assert!(samples.len() > 4000);
        let crossings = samples.windows(2).filter(|s| s[0] <= 0 && s[1] > 0).count();
        let frequency = crossings as f64 * 8000.0 / samples.len() as f64;
        let expected_frequency = if matches!(case,"italian"|"required_italian") { 440.0 } else { 880.0 };
        assert!(
            ((expected_frequency - 40.0)..(expected_frequency + 40.0)).contains(&frequency),
            "Wrong selected audio frequency for {case}: {frequency}"
        );
        manager.stop(&response.id).await;
        assert_eq!(manager.active_count().await, 0);
        server.abort();
    }
}

#[tokio::test]
#[ignore = "requires real FFmpeg/libx264/WebVTT and ffprobe; loopback only"]
async fn real_text_subtitle_hls_selection_seek_and_sparse_startup() {
    async fn served_media(
        manager: &PlaybackManager,
        id: &str,
        capability: &str,
        path: &str,
        expected_mime: &str,
    ) -> Vec<u8> {
        let file = path.rsplit('/').next().unwrap();
        let served = manager.serve(id, capability, file).await.unwrap();
        assert_eq!(
            served
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            expected_mime
        );
        axum::body::to_bytes(served.into_body(), 64 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec()
    }
    let ffmpeg = PathBuf::from(std::env::var("PLAYBACK_TEST_FFMPEG").unwrap());
    let ffprobe = PathBuf::from(std::env::var("PLAYBACK_TEST_FFPROBE").unwrap());
    let root = tempfile::tempdir().unwrap();
    for late in [false, true] {
        let suffix = if late { "late" } else { "normal" };
        let captions = root.path().join(format!("{suffix}.srt"));
        tokio::fs::write(&captions,if late {"1\n00:00:30,000 --> 00:00:35,000\nLate first caption.\n"} else {"1\n00:00:00,000 --> 00:00:03,000\nEnglish fixture caption.\n\n2\n00:00:04,000 --> 00:00:07,000\nSecond caption after seek.\n"}).await.unwrap();
        let fixture = root.path().join(format!("{suffix}.mkv"));
        let status = Command::new(&ffmpeg)
            .args([
                "-v",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=160x90:rate=24",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=880:sample_rate=48000",
                "-i",
            ])
            .arg(&captions)
            .args([
                "-t",
                if late { "36" } else { "8" },
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-map",
                "2:s",
                "-c:v",
                "libx264",
                "-threads",
                "2",
                "-g",
                "24",
                "-c:a",
                "aac",
                "-c:s",
                "srt",
                "-metadata:s:a:0",
                "language=eng",
                "-metadata:s:s:0",
                "language=eng",
                "-metadata:s:s:0",
                "title=English CC",
            ])
            .arg(&fixture)
            .status()
            .await
            .unwrap();
        assert!(status.success());
        if !late {
            let rolling = root.path().join("rolling");
            tokio::fs::create_dir(&rolling).await.unwrap();
            let mut command = Command::new(&ffmpeg);
            command
                .args(["-v", "error", "-nostdin", "-stream_loop", "40", "-i"])
                .arg(&fixture)
                .args([
                    "-map",
                    "0:v",
                    "-map",
                    "0:a",
                    "-map",
                    "0:s",
                    "-c:v",
                    "copy",
                    "-c:a",
                    "copy",
                    "-c:s",
                    "webvtt",
                    "-max_interleave_delta",
                    "1000000",
                    "-hls_segment_options",
                    "mpegts_copyts=1",
                    "-f",
                    "hls",
                    "-hls_time",
                    &HLS_SEGMENT_SECONDS.to_string(),
                    "-hls_list_size",
                    &(HLS_WINDOW_SECONDS / HLS_SEGMENT_SECONDS).to_string(),
                    "-hls_delete_threshold",
                    &(HLS_DELETE_GRACE_SECONDS / HLS_SEGMENT_SECONDS).to_string(),
                    "-hls_flags",
                    "delete_segments+temp_file",
                    "-var_stream_map",
                    "v:0,a:0,s:0,sgroup:subs,language:eng",
                    "-hls_segment_filename",
                ])
                .arg(rolling.join("segment-%09d.ts"))
                .arg(rolling.join("index.m3u8"));
            assert!(timeout(Duration::from_secs(30), command.status())
                .await
                .unwrap()
                .unwrap()
                .success());
            let mut entries = tokio::fs::read_dir(&rolling).await.unwrap();
            let mut total = 0;
            let mut captions = 0;
            while let Some(entry) = entries.next_entry().await.unwrap() {
                total += 1;
                if media_type(&entry.file_name().to_string_lossy()) == Some("text/vtt") {
                    captions += 1;
                }
            }
            let retained = (HLS_WINDOW_SECONDS + HLS_DELETE_GRACE_SECONDS) / HLS_SEGMENT_SECONDS;
            assert!(
                total <= retained * 2 + 3 && captions > 0 && captions <= retained,
                "VTT segments leaked past rolling retention"
            );
            assert!(cache_safe(&rolling, false).await);
        }
        let bytes = tokio::fs::read(&fixture).await.unwrap();
        let router = axum::Router::new().route(
            "/fixture.mkv",
            axum::routing::get(move || {
                let b = bytes.clone();
                async move { b }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/fixture.mkv", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let media = root.path().join(format!("media-{suffix}"));
        let manager = PlaybackManager::new(Config {
            ffmpeg: ffmpeg.clone(),
            ffprobe: ffprobe.clone(),
            root: media.clone(),
            max_sessions: 1,
            ttl: Duration::from_secs(60),
        });
        let provider = Arc::new(Semaphore::new(1));
        let cases = if late {
            vec![(true, 0.0)]
        } else {
            vec![(true, 0.0), (true, 2.0), (false, 2.0)]
        };
        for (enabled, position) in cases {
            let start = Instant::now();
            let response = manager
                .start_with_selection(
                    url.clone(),
                    HashMap::new(),
                    position,
                    None,
                    false,
                    false,
                    Some(provider.clone().try_acquire_owned().unwrap()),
                    TrackSelection {
                        audio_track_index: Some(1),
                        subtitle_track_index: (enabled && position > 0.0).then_some(2),
                        preferred_subtitle_language: (enabled && position == 0.0)
                            .then(|| "en".into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert!(
                start.elapsed() < Duration::from_secs(12),
                "Sparse captions delayed AV startup"
            );
            assert_eq!(provider.available_permits(), 0);
            assert_eq!(response.position, position);
            assert!(response.subtitles_supported);
            let track = &response.subtitle_tracks[0];
            assert_eq!(track.title, "English CC");
            assert!(track.supported);
            assert_eq!(track.selected, enabled);
            let cap = response.url.split('/').nth(3).unwrap();
            if enabled {
                assert!(response.url.ends_with("master.m3u8"));
                assert_eq!(response.selected_subtitle.as_ref().unwrap().input_index, 2);
                assert_eq!(
                    response
                        .selected_subtitle
                        .as_ref()
                        .unwrap()
                        .output_stream_index,
                    0
                );
                let base =
                    reqwest::Url::parse(&format!("http://client.invalid{}", response.url)).unwrap();
                let master = served_media(
                    &manager,
                    &response.id,
                    cap,
                    base.path(),
                    "application/vnd.apple.mpegurl",
                )
                .await;
                let master = String::from_utf8(master).unwrap();
                assert!(
                    master.contains("SUBTITLES=\"subs\""),
                    "Incomplete master at position {position}: {master}"
                );
                let quoted = master
                    .split("URI=\"")
                    .nth(1)
                    .unwrap()
                    .split('"')
                    .next()
                    .unwrap();
                let subtitle_url = base.join(quoted).unwrap();
                assert_eq!(
                    subtitle_url.path(),
                    format!("/media/{}/{cap}/index_vtt.m3u8", response.id)
                );
                let playlist = served_media(
                    &manager,
                    &response.id,
                    cap,
                    subtitle_url.path(),
                    "application/vnd.apple.mpegurl",
                )
                .await;
                let playlist = String::from_utf8(playlist).unwrap();
                let file = playlist
                    .lines()
                    .find(|line| media_type(line) == Some("text/vtt"))
                    .unwrap();
                let vtt_url = subtitle_url.join(file).unwrap();
                assert_eq!(
                    vtt_url.path(),
                    format!("/media/{}/{cap}/{file}", response.id)
                );
                let vtt =
                    served_media(&manager, &response.id, cap, vtt_url.path(), "text/vtt").await;
                let mut vtt = String::from_utf8(vtt).unwrap();
                // A ready sparse rendition may publish an empty first segment.
                // Wait for the expected cue publication independently of AV readiness,
                // both at initial playback and after a seek.
                if !late {
                    let expected = if position > 0.0 {
                        "Second caption after seek."
                    } else {
                        "English fixture caption."
                    };
                    for _ in 0..30 {
                        if vtt.contains(expected) {
                            break;
                        }
                        sleep(Duration::from_millis(100)).await;
                        let latest = served_media(
                            &manager,
                            &response.id,
                            cap,
                            subtitle_url.path(),
                            "application/vnd.apple.mpegurl",
                        )
                        .await;
                        let latest = String::from_utf8(latest).unwrap();
                        for name in latest
                            .lines()
                            .filter(|line| media_type(line) == Some("text/vtt"))
                        {
                            let caption_url = subtitle_url.join(name).unwrap();
                            let bytes = served_media(
                                &manager,
                                &response.id,
                                cap,
                                caption_url.path(),
                                "text/vtt",
                            )
                            .await;
                            vtt.push_str(&String::from_utf8(bytes).unwrap());
                        }
                    }
                }
                assert!(vtt.starts_with("WEBVTT"));
                assert!(vtt.contains("X-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:0"));
                assert!(manager.serve(&response.id, "wrong", file).await.is_err());
                if !late {
                    assert!(
                        vtt.contains(if position > 0.0 {
                            "Second caption after seek."
                        } else {
                            "English fixture caption."
                        }),
                        "Missing expected caption at position {position}; received: {vtt}"
                    );
                    if position > 0.0 {
                        assert!(
                            vtt.contains("00:02."),
                            "Caption timestamps were not rebased after seek: {vtt}"
                        );
                    }
                }
                let av_uri = master
                    .lines()
                    .find(|line| !line.is_empty() && !line.starts_with('#'))
                    .unwrap();
                let av_url = base.join(av_uri).unwrap();
                assert_eq!(
                    av_url.path(),
                    format!("/media/{}/{cap}/index.m3u8", response.id)
                );
                let av = served_media(
                    &manager,
                    &response.id,
                    cap,
                    av_url.path(),
                    "application/vnd.apple.mpegurl",
                )
                .await;
                let av = String::from_utf8(av).unwrap();
                let segment = av
                    .lines()
                    .find(|line| media_type(line) == Some("video/mp2t"))
                    .unwrap();
                let probe = Command::new(&ffprobe)
                    .args([
                        "-v",
                        "error",
                        "-select_streams",
                        "v:0",
                        "-show_entries",
                        "packet=pts_time",
                        "-of",
                        "json",
                    ])
                    .arg(media.join(&response.id).join(segment))
                    .output()
                    .await
                    .unwrap();
                assert!(probe.status.success());
                let packets: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
                let pts = packets["packets"][0]["pts_time"]
                    .as_str()
                    .unwrap()
                    .parse::<f64>()
                    .unwrap();
                assert!(
                    (0.0..0.2).contains(&pts),
                    "AV/VTT clocks disagree: first video PTS {pts}"
                );
            } else {
                assert!(response.url.ends_with("index.m3u8"));
                assert!(response.selected_subtitle.is_none());
                assert!(!media.join(&response.id).join("index_vtt.m3u8").exists());
            }
            manager.stop(&response.id).await;
            assert_eq!(provider.available_permits(), 1);
            assert_eq!(manager.active_count().await, 0);
            assert!(!media.join(&response.id).exists());
        }
        server.abort();
    }
}
