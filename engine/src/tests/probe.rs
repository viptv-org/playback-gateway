use super::*;

#[cfg(unix)]
#[tokio::test]
async fn shared_live_hls_preserves_the_existing_native_hevc_copy_gate() {
    let root = tempfile::tempdir().unwrap();
    let manager = scripted_probe(
        root.path(),
        r#"printf '%s' '{"streams":[{"index":0,"codec_type":"video","codec_name":"hevc","width":320,"height":180,"pix_fmt":"yuv420p","profile":"Main","level":90,"avg_frame_rate":"25/1","r_frame_rate":"25/1","field_order":"progressive"},{"index":1,"codec_type":"audio","codec_name":"aac","profile":"LC","channels":2}],"format":{"format_name":"hls"}}'"#,
    );
    let prepared = manager
        .prepare_shared_with_selection(
            "https://fixture.invalid/live.m3u8".into(),
            HashMap::new(),
            0.0,
            Some(Capabilities {
                hevc: true,
                hevc_sdr: true,
                direct_play: true,
                direct_hls: Some(true),
                direct_files: Some(false),
                ..Capabilities::default()
            }),
            false,
            true,
            TrackSelection::default(),
            "fixture".into(),
        )
        .await
        .unwrap();
    let plan = prepared.plan.as_ref().unwrap();
    assert_eq!(plan.video_codec, "hevc");
    assert_eq!(plan.video_mode, "copy");
    assert_eq!(plan.mode, "remux");
    assert!(plan.shareable);
    drop(prepared);
    manager.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn physical_ingest_eligibility_declines_bitmap_subtitles_unknown_codec_and_progressive() {
    for case in ["subtitle", "codec", "progressive"] {
        let root = tempfile::tempdir().unwrap();
        let mut facts = serde_json::json!({"streams":[
            {"index":0,"codec_type":"video","codec_name":"h264","width":640,"height":360,"pix_fmt":"yuv420p","profile":"High","level":31,"avg_frame_rate":"25/1","r_frame_rate":"25/1","field_order":"progressive"},
            {"index":1,"codec_type":"audio","codec_name":"aac","profile":"LC","channels":2}],"format":{"format_name":"mpegts"}});
        if case == "subtitle" {
            facts["streams"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"index":2,"codec_type":"subtitle","codec_name":"hdmv_pgs_subtitle"}));
        }
        if case == "codec" {
            facts["streams"][0]["codec_name"] = "mpeg4".into();
        }
        let manager = scripted_probe(root.path(), &format!("printf '%s' '{}'", facts));
        let prepared = manager
            .prepare_shared_with_selection(
                "https://fixture.invalid/source".into(),
                HashMap::new(),
                0.0,
                Some(Capabilities {
                    direct_play: false,
                    direct_hls: Some(case != "progressive"),
                    ..Capabilities::default()
                }),
                false,
                true,
                TrackSelection::default(),
                "fixture".into(),
            )
            .await
            .unwrap();
        let plan = prepared.plan.as_ref().unwrap();
        assert!(
            !plan.shared_ingest,
            "ineligible source must retain physical input accounting"
        );
        assert!(!serde_json::to_string(plan)
            .unwrap()
            .contains("shared_ingest"));
        drop(prepared);
        manager.shutdown().await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn preparation_normalizes_output_and_does_not_start_a_worker() {
    let root = tempfile::tempdir().unwrap();
    let manager = scripted_probe(
        root.path(),
        r#"printf '%s' '{"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":1280,"height":720,"pix_fmt":"yuv420p","profile":"High","level":41,"avg_frame_rate":"30/1","r_frame_rate":"30/1","field_order":"progressive"},{"index":1,"codec_type":"audio","codec_name":"aac","profile":"LC","channels":2}],"format":{"duration":"30"}}'"#,
    );
    let provider = Arc::new(Semaphore::new(1));
    let mut plans = Vec::new();
    for (width, height) in [(1920, 1080), (3840, 2160)] {
        let prepared = manager
            .prepare_with_selection(
                "https://fixture.invalid/private-source".into(),
                HashMap::new(),
                0.0,
                Some(Capabilities {
                    max_width: width,
                    max_height: height,
                    ..Capabilities::default()
                }),
                false,
                false,
                Some(provider.clone().try_acquire_owned().unwrap()),
                TrackSelection::default(),
            )
            .await
            .unwrap();
        let plan = prepared.plan.as_ref().unwrap();
        assert_eq!(plan.mode, "remux");
        assert_eq!((plan.width, plan.height), (1280, 720));
        assert!(plan.shareable);
        assert!(!serde_json::to_string(plan)
            .unwrap()
            .contains("private-source"));
        plans.push(plan.clone());
        assert_eq!(provider.available_permits(), 0);
        assert_eq!(manager.active_count().await, 0);
        assert!(
            !root.path().join("media").exists(),
            "preparation must not write worker output"
        );
        // The encoder path intentionally does not exist. Successful preparation
        // proves no output process was required. Dropping it frees admission.
        drop(prepared);
        assert_eq!(provider.available_permits(), 1);
    }
    assert_eq!(
        plans[0], plans[1],
        "capability headroom must not split identical outputs"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("probe.sh.count")).unwrap(),
        "x",
        "inspection reused while cached"
    );
    manager.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn qsv_failure_retries_with_same_reservation_and_copies_compatible_audio() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let ffprobe = root.path().join("probe");
    std::fs::write(&ffprobe, r#"#!/bin/sh
printf '%s' '{"streams":[{"index":0,"codec_type":"video","codec_name":"hevc","width":640,"height":360,"pix_fmt":"yuv420p","sample_aspect_ratio":"1:1","field_order":"progressive"},{"index":1,"codec_type":"audio","codec_name":"aac","profile":"LC","channels":2,"tags":{"language":"eng"}}],"format":{"duration":"10"}}'
"#).unwrap();
    let ffmpeg = root.path().join("encoder");
    std::fs::write(
        &ffmpeg,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$0.attempts"
case "$*" in
 *h264_qsv*) exit 1 ;;
esac
for last do :; done
dir=${last%/*}
printf x > "$dir/segment-000000000.ts"
printf '#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nsegment-000000000.ts\n#EXT-X-ENDLIST\n' > "$last"
"#,
    )
    .unwrap();
    for path in [&ffprobe, &ffmpeg] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let manager = PlaybackManager::new_with_qsv(
        Config {
            ffmpeg: ffmpeg.clone(),
            ffprobe,
            root: root.path().join("media"),
            max_sessions: 1,
            ttl: Duration::from_secs(60),
        },
        Some("/dev/dri/renderD128".into()),
    );
    manager.qsv_ready.set(true).unwrap();
    manager.vaapi_ready.set(hardware::Vaapi::default()).unwrap();
    let provider = Arc::new(Semaphore::new(1));
    let result = manager
        .start_with_permit(
            "http://fixture.invalid/video".into(),
            HashMap::new(),
            0.0,
            None,
            false,
            false,
            Some(provider.clone().acquire_owned().await.unwrap()),
        )
        .await
        .unwrap();
    assert_eq!(result.video_mode, "encode");
    assert_eq!(result.audio_mode, "copy");
    assert_eq!(provider.available_permits(), 0);
    let attempts = std::fs::read_to_string(ffmpeg.with_extension("attempts")).unwrap();
    let attempts: Vec<_> = attempts.lines().collect();
    assert_eq!(attempts.len(), 3);
    assert!(attempts[0].contains("-c:v hevc_qsv"));
    assert!(attempts[1].contains("hwupload="));
    assert!(attempts[2].contains("libx264"));
    assert!(!attempts[2].contains("-init_hw_device"));
    assert!(attempts.iter().all(|a| a.contains("-c:a copy")));
    assert!(manager.stop(&result.id).await);
    assert_eq!(provider.available_permits(), 1);
    manager.shutdown().await;
}

#[test]
fn gateway_rejects_direct_url_credential_handoff() {
    assert!(serde_json::from_value::<Capabilities>(serde_json::json!({
        "direct_urls": true
    }))
    .is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn bounded_probe_cache_reuses_identical_sources_within_their_ttl() {
    let root = tempfile::tempdir().unwrap();
    let manager = scripted_probe(
        root.path(),
        r#"printf '%s' '{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"}]}'"#,
    );
    let first = manager
        .cached_probe(
            "http://example.com/video",
            "Authorization: opaque",
            false,
            None,
        )
        .await
        .unwrap();
    let second = manager
        .cached_probe(
            "http://example.com/video",
            "Authorization: opaque",
            false,
            None,
        )
        .await
        .unwrap();
    assert_eq!(first.streams.len(), second.streams.len());
    assert_eq!(
        std::fs::read_to_string(root.path().join("probe.sh.count")).unwrap(),
        "x",
        "a seek/restart of the same authorized VOD must not repeat ffprobe"
    );
    // Live sources share the cache now: a channel change inside the short
    // live TTL must not pay another full ffprobe before playback starts.
    manager
        .cached_probe("http://example.com/live", "", true, None)
        .await
        .unwrap();
    manager
        .cached_probe("http://example.com/live", "", true, None)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("probe.sh.count"))
            .unwrap()
            .len(),
        2,
        "a live channel change within the TTL must not repeat ffprobe"
    );
    // The live discriminator in the digest keeps live and VOD identities
    // apart even for one URL.
    manager
        .cached_probe(
            "http://example.com/video",
            "Authorization: opaque",
            true,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("probe.sh.count"))
            .unwrap()
            .len(),
        3,
        "live playback of a VOD-probed URL must not reuse the VOD entry"
    );
    // Header changes still bypass the cached identity.
    manager
        .cached_probe(
            "http://example.com/video",
            "Authorization: changed",
            false,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("probe.sh.count"))
            .unwrap()
            .len(),
        4,
        "header changes must bypass the cached identity"
    );
    // Live entries expire on their own shorter TTL, not the VOD window.
    for entry in manager.probe_cache.lock().await.values_mut() {
        if entry.live {
            entry.inserted = Instant::now() - LIVE_PROBE_CACHE_TTL - Duration::from_secs(1);
        }
    }
    manager
        .cached_probe("http://example.com/live", "", true, None)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("probe.sh.count"))
            .unwrap()
            .len(),
        5,
        "an expired live entry must be inspected again"
    );
    assert!(manager
        .probe_cache
        .lock()
        .await
        .keys()
        .all(|key| key.len() == 32));
}

#[cfg(unix)]
#[tokio::test]
async fn playing_vod_keeps_probe_until_its_last_session_releases_it() {
    let root = tempfile::tempdir().unwrap();
    let manager = scripted_probe(
        root.path(),
        r#"printf '%s' '{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"}]}'"#,
    );
    let playing = manager
        .cached_probe("http://example.com/movie", "", false, None)
        .await
        .unwrap();
    for entry in manager.probe_cache.lock().await.values_mut() {
        entry.inserted = Instant::now() - PROBE_CACHE_TTL - Duration::from_secs(1);
    }
    let seek = manager
        .cached_probe("http://example.com/movie", "", false, None)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("probe.sh.count")).unwrap(),
        "x",
        "a seek during playback repeated inspection after the short idle cache TTL"
    );
    drop(playing);
    drop(seek);
    manager
        .cached_probe("http://example.com/movie", "", false, None)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("probe.sh.count")).unwrap(),
        "xx",
        "an idle expired source must be inspected again"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn probe_retries_one_transient_failure_and_holds_admission() {
    let root = tempfile::tempdir().unwrap();
    let manager = scripted_probe(
        root.path(),
        r#"
if [ "$(wc -c < "$0.count")" -eq 1 ]; then
    printf 'HTTP error 404 Not Found\n' >&2
    exit 1
fi
printf '%s' '{"streams":[{"codec_type":"video","codec_name":"h264","width":960,"height":540,"pix_fmt":"yuv420p"},{"codec_type":"audio","codec_name":"aac","profile":"HE-AAC","channels":2}]}'
"#,
    );
    let capacity = Arc::new(Semaphore::new(1));
    let permit = capacity.clone().acquire_owned().await.unwrap();
    let worker = {
        let manager = manager.clone();
        tokio::spawn(async move {
            manager
                .start_with_permit(
                    "http://example.com/video".into(),
                    HashMap::new(),
                    0.0,
                    None,
                    false,
                    true,
                    Some(permit),
                )
                .await
        })
    };
    wait_probe_file(&root.path().join("probe.sh.count")).await;
    assert_eq!(capacity.available_permits(), 0);
    assert_eq!(manager.preparation_slots.available_permits(), 0);
    assert_eq!(manager.slots.available_permits(), 1);
    let result = timeout(Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap();
    // The valid second probe advances to ffmpeg startup (fixture ffmpeg deliberately absent).
    assert!(!result.unwrap_err().contains("inspect source"));
    assert_eq!(
        std::fs::read(root.path().join("probe.sh.count"))
            .unwrap()
            .len(),
        2
    );
    let cleanup = std::mem::take(&mut *manager.cleanup_tasks.lock().unwrap());
    for task in cleanup {
        timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(capacity.available_permits(), 1);
    assert_eq!(manager.slots.available_permits(), 1);
    assert_eq!(manager.preparation_slots.available_permits(), 1);
    assert!(manager
        .probe("http://example.com/video", "", None)
        .await
        .is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn probe_attempts_are_bounded_and_permanent_failures_never_retry() {
    for (body, count) in [
        (
            "printf 'HTTP error 503 Service Unavailable\\n' >&2; exit 1",
            2,
        ),
        ("printf 'Connection reset by peer\\n' >&2; exit 1", 2),
        ("printf 'HTTP error 401 Unauthorized\\n' >&2; exit 1", 1),
        ("printf 'Server returned 403 Forbidden\\n' >&2; exit 1", 1),
        (
            "printf 'Protocol not on whitelist\\nHTTP error 404 Not Found\\n' >&2; exit 1",
            1,
        ),
        // Unreadable or oversized metadata earns exactly one reduced-entry
        // retry: a noisy response is not evidence of a dead source.
        ("printf 'not json'", 2),
        (
            "printf 'not json'; printf 'HTTP error 404 Not Found\\n' >&2; exit 1",
            2,
        ),
        ("head -c 1048577 /dev/zero", 2),
        ("head -c 65537 /dev/zero >&2; exit 1", 1),
    ] {
        let root = tempfile::tempdir().unwrap();
        let manager = scripted_probe(root.path(), body);
        assert!(timeout(
            Duration::from_secs(5),
            manager.probe("http://example.com/video", "", None)
        )
        .await
        .unwrap()
        .is_none());
        assert_eq!(
            std::fs::read(root.path().join("probe.sh.count"))
                .unwrap()
                .len(),
            count
        );
    }
    let root = tempfile::tempdir().unwrap();
    let manager = scripted_probe(root.path(), "exit 0");
    std::fs::remove_file(root.path().join("probe.sh")).unwrap();
    assert!(matches!(
        manager
            .probe_attempt("http://example.com/video", "", None, false)
            .await,
        Err(ProbeFailure::Spawn)
    ));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn unreadable_probe_output_falls_back_to_a_reduced_entry_set() {
    let root = tempfile::tempdir().unwrap();
    // The full scheme answers with something the parser cannot read; the
    // reduced scheme (three `-show_entries`, detected by its own count file)
    // must still describe the source instead of failing playback.
    let manager = scripted_probe(
        root.path(),
        r#"for arg in "$@"; do case "$arg" in *stream_disposition*) printf '%s' 'not json'; exit 0; esac; done; printf '%s' '{"format":{"format_name":"matroska,webm","duration":"120"},"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":1920,"height":1080,"pix_fmt":"yuv420p"},{"index":1,"codec_type":"audio","codec_name":"aac","channels":2}]}'"#,
    );
    let probe = manager
        .probe("http://example.com/video", "", None)
        .await
        .expect("a reduced probe must describe a source whose full probe was unreadable");
    assert_eq!(
        probe.format.get("format_name").and_then(|v| v.as_str()),
        Some("matroska,webm")
    );
    assert_eq!(probe.duration(), Some(120.0));
    assert_eq!(
        std::fs::read(root.path().join("probe.sh.count"))
            .unwrap()
            .len(),
        2,
        "one full attempt then one reduced attempt"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancelled_probe_kills_and_reaps_child() {
    let root = tempfile::tempdir().unwrap();
    let manager = scripted_probe(root.path(), "printf '%s' $$ > \"$0.pid\"; exec sleep 60");
    let worker = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.probe("http://example.com/video", "", None).await })
    };
    let pid = wait_probe_file(&root.path().join("probe.sh.pid")).await;
    let pid: u32 = pid.parse().unwrap();
    worker.abort();
    assert!(matches!(worker.await, Err(error) if error.is_cancelled()));
    let cleanup = std::mem::take(&mut *manager.cleanup_tasks.lock().unwrap());
    assert!(!cleanup.is_empty());
    for task in cleanup {
        timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
    }
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "probe child must be reaped, not merely killed"
    );
    assert_eq!(
        std::fs::read(root.path().join("probe.sh.count"))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn probe_diagnostic_categories_are_closed_and_conservative() {
    for code in [404, 408, 429, 500, 503, 599] {
        let category = probe_failure(format!("HTTP error {code} failure").as_bytes());
        assert_eq!(category, ProbeFailure::Http(code));
        assert!(category.retryable());
    }
    assert_eq!(
        probe_failure(b"HTTP error 404\nServer returned 403 Forbidden"),
        ProbeFailure::Http(403)
    );
    assert_eq!(probe_failure(b"unknown diagnostic"), ProbeFailure::Exit);
    assert!(!ProbeFailure::InvalidJson.retryable());
    assert!(!ProbeFailure::Oversized.retryable());
    assert!(ProbeFailure::Timeout.retryable());
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn cancelled_probe_keeps_reservation_until_child_reaped() {
    let root = tempfile::tempdir().unwrap();
    let manager = scripted_probe(root.path(), "printf '%s' $$ > \"$0.pid\"; exec sleep 60");
    let slots = Arc::new(Semaphore::new(1));
    let mut preparation = Box::pin(manager.start_with_permit(
        "http://example.com/live".into(),
        HashMap::new(),
        0.0,
        None,
        false,
        true,
        Some(slots.clone().try_acquire_owned().unwrap()),
    ));
    let probe_pid = root.path().join("probe.sh.pid");
    tokio::select! {
        result = &mut preparation => panic!("fixture must stay in probing: {result:?}"),
        _ = wait_probe_file(&probe_pid) => {}
    }
    drop(preparation);
    // No await: the cleanup owner has not run on this single-thread runtime.
    // Admission must remain closed until that owner confirms child teardown.
    assert_eq!(
        slots.available_permits(),
        0,
        "cancelled input still owns its connection"
    );
    manager.shutdown().await;
    assert_eq!(slots.available_permits(), 1);
}

#[tokio::test]
async fn provider_permit_released_on_probe_failure() {
    let root = tempfile::tempdir().unwrap();
    let slots = Arc::new(Semaphore::new(1));
    let manager = PlaybackManager::new(Config {
        ffmpeg: "/missing/ffmpeg".into(),
        ffprobe: "/missing/ffprobe".into(),
        root: root.path().into(),
        max_sessions: 1,
        ttl: Duration::from_secs(60),
    });
    let result = manager
        .start_with_permit(
            "https://example.com/movie".into(),
            HashMap::new(),
            0.0,
            None,
            false,
            false,
            Some(slots.clone().try_acquire_owned().unwrap()),
        )
        .await;
    assert!(result.unwrap_err().contains("inspect"));
    assert_eq!(slots.available_permits(), 1);
    manager.shutdown().await;
}

#[tokio::test]
async fn validates_before_spawning_and_redacts_errors() {
    let manager = PlaybackManager::new(Config {
        ffmpeg: "/missing/ffmpeg".into(),
        ffprobe: "/missing/ffprobe".into(),
        root: std::env::temp_dir().join(Uuid::new_v4().to_string()),
        max_sessions: 0,
        ttl: Duration::from_secs(1),
    });
    assert!(manager
        .start(
            "file:///etc/passwd".into(),
            HashMap::new(),
            0.0,
            None,
            false
        )
        .await
        .is_err());
    let error = manager
        .start(
            "https://user:secret@example.com/movie".into(),
            HashMap::new(),
            0.0,
            None,
            false,
        )
        .await
        .unwrap_err();
    assert!(!error.contains("secret"));
    assert!(manager
        .start(
            "https://example.com/movie".into(),
            HashMap::new(),
            f64::NAN,
            None,
            false
        )
        .await
        .is_err());
    assert_eq!(manager.active_count().await, 0);
    assert!(!manager.heartbeat("unknown").await);
    assert!(!manager.stop("unknown").await);
    assert!(!manager.ffmpeg_available().await);
}
