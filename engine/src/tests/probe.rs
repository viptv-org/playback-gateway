use super::*;

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
printf '#EXTM3U\n#EXTINF:1,\nsegment-000000000.ts\n' > "$last"
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

#[cfg(unix)]
#[tokio::test]
async fn direct_url_clients_receive_the_original_source_instead_of_a_session() {
    let root = tempfile::tempdir().unwrap();
    // HDR HEVC with Dolby audio in Matroska: the declared envelope refuses
    // to hand this over, so anything but direct-url delivery would
    // transcode it. A native client fetches the source itself instead.
    let probe_marker = root.path().join("unexpected-probe");
    let manager = scripted_probe(
        root.path(),
        &format!("touch '{}'; exit 77", probe_marker.display()),
    );
    let caps: Capabilities = serde_json::from_value(serde_json::json!({
        "h264": false, "aac": false, "max_width": 0, "max_height": 0,
        "direct_play": false, "direct_urls": true
    }))
    .unwrap();
    let mut headers = HashMap::new();
    headers.insert("Cookie".to_owned(), "session=opaque".to_owned());
    headers.insert("user-agent".to_owned(), "viptv-native/1".to_owned());
    headers.insert(
        "Referer".to_owned(),
        "https://provider.example/watch".to_owned(),
    );
    let response = manager
        .start(
            "http://example.com/video".to_owned(),
            headers,
            0.0,
            Some(caps),
            false,
        )
        .await
        .unwrap();
    // The client's own engine fetches the original URL: no proxy session,
    // no transcode, and no codec-policy refusal.
    assert_eq!(response.url, "http://example.com/video");
    assert_eq!(response.mode, "direct");
    assert_eq!(response.video_mode, "copy");
    assert_eq!(response.audio_mode, "copy");
    assert!(
        !probe_marker.exists(),
        "Native playback must never start server inspection"
    );
    assert_eq!(response.format, "file");
    assert_eq!(response.duration, 0.0);
    assert!(response.audio_tracks.is_empty());
    let authorization = response.authorization.expect("upstream authorization");
    assert_eq!(authorization.cookie.as_deref(), Some("session=opaque"));
    assert_eq!(authorization.user_agent.as_deref(), Some("viptv-native/1"));
    let forwarded = authorization.headers.expect("upstream header set");
    assert_eq!(
        forwarded.get("Referer").map(String::as_str),
        Some("https://provider.example/watch")
    );
    assert!(!forwarded.contains_key("Cookie"));
    assert!(!forwarded.contains_key("user-agent"));
    // The transport-less session still answers heartbeats.
    assert!(manager.heartbeat(&response.id).await);
    // The reaper must not retire a transport-less direct-URL session: it
    // owns no directory and no encoder, so only its TTL ends it, and
    // every heartbeat renews that TTL.
    manager.reap().await;
    assert!(manager.heartbeat(&response.id).await);
    assert_eq!(manager.active_count().await, 1);
    manager.shutdown().await;
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
    assert_eq!(manager.slots.available_permits(), 0);
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
