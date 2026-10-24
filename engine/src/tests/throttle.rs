use super::*;

#[test]
fn throttle_tracks_only_fetched_av_segments_and_uses_hysteresis() {
    let mut throttle = Throttle::on_demand();
    // Playlists, captions and malformed names never move the resume point.
    for file in ["index.m3u8", "index_vtt.m3u8", "index7.vtt", "segment-1.ts"] {
        throttle.observe(file);
    }
    assert_eq!(segment_number("segment-000000012.ts"), Some(12));
    assert_eq!(segment_number("segment-1.ts"), None);
    assert_eq!(throttle.transition(HLS_THROTTLE_AHEAD_SEGMENTS), None);
    assert_eq!(
        throttle.transition(HLS_THROTTLE_AHEAD_SEGMENTS + 1),
        Some(true),
        "an encoder past the throttle point must be suspended"
    );
    throttle.observe("segment-000000040.ts");
    // An older fetch (another viewer, a retry) never moves the point back.
    throttle.observe("segment-000000003.ts");
    assert_eq!(throttle.transition(40 + HLS_THROTTLE_AHEAD_SEGMENTS), None);
    assert_eq!(
        throttle.transition(40 + HLS_THROTTLE_AHEAD_SEGMENTS + 1),
        Some(true)
    );
    // The pause point stays inside the advertised rolling window, and resuming
    // needs real catch-up, not a single fetch.
    const {
        assert!(HLS_THROTTLE_AHEAD_SEGMENTS < (HLS_WINDOW_SECONDS / HLS_SEGMENT_SECONDS) as u64);
        assert!(HLS_THROTTLE_RESUME_SEGMENTS < HLS_THROTTLE_AHEAD_SEGMENTS);
    }
    assert!(!Throttle::default().holding());
}

/// SIGSTOP/SIGCONT really suspend the child, the stall watchdog is told to
/// wait, and a suspended child still dies on the normal cleanup path.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn throttle_suspends_resumes_and_still_kills_the_encoder() {
    let state = |pid: u32| {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        stat.rsplit_once(") ").unwrap().1.chars().next().unwrap()
    };
    let mut child = Command::new("sleep")
        .arg("30")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    let mut throttle = Throttle::on_demand();
    throttle.apply(&child, HLS_THROTTLE_AHEAD_SEGMENTS + 1);
    assert!(throttle.holding());
    sleep(Duration::from_millis(50)).await;
    assert_eq!(state(pid), 'T', "the encoder must be stopped");
    throttle.observe("segment-000000100.ts");
    throttle.apply(&child, 100 + HLS_THROTTLE_RESUME_SEGMENTS);
    sleep(Duration::from_millis(50)).await;
    assert_eq!(state(pid), 'S', "the encoder must run again");
    assert!(
        throttle.holding(),
        "a resumed input gets a grace period before the stall watchdog"
    );
    throttle.apply(&child, 200 + HLS_THROTTLE_AHEAD_SEGMENTS);
    sleep(Duration::from_millis(50)).await;
    assert_eq!(state(pid), 'T');
    child.start_kill().unwrap();
    assert!(!timeout(Duration::from_secs(3), child.wait())
        .await
        .expect("a suspended encoder must still die on cleanup")
        .unwrap()
        .success());
}

