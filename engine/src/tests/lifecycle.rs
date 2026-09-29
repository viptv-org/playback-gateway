use super::*;

#[tokio::test]
async fn open_segment_growth_and_stalled_leased_process_are_reaped() {
    let root = tempfile::tempdir().unwrap();
    let manager = PlaybackManager::new(Config {
        ffmpeg: "missing".into(),
        ffprobe: "missing".into(),
        root: root.path().into(),
        max_sessions: 1,
        ttl: Duration::from_secs(60),
    });
    manager.initialize().await.unwrap();
    let dir = root.path().join("fixture");
    tokio::fs::create_dir(&dir).await.unwrap();
    let segment = tokio::fs::File::create(dir.join("segment-000000000.ts.tmp"))
        .await
        .unwrap();
    segment.set_len(32 * 1024 * 1024 + 1).await.unwrap();
    assert!(
        !cache_safe(&dir, false).await,
        "open segments must be counted"
    );
    segment.set_len(1).await.unwrap();
    assert!(cache_safe(&dir, false).await);
    assert!(
        !cache_safe(&dir, true).await,
        "active writer with no playlist has stalled"
    );
    let child = Command::new("/bin/sleep")
        .arg("60")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    manager.sessions.lock().await.insert(
        "id".into(),
        Session {
            stream: None,
            _source_probe: None,
            direct: None,
            capability: "cap".into(),
            dir: dir.clone(),
            child: Some(child),
            touched: Instant::now(),
            stable_target_duration: false,
            target_duration: None,
            supervised_live: false,
            throttle: Throttle::default(),
            permits: Arc::new(InputPermits {
                _playback: manager.slots.clone().try_acquire_owned().unwrap(),
                _provider: None,
            }),
            cleanup_tasks: manager.cleanup_tasks.clone(),
        },
    );
    assert!(manager.heartbeat("id").await);
    manager.reap().await;
    assert!(!dir.exists());
    assert_eq!(manager.active_count().await, 0);
    assert!(manager.slots.clone().try_acquire_owned().is_ok());
    manager.shutdown().await;
}

#[test]
fn full_source_duration_is_optional_and_validated() {
    for value in [serde_json::json!("20.5"), serde_json::json!(20.5)] {
        let probe: Probe =
            serde_json::from_value(serde_json::json!({"streams":[],"format":{"duration":value}}))
                .unwrap();
        assert_eq!(probe.duration(), Some(20.5));
    }
    for value in [
        serde_json::json!("N/A"),
        serde_json::json!("NaN"),
        serde_json::json!(-1),
        serde_json::Value::Null,
    ] {
        let probe: Probe =
            serde_json::from_value(serde_json::json!({"streams":[],"format":{"duration":value}}))
                .unwrap();
        assert_eq!(probe.duration(), None);
    }
}

#[tokio::test]
async fn live_sources_reject_resume_offsets() {
    let root = tempfile::tempdir().unwrap();
    let manager = PlaybackManager::new(Config {
        ffmpeg: "missing".into(),
        ffprobe: "missing".into(),
        root: root.path().into(),
        max_sessions: 1,
        ttl: Duration::from_secs(30),
    });
    let error = manager
        .start_with_kind(
            "https://example.com/live.ts".into(),
            HashMap::new(),
            10.0,
            None,
            false,
            true,
        )
        .await
        .unwrap_err();
    assert!(error.contains("Live playback"));
    manager.shutdown().await;
}

#[test]
fn rejects_header_injection() {
    assert!(header_block(&HashMap::from([(
        "User-Agent".into(),
        "test\r\nHost: evil".into()
    )]))
    .is_err());
    assert!(header_block(&HashMap::from([("Bad:Key".into(), "test".into())])).is_err());
    assert_eq!(
        header_block(&HashMap::from([("User-Agent".into(), "player".into())])).unwrap(),
        "User-Agent: player\r\n"
    );
}