/// A long on-demand source remuxes far faster than realtime. The encoder must
/// pause near the viewer instead of rolling the window past it, survive the
/// stall watchdog while paused, resume as soon as the viewer catches up and
/// pause again with the viewer's next segment still listed.
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires real FFmpeg/ffprobe binaries"]
async fn on_demand_encoding_pauses_ahead_of_the_viewer_and_resumes() {
    use std::os::unix::fs::PermissionsExt;
    let ffmpeg =
        PathBuf::from(std::env::var("PLAYBACK_TEST_FFMPEG").expect("PLAYBACK_TEST_FFMPEG"));
    let ffprobe =
        PathBuf::from(std::env::var("PLAYBACK_TEST_FFPROBE").expect("PLAYBACK_TEST_FFPROBE"));
    let root = tempfile::tempdir().unwrap();
    let fixture = root.path().join("movie.mkv");
    let status = Command::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x180:rate=24",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "150",
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "24",
            // A realistic 2 Mbit/s, so buffered socket data is a realistic
            // number of seconds when a suspended encoder resumes.
            "-b:v",
            "2M",
            "-minrate",
            "2M",
            "-maxrate",
            "2M",
            "-bufsize",
            "2M",
            "-x264-params",
            "nal-hrd=cbr",
            "-c:a",
            "aac",
            "-ac",
            "2",
            "-shortest",
        ])
        .arg(&fixture)
        .status()
        .await
        .unwrap();
    assert!(status.success());
    let bytes = tokio::fs::read(&fixture).await.unwrap();
    // A network-bound source (~1.6 MB/s, about six times realtime for this
    // fixture) rather than an instant local read.
    let router = axum::Router::new().route(
        "/movie.mkv",
        axum::routing::get(move || {
            let bytes = bytes.clone();
            async move {
                axum::body::Body::from_stream(async_stream::stream! {
                    for chunk in bytes.chunks(16 * 1024) {
                        sleep(Duration::from_millis(10)).await;
                        yield Ok::<_, std::io::Error>(axum::body::Bytes::copy_from_slice(chunk));
                    }
                })
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/movie.mkv", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let arguments = root.path().join("ffmpeg-arguments.txt");
    let wrapper = root.path().join("ffmpeg-wrapper.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\nexec \"{}\" \"$@\"\n",
            arguments.display(),
            ffmpeg.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let media = root.path().join("media");
    let manager = PlaybackManager::new(Config {
        ffmpeg: wrapper,
        ffprobe,
        root: media.clone(),
        max_sessions: 1,
        // The reaper ticks at min(ttl, 1s); keep the production cadence.
        ttl: Duration::from_secs(60),
    });
    let response = manager
        .start_with_permit(url, HashMap::new(), 0.0, None, false, false, None)
        .await
        .unwrap();
    assert_eq!(response.video_mode, "copy");
    let args = tokio::fs::read_to_string(&arguments).await.unwrap();
    let args: Vec<_> = args.lines().collect();
    assert!(
        !args.contains(&"-re"),
        "on-demand input is paced by its viewer, not by realtime: {args:?}"
    );
    let dir = media.join(&response.id);
    let capability = response.url.split('/').nth(3).unwrap().to_owned();
    let newest = || async { newest_segment(&dir).await.unwrap_or(0) };
    // Nobody fetches anything: the encoder must stop near the throttle point
    // instead of racing through the source.
    let paused = timeout(Duration::from_secs(30), async {
        loop {
            let before = newest().await;
            sleep(Duration::from_millis(2500)).await;
            let after = newest().await;
            if after > HLS_THROTTLE_AHEAD_SEGMENTS && after == before {
                return after;
            }
        }
    })
    .await
    .expect("the encoder must pause ahead of an idle viewer");
    assert!(
        paused < (HLS_WINDOW_SECONDS / HLS_SEGMENT_SECONDS) as u64,
        "the viewer's first segment must still be listed: {paused}"
    );
    // Longer than the 20s stall watchdog: a suspended encoder is not stalled.
    sleep(Duration::from_secs(21)).await;
    assert!(
        manager.input_running(&response.id).await,
        "the stall watchdog must not close a throttled session"
    );
    assert_eq!(newest().await, paused, "the encoder must stay suspended");
    let playlist = tokio::fs::read_to_string(dir.join("index.m3u8"))
        .await
        .unwrap();
    assert!(
        playlist.contains("#EXT-X-MEDIA-SEQUENCE:0") || !playlist.contains("#EXT-X-MEDIA-SEQUENCE"),
        "no segment may have left the window while paused"
    );
    // The viewer catches up: fetching near the edge resumes the encoder.
    let file = format!("segment-{paused:09}.ts");
    assert!(manager
        .serve(&response.id, &capability, &file)
        .await
        .is_ok());
    let repaused = timeout(Duration::from_secs(30), async {
        loop {
            let before = newest().await;
            sleep(Duration::from_millis(2500)).await;
            let after = newest().await;
            if after > paused && after == before {
                return after;
            }
        }
    })
    .await
    .expect("the encoder must resume once the viewer catches up, then pause again");
    assert!(
        repaused - paused < (HLS_WINDOW_SECONDS / HLS_SEGMENT_SECONDS) as u64,
        "the viewer's next segment must still be listed: {paused} -> {repaused}"
    );
    assert!(manager.input_running(&response.id).await);
    assert!(manager.stop(&response.id).await);
    manager.shutdown().await;
    server.abort();
    let _ = server.await;
}

#[test]
fn vod_prefix_cleanup_trails_the_furthest_fetch_without_repeats() {
    let mut throttle = Throttle::on_demand();
    let mut playlist = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:2\n");
    for i in 0..100u64 {
        playlist.push_str(&format!("#EXTINF:2.000000,\nsegment-{i:09}.ts\n"));
    }
    throttle.apply_playlist(&playlist);
    assert!(
        throttle.take_pending_deletions().is_empty(),
        "nothing is reclaimed before the viewer fetches"
    );
    // A deep fetch marks ~162s consumed; only media ending more than the
    // grace period behind it (end < 102s, segments 0..=49) is reclaimed.
    throttle.observe("segment-000000080.ts");
    throttle.apply_playlist(&playlist);
    let pending = throttle.take_pending_deletions();
    assert_eq!(pending.len(), 50);
    assert_eq!(pending.first().map(String::as_str), Some("segment-000000000.ts"));
    assert_eq!(pending.last().map(String::as_str), Some("segment-000000049.ts"));
    // Re-applying the same playlist must not queue the same files again.
    throttle.apply_playlist(&playlist);
    assert!(throttle.take_pending_deletions().is_empty());
    // Catching up reclaims the next contiguous prefix only.
    throttle.observe("segment-000000090.ts");
    throttle.apply_playlist(&playlist);
    let pending = throttle.take_pending_deletions();
    assert_eq!(pending.first().map(String::as_str), Some("segment-000000050.ts"));
}

#[test]
fn fragmented_mp4_segments_count_toward_throttle_and_cleanup() {
    assert_eq!(segment_number("segment-000000012.m4s"), Some(12));
    assert_eq!(segment_number("init.mp4"), None);
    let mut throttle = Throttle::on_demand();
    let mut playlist = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:2\n");
    for i in 0..80u64 {
        playlist.push_str(&format!("#EXTINF:2.000000,\nsegment-{i:09}.m4s\n"));
    }
    throttle.observe("segment-000000070.m4s");
    throttle.apply_playlist(&playlist);
    assert!(
        !throttle.take_pending_deletions().is_empty(),
        "fMP4 VOD sessions reclaim the watched prefix too"
    );
}