#[test]
fn served_hls_target_duration_is_stable_from_initial_to_steady_segments() {
    for (advertised, expected) in [("1", "2"), ("2", "2"), ("99", "99")] {
        let input = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:{advertised}\n#EXTINF:1.0,\nsegment-000000000.ts\n"
        );
        let output = String::from_utf8(stable_hls_target_duration(input.into_bytes())).unwrap();
        assert!(output.contains(&format!("#EXT-X-TARGETDURATION:{expected}\n")));
    }
    let master = b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nindex.m3u8\n".to_vec();
    assert_eq!(stable_hls_target_duration(master.clone()), master);
}

#[test]
fn serving_only_accepts_generated_names() {
    for bad in [
        "../index.m3u8",
        "segment-../../etc/passwd.ts",
        "segment-000000001.ts.tmp",
        "/index.m3u8",
        "segment-%2e%2e.ts",
        "index../private.vtt",
        "index.vtt",
        "index0.vtt.tmp",
        "index00000000000000000.vtt",
        "master.m3u8.tmp",
        "secret.txt",
    ] {
        assert!(media_type(bad).is_none());
    }
    assert!(media_type("segment-000000001.ts").is_some());
    assert_eq!(media_type("index0.vtt"), Some("text/vtt"));
    assert_eq!(
        media_type("index_vtt.m3u8"),
        Some("application/vnd.apple.mpegurl")
    );
    assert_eq!(
        media_type("master.m3u8"),
        Some("application/vnd.apple.mpegurl")
    );
    assert!(constant_time_eq(b"secret", b"secret"));
    assert!(!constant_time_eq(b"secret", b"secrex"));
}

#[test]
fn dimensions_are_bounded_even_and_filter_never_upscales() {
    let mut caps = Capabilities {
        max_width: 9999,
        max_height: 721,
        ..Capabilities::default()
    };
    assert_eq!(dimensions(&caps).unwrap(), (9998, 720));
    caps.max_width = 3840;
    caps.max_height = 2160;
    assert_eq!(dimensions(&caps).unwrap(), (3840, 2160));
    assert!(scale_filter(1280, 720).contains("min(iw,1280)"));
    assert!(scale_filter(1280, 720).contains("force_divisible_by=2"));
    caps.max_height = 1;
    assert!(dimensions(&caps).is_err());
}

#[test]
fn remux_rates_admit_up_to_sixty_frames_per_second() {
    for rate in [
        "24/1",
        "24000/1001",
        "30000/1001",
        "30/1",
        "50/1",
        // A 720p60 channel is an ordinary source; the old 30fps ceiling
        // silently forced a full re-encode of it.
        "60000/1001",
        "60/1",
    ] {
        assert!(conservative_frame_rate(Some(rate)), "{rate}");
    }
    for rate in [
        "0/0",
        "0/1",
        "30/0",
        "120/1",
        "120000/1001",
        "NaN",
        "30",
        "-1/1",
        "1/1/1",
        "18446744073709551616/1",
        "1/",
    ] {
        assert!(!conservative_frame_rate(Some(rate)), "{rate}");
    }
    assert!(!conservative_frame_rate(None));
}

#[test]
fn private_egress_is_an_input_option_not_an_upstream_header() {
    let mut cmd = Command::new("ffmpeg");
    cmd.env("no_proxy", "*")
        .env("HTTP_PROXY", "http://untrusted.invalid");
    input_args(
        &mut cmd,
        "x-playback-egress-proxy: http://warp:8899\r\nUser-Agent: VIPTV\r\n",
    );
    let args = cmd
        .as_std()
        .get_args()
        .map(|v| v.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(args
        .windows(2)
        .any(|v| v == ["-http_proxy", "http://warp:8899"]));
    assert!(args
        .windows(2)
        .any(|v| v == ["-headers", "User-Agent: VIPTV\r\n"]));
    assert!(!args.iter().any(|v| v.contains("x-playback-egress-proxy")));
    for variable in [
        "no_proxy",
        "NO_PROXY",
        "http_proxy",
        "HTTP_PROXY",
        "https_proxy",
        "HTTPS_PROXY",
        "all_proxy",
        "ALL_PROXY",
    ] {
        assert!(cmd
            .as_std()
            .get_envs()
            .any(|(key, value)| key == variable && value.is_none()));
    }
}

#[test]
fn input_protocols_allow_network_crypto_but_not_local_files() {
    let mut cmd = Command::new("ffmpeg");
    input_args(&mut cmd, "");
    let args: Vec<_> = cmd
        .as_std()
        .get_args()
        .map(|arg| arg.to_str().unwrap())
        .collect();
    let whitelist = args[args
        .iter()
        .position(|arg| *arg == "-protocol_whitelist")
        .unwrap()
        + 1];
    assert_eq!(whitelist, "http,https,httpproxy,tcp,tls,crypto");
    for (key, value) in [
        ("-reconnect", "1"),
        ("-reconnect_streamed", "1"),
        ("-reconnect_delay_max", "2"),
        ("-rw_timeout", "10000000"),
    ] {
        let index = args.iter().position(|arg| *arg == key).unwrap();
        assert_eq!(args[index + 1], value);
    }
    assert!(!args.contains(&"-reconnect_at_eof"));
    assert!(!args.contains(&"-reconnect_on_http_error"));
    assert!(!whitelist
        .split(',')
        .any(|p| matches!(p, "file" | "pipe" | "concat" | "subfile")));
}

#[test]
fn remux_requires_known_compatible_streams() {
    let good = r#"{"streams":[{"codec_type":"video","codec_name":"h264","width":1280,"height":720,"pix_fmt":"yuv420p","profile":"High","level":40,"avg_frame_rate":"30000/1001","r_frame_rate":"30000/1001"},{"codec_type":"audio","codec_name":"aac","profile":"LC","channels":2}]}"#;
    let probe: Probe = serde_json::from_str(good).unwrap();
    assert!(probe.compatible(1280, 720));
    assert!(probe.compatible_video(1280, 720, 40));
    assert!(probe.compatible_audio_stream(
        probe
            .streams
            .iter()
            .find(|stream| stream.codec_type.as_deref() == Some("audio"))
    ));
    assert!(!probe.compatible(640, 480));
    let multichannel: Probe =
        serde_json::from_str(&good.replace("\"channels\":2", "\"channels\":6")).unwrap();
    let multichannel_audio = multichannel
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("audio"));
    assert!(multichannel.compatible_video(1280, 720, 40));
    assert!(!multichannel.compatible_audio_stream(multichannel_audio));
    assert!(!multichannel.compatible(1280, 720));
    let level_41: Probe =
        serde_json::from_str(&good.replace("\"level\":40", "\"level\":41")).unwrap();
    // A 720p source at level 4.1 is the common live case and must be copied.
    assert!(level_41.compatible(1280, 720));
    assert!(level_41.compatible(1920, 1080));
    // Level 5.1 is the modern browser ceiling and now copies as-is; only
    // levels beyond it were authored for hardware beyond that baseline.
    let level_51: Probe =
        serde_json::from_str(&good.replace("\"level\":40", "\"level\":51")).unwrap();
    assert!(level_51.compatible(1280, 720));
    assert!(level_51.compatible(1920, 1080));
    let level_52: Probe =
        serde_json::from_str(&good.replace("\"level\":40", "\"level\":52")).unwrap();
    assert!(!level_52.compatible(1280, 720));
    assert!(!level_52.compatible(1920, 1080));
    for bad in [
        good.replace("h264", "hevc"),
        good.replace("yuv420p", "yuv420p10le"),
        good.replace("\"channels\":2", "\"channels\":6"),
        good.replace("\"level\":40", "\"level\":52"),
        // Above the copyable rate, not merely above 30fps: a 720p60 channel
        // is now copied, so 120fps is the case that must still convert.
        good.replace(
            "\"avg_frame_rate\":\"30000/1001\"",
            "\"avg_frame_rate\":\"120/1\"",
        ),
        good.replace(
            "\"r_frame_rate\":\"30000/1001\"",
            "\"r_frame_rate\":\"120000/1001\"",
        ),
        good.replace(",\"avg_frame_rate\":\"30000/1001\"", ""),
        good.replace(",\"r_frame_rate\":\"30000/1001\"", ""),
    ] {
        assert!(!serde_json::from_str::<Probe>(&bad)
            .unwrap()
            .compatible(1920, 1080));
    }
    assert!(!serde_json::from_str::<Probe>(r#"{"streams":[]}"#)
        .unwrap()
        .compatible(1280, 720));
}

#[tokio::test]
async fn startup_cleanup_only_removes_generated_session_shapes_once() {
    let root = tempfile::tempdir().unwrap();
    let stale = root.path().join(Uuid::new_v4().to_string());
    let empty = root.path().join(Uuid::new_v4().to_string());
    let unrelated = root.path().join(Uuid::new_v4().to_string());
    let named = root.path().join("not-a-session");
    for dir in [&stale, &empty, &unrelated, &named] {
        tokio::fs::create_dir(dir).await.unwrap();
    }
    for file in [
        "index.m3u8",
        "index.m3u8.tmp",
        "segment-000000001.ts",
        "segment-000000002.ts.tmp",
    ] {
        tokio::fs::write(stale.join(file), b"fixture")
            .await
            .unwrap();
    }
    tokio::fs::write(unrelated.join("precious.txt"), b"keep")
        .await
        .unwrap();
    tokio::fs::write(named.join("index.m3u8"), b"keep")
        .await
        .unwrap();
    #[cfg(unix)]
    let links = {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("index.m3u8"), b"keep").unwrap();
        let directory_link = root.path().join(Uuid::new_v4().to_string());
        std::os::unix::fs::symlink(outside.path(), &directory_link).unwrap();
        let with_link = root.path().join(Uuid::new_v4().to_string());
        std::fs::create_dir(&with_link).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("index.m3u8"),
            with_link.join("index.m3u8"),
        )
        .unwrap();
        (outside, directory_link, with_link)
    };
    let manager = PlaybackManager::new(Config {
        ffmpeg: "ffmpeg".into(),
        ffprobe: "ffprobe".into(),
        root: root.path().into(),
        max_sessions: 1,
        ttl: Duration::from_secs(60),
    });
    manager.initialize().await.unwrap();
    assert!(!stale.exists() && !empty.exists());
    assert!(unrelated.join("precious.txt").exists() && named.join("index.m3u8").exists());
    #[cfg(unix)]
    assert!(
        links.0.path().join("index.m3u8").exists()
            && links.1.exists()
            && links.2.join("index.m3u8").exists()
    );
    let fresh = root.path().join(Uuid::new_v4().to_string());
    tokio::fs::create_dir(&fresh).await.unwrap();
    manager.initialize().await.unwrap();
    assert!(
        fresh.exists(),
        "initialization must never sweep an active generation twice"
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn capabilities_expiry_and_cleanup_are_enforced() {
    let root = tempfile::tempdir().unwrap();
    let manager = PlaybackManager::new(Config {
        ffmpeg: "ffmpeg".into(),
        ffprobe: "ffprobe".into(),
        root: root.path().into(),
        max_sessions: 1,
        ttl: Duration::from_secs(60),
    });
    let dir = root.path().join("session");
    tokio::fs::create_dir(&dir).await.unwrap();
    tokio::fs::write(dir.join("index.m3u8"), b"#EXTM3U\n")
        .await
        .unwrap();
    let permit = manager.slots.clone().try_acquire_owned().unwrap();
    manager.sessions.lock().await.insert(
        "id".into(),
        Session {
            stream: None,
            _source_probe: None,
            direct: None,
            capability: "capability".into(),
            dir: dir.clone(),
            child: None,
            touched: Instant::now(),
            stable_target_duration: false,
            target_duration: None,
            supervised_live: false,
            throttle: Throttle::default(),
            permits: Arc::new(InputPermits {
                _playback: permit,
                _provider: None,
            }),
            cleanup_tasks: manager.cleanup_tasks.clone(),
        },
    );
    assert_eq!(manager.active_count().await, 1);
    assert_eq!(manager.active_ids().await, vec!["id".to_owned()]);
    assert!(manager.slots.clone().try_acquire_owned().is_err());
    assert!(manager.serve("id", "wrong", "index.m3u8").await.is_err());
    assert!(manager
        .serve("id", "capability", "../index.m3u8")
        .await
        .is_err());
    let served = manager
        .serve("id", "capability", "index.m3u8")
        .await
        .unwrap();
    assert_eq!(
        served
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap(),
        "application/vnd.apple.mpegurl"
    );
    let bytes = axum::body::to_bytes(served.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), b"#EXTM3U\n");
    assert!(manager.heartbeat("id").await);
    manager.sessions.lock().await.get_mut("id").unwrap().touched =
        Instant::now() - Duration::from_secs(61);
    assert!(!manager.heartbeat("id").await);
    assert!(manager
        .serve("id", "capability", "index.m3u8")
        .await
        .is_err());
    manager.reap().await;
    assert_eq!(manager.active_count().await, 0);
    assert!(!dir.exists());
    assert!(manager.slots.clone().try_acquire_owned().is_ok());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn shutdown_reaps_children_and_cancelled_start_artifacts() {
    let root = tempfile::tempdir().unwrap();
    let manager = PlaybackManager::new(Config {
        ffmpeg: "ffmpeg".into(),
        ffprobe: "ffprobe".into(),
        root: root.path().into(),
        max_sessions: 2,
        ttl: Duration::from_secs(60),
    });
    let provider_slots = Arc::new(Semaphore::new(2));
    let mut pids = Vec::new();
    let mut dirs = Vec::new();
    for index in 0..2 {
        let dir = root.path().join(format!("session-{index}"));
        tokio::fs::create_dir(&dir).await.unwrap();
        tokio::fs::write(dir.join("index.m3u8"), b"#EXTM3U\n")
            .await
            .unwrap();
        let child = Command::new("/bin/sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        pids.push(child.id().unwrap());
        dirs.push(dir.clone());
        let session = Session {
            stream: None,
            _source_probe: None,
            direct: None,
            capability: "capability".into(),
            dir,
            child: Some(child),
            touched: Instant::now(),
            stable_target_duration: false,
            target_duration: None,
            supervised_live: false,
            throttle: Throttle::default(),
            permits: Arc::new(InputPermits {
                _playback: manager.slots.clone().try_acquire_owned().unwrap(),
                _provider: Some(provider_slots.clone().try_acquire_owned().unwrap()),
            }),
            cleanup_tasks: manager.cleanup_tasks.clone(),
        };
        if index == 0 {
            manager.sessions.lock().await.insert("id".into(), session);
        } else {
            drop(session);
        } // Simulate a cancelled startup request.
    }
    manager.shutdown().await;
    assert_eq!(manager.active_count().await, 0);
    assert_eq!(provider_slots.available_permits(), 2);
    for dir in dirs {
        assert!(!dir.exists());
    }
    for pid in pids {
        assert!(!PathBuf::from(format!("/proc/{pid}")).exists());
    }
    assert!(manager
        .start(
            "https://example.com/movie".into(),
            HashMap::new(),
            0.0,
            None,
            false
        )
        .await
        .unwrap_err()
        .contains("shutting down"));
    manager.shutdown().await; // Idempotent.
}
