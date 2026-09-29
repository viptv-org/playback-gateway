use super::*;
use axum::{extract::State, routing::get, Router};
use std::sync::atomic::AtomicUsize;

type LivePeakState = (Arc<Vec<u8>>, Arc<AtomicUsize>, Arc<AtomicUsize>);
type GateUnfetchedState = (Arc<Vec<u8>>, Arc<tokio::sync::Notify>, Arc<AtomicUsize>);

async fn fixture() -> (Url, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    async fn origin(State(count): State<Arc<AtomicUsize>>, headers: HeaderMap) -> Response {
        count.fetch_add(1, Ordering::SeqCst);
        let bytes = b"0123456789abcdef";
        let (start, end) = range_bounds(
            headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
            bytes.len() as u64,
        )
        .unwrap();
        Response::builder()
            .status(206)
            .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/16"))
            .header(header::ETAG, "\"fixture-v1\"")
            .body(Body::from(bytes[start as usize..=end as usize].to_vec()))
            .unwrap()
    }
    async fn live(State(count): State<Arc<AtomicUsize>>) -> &'static str {
        count.fetch_add(1, Ordering::SeqCst);
        "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:6,\nsegment.ts?secret=never-public\n"
    }
    let count = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/movie.mp4", get(origin))
        .route("/live.m3u8", get(live))
        .route("/segment.ts", get(|| async { b"original-segment".to_vec() }))
        .route(
            "/vod.m3u8",
            get(|| async {
                "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\nsegment.ts?secret=never-public\n#EXT-X-ENDLIST\n"
            }),
        )
        .route(
            "/master.m3u8",
            get(|| async {
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=100,RESOLUTION=1280x720\nvariant.m3u8?token=secret\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"English\",URI=\"audio.m3u8?token=secret\"\n"
            }),
        )
        .route(
            "/variant.m3u8",
            get(|| async {
                "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:6,\nvseg.ts?token=secret\n"
            }),
        )
        .route(
            "/audio.m3u8",
            get(|| async { "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\naseg.aac?token=secret\n" }),
        )
        .route("/vseg.ts", get(|| async { b"variant-segment".to_vec() }))
        .route("/aseg.aac", get(|| async { b"audio-segment".to_vec() }))
        .route("/large.m3u8", get(|| async { "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\nlarge.ts\n" }))
        .route("/large.ts", get(|| async { vec![7u8; 9 * 1024 * 1024] }))
        .with_state(count.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, count, task)
}
fn permits() -> Arc<InputPermits> {
    Arc::new(InputPermits {
        _playback: Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap(),
        _provider: None,
    })
}
#[tokio::test]
async fn redirects_stay_server_side_and_reject_private_cross_origin() {
    let app = Router::new()
        .route(
            "/redirect.m3u8",
            get(|| async {
                Response::builder()
                    .status(302)
                    .header(header::LOCATION, "/actual.m3u8?secret=private")
                    .body(Body::empty())
                    .unwrap()
            }),
        )
        .route(
            "/actual.m3u8",
            get(|| async { "#EXTM3U\n#EXTINF:6,\nsegment.ts?secret=private\n" }),
        )
        .route(
            "/unsafe.m3u8",
            get(|| async {
                Response::builder()
                    .status(302)
                    .header(header::LOCATION, "http://127.0.0.1:1/private.m3u8")
                    .body(Body::empty())
                    .unwrap()
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let direct = Direct::prepare(
        base.join("/redirect.m3u8").unwrap(),
        &HashMap::new(),
        "hls",
        permits(),
    )
    .await
    .unwrap();
    let response = direct
        .serve("index.m3u8", Method::GET, HeaderMap::new())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::LOCATION));
    let playlist = String::from_utf8(bytes(response).await.to_vec()).unwrap();
    assert!(!playlist.contains("secret"));
    assert!(!playlist.contains("http"));
    let unsafe_result = Direct::prepare(
        base.join("/unsafe.m3u8").unwrap(),
        &HashMap::new(),
        "hls",
        permits(),
    )
    .await;
    assert!(matches!(unsafe_result, Err(error) if error == "Unsafe media destination"));
    task.abort();
}
async fn bytes(response: Response) -> Bytes {
    axum::body::to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap()
}
#[tokio::test]
async fn original_file_ranges_resume_share_bytes_and_reject_expired_access() {
    let (base, count, task) = fixture().await;
    let direct = Direct::prepare(
        base.join("/movie.mp4").unwrap(),
        &HashMap::new(),
        "mp4",
        permits(),
    )
    .await
    .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(header::RANGE, "bytes=4-9".parse().unwrap());
    let response = direct
        .clone()
        .serve("source.mp4", Method::GET, headers.clone())
        .await
        .unwrap();
    assert_eq!(response.status(), 206);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 4-9/16");
    // GET must stream chunked: promising a byte count would turn any
    // mid-stream termination into an HTTP/2 framing violation.
    assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
    assert_eq!(bytes(response).await, "456789");
    assert_eq!(
        bytes(
            direct
                .clone()
                .serve("source.mp4", Method::GET, headers)
                .await
                .unwrap()
        )
        .await,
        "456789"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "initial metadata range and one shared media range"
    );
    let mut headers = HeaderMap::new();
    headers.insert(header::RANGE, "bytes=90-100".parse().unwrap());
    assert_eq!(
        direct
            .clone()
            .serve("source.mp4", Method::GET, headers)
            .await
            .unwrap()
            .status(),
        416
    );
    let head = direct
        .clone()
        .serve("source.mp4", Method::HEAD, HeaderMap::new())
        .await
        .unwrap();
    assert_eq!(head.headers()[header::CONTENT_LENGTH], "16");
    assert!(bytes(head).await.is_empty());
    direct.closed.store(true, Ordering::Release);
    assert!(direct
        .serve("source.mp4", Method::GET, HeaderMap::new())
        .await
        .is_err());
    task.abort();
}

#[tokio::test]
async fn failed_chunk_fetches_retry_and_the_body_completes() {
    let data = Arc::new(vec![b'x'; 2 * CHUNK as usize + 64]);
    let failed = Arc::new(AtomicBool::new(false));
    let state = (data.clone(), failed.clone());
    async fn origin(
        State((data, failed)): State<(Arc<Vec<u8>>, Arc<AtomicBool>)>,
        headers: HeaderMap,
    ) -> Response {
        let (start, end) = range_bounds(
            headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
            data.len() as u64,
        )
        .unwrap();
        // The second chunk fails exactly once before recovering.
        if start >= CHUNK && !failed.swap(true, Ordering::SeqCst) {
            return Response::builder().status(500).body(Body::empty()).unwrap();
        }
        Response::builder()
            .status(206)
            .header(
                header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{}", data.len()),
            )
            .header(header::ETAG, "\"flaky-etag\"")
            .body(Body::from(data[start as usize..=end as usize].to_vec()))
            .unwrap()
    }
    let app = Router::new()
        .route("/flaky.mp4", get(origin))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let direct = Direct::prepare(
        url.join("/flaky.mp4").unwrap(),
        &HashMap::new(),
        "mp4",
        permits(),
    )
    .await
    .unwrap();
    let response = direct
        .clone()
        .serve("source.mp4", Method::GET, HeaderMap::new())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
    let received = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(received.len(), data.len());
    assert!(received.iter().all(|byte| *byte == b'x'));
    assert!(failed.load(Ordering::SeqCst), "the chunk failed once");
    task.abort();
}

#[tokio::test]
async fn file_chunks_prefetch_concurrently_with_client_consumption() {
    let data = Arc::new(vec![7u8; 3 * CHUNK as usize]);
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let state = (data.clone(), live.clone(), peak.clone());
    async fn origin(
        State((data, live, peak)): State<LivePeakState>,
        headers: HeaderMap,
    ) -> Response {
        // A slow origin makes overlapping fetches observable.
        let current = live.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(current, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(120)).await;
        live.fetch_sub(1, Ordering::SeqCst);
        let (start, end) = range_bounds(
            headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
            data.len() as u64,
        )
        .unwrap();
        Response::builder()
            .status(206)
            .header(
                header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{}", data.len()),
            )
            .header(header::ETAG, "\"slow-etag\"")
            .body(Body::from(data[start as usize..=end as usize].to_vec()))
            .unwrap()
    }
    let app = Router::new()
        .route("/slow.mp4", get(origin))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let direct = Direct::prepare(
        url.join("/slow.mp4").unwrap(),
        &HashMap::new(),
        "mp4",
        permits(),
    )
    .await
    .unwrap();
    let response = direct
        .clone()
        .serve("source.mp4", Method::GET, HeaderMap::new())
        .await
        .unwrap();
    let received = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(received.len(), data.len());
    assert!(received.iter().all(|byte| *byte == 7));
    assert!(
        peak.load(Ordering::SeqCst) >= 2,
        "chunk fetches must overlap client consumption, not serialize"
    );
    task.abort();
}

#[tokio::test]
async fn closed_mid_stream_ends_the_body_cleanly_after_fetched_chunks() {
    let data = Arc::new(vec![7u8; 4 * CHUNK as usize]);
    let gate = Arc::new(tokio::sync::Notify::new());
    let unfetched = Arc::new(AtomicUsize::new(0));
    let state = (data.clone(), gate.clone(), unfetched.clone());
    async fn origin(
        State((data, gate, unfetched)): State<GateUnfetchedState>,
        headers: HeaderMap,
    ) -> Response {
        let (start, end) = range_bounds(
            headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
            data.len() as u64,
        )
        .unwrap();
        if start >= 3 * CHUNK {
            unfetched.fetch_add(1, Ordering::SeqCst);
        }
        // Every chunk but the first waits for the test to open the gate.
        if start >= CHUNK {
            gate.notified().await;
        }
        Response::builder()
            .status(206)
            .header(
                header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{}", data.len()),
            )
            .header(header::ETAG, "\"gated-etag\"")
            .body(Body::from(data[start as usize..=end as usize].to_vec()))
            .unwrap()
    }
    let app = Router::new()
        .route("/gated.mp4", get(origin))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let direct = Direct::prepare(
        url.join("/gated.mp4").unwrap(),
        &HashMap::new(),
        "mp4",
        permits(),
    )
    .await
    .unwrap();
    let response = direct
        .clone()
        .serve("source.mp4", Method::GET, HeaderMap::new())
        .await
        .unwrap();
    let mut stream = response.into_body().into_data_stream();
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(first.len(), CHUNK as usize);
    // Teardown begins mid-stream; pending chunks are released after it.
    direct.closed.store(true, Ordering::Release);
    gate.notify_waiters();
    let mut received = first.len();
    while let Some(frame) = stream.next().await {
        // A short but clean stream: no error may surface.
        received += frame.unwrap().len();
    }
    assert!(
        received >= 2 * CHUNK as usize,
        "already-fetched chunks still drain"
    );
    assert!(received < data.len(), "unfetched chunks are abandoned");
    assert_eq!(
        unfetched.load(Ordering::SeqCst),
        0,
        "no new fetches start after teardown"
    );
    task.abort();
}

#[tokio::test]
async fn original_hls_never_returns_origin_urls_and_preserves_segment_bytes() {
    let (base, _, task) = fixture().await;
    let direct = Direct::prepare(
        base.join("/live.m3u8").unwrap(),
        &HashMap::new(),
        "hls",
        permits(),
    )
    .await
    .unwrap();
    let response = direct
        .clone()
        .serve("index.m3u8", Method::GET, HeaderMap::new())
        .await
        .unwrap();
    let text = String::from_utf8(bytes(response).await.to_vec()).unwrap();
    assert!(!text.contains("secret"));
    assert!(!text.contains("http"));
    assert!(text.contains("#EXT-X-MEDIA-SEQUENCE:1"));
    let segment = text.lines().find(|l| l.starts_with("d-")).unwrap();
    assert_eq!(
        bytes(
            direct
                .clone()
                .serve(segment, Method::GET, HeaderMap::new())
                .await
                .unwrap()
        )
        .await,
        "original-segment"
    );
    assert!(direct
        .serve(
            "http://untrusted.invalid/file",
            Method::GET,
            HeaderMap::new()
        )
        .await
        .is_err());
    task.abort();
}

#[tokio::test]
async fn master_playlists_recurse_into_rewritten_variant_playlists() {
    let (base, _, task) = fixture().await;
    let direct = Direct::prepare(
        base.join("/master.m3u8").unwrap(),
        &HashMap::new(),
        "hls",
        permits(),
    )
    .await
    .unwrap();
    let master = String::from_utf8(
        bytes(
            direct
                .clone()
                .serve("index.m3u8", Method::GET, HeaderMap::new())
                .await
                .unwrap(),
        )
        .await
        .to_vec(),
    )
    .unwrap();
    assert!(master.contains("#EXT-X-STREAM-INF:BANDWIDTH=100,RESOLUTION=1280x720"));
    assert!(!master.contains("secret"));
    assert!(!master.contains("http"));
    let variant = master
        .lines()
        .find(|l| l.starts_with("d-"))
        .unwrap()
        .to_owned();
    assert!(variant.ends_with(".m3u8"), "{variant}");
    let media = master
        .lines()
        .find(|l| l.starts_with("#EXT-X-MEDIA:"))
        .unwrap();
    let audio = media
        .split("URI=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_owned();
    assert!(
        audio.starts_with("d-") && audio.ends_with(".m3u8"),
        "{media}"
    );
    // Fetching each rewritten key refetches and rewrites the nested media
    // playlist through the resources map, never the origin URL.
    for (playlist, segment_body) in [(&variant, "variant-segment"), (&audio, "audio-segment")] {
        let media = String::from_utf8(
            bytes(
                direct
                    .clone()
                    .serve(playlist, Method::GET, HeaderMap::new())
                    .await
                    .unwrap(),
            )
            .await
            .to_vec(),
        )
        .unwrap();
        assert!(!media.contains("secret"), "{media}");
        assert!(!media.contains("http"), "{media}");
        assert!(media.contains("#EXTINF:6,"));
        let segment = media
            .lines()
            .find(|l| l.starts_with("d-"))
            .unwrap()
            .to_owned();
        assert_eq!(
            bytes(
                direct
                    .clone()
                    .serve(&segment, Method::GET, HeaderMap::new())
                    .await
                    .unwrap()
            )
            .await,
            segment_body
        );
    }
    task.abort();
}
#[tokio::test]
async fn playlist_polls_hit_the_ttl_cache_instead_of_the_origin() {
    let (base, count, task) = fixture().await;
    let direct = Direct::prepare(
        base.join("/live.m3u8").unwrap(),
        &HashMap::new(),
        "hls",
        permits(),
    )
    .await
    .unwrap();
    let after_prepare = count.load(Ordering::SeqCst);
    for _ in 0..5 {
        direct
            .clone()
            .serve("index.m3u8", Method::GET, HeaderMap::new())
            .await
            .unwrap();
    }
    assert_eq!(
        count.load(Ordering::SeqCst),
        after_prepare,
        "polls inside the TTL must not touch the origin"
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    direct
        .clone()
        .serve("index.m3u8", Method::GET, HeaderMap::new())
        .await
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), after_prepare + 1);
    direct
        .clone()
        .serve("index.m3u8", Method::GET, HeaderMap::new())
        .await
        .unwrap();
    assert_eq!(
        count.load(Ordering::SeqCst),
        after_prepare + 1,
        "the refreshed rewrite is cached again"
    );
    task.abort();
}
#[tokio::test]
async fn immutable_resources_cache_while_live_playlists_revalidate() {
    let (base, _, task) = fixture().await;
    let live = Direct::prepare(
        base.join("/live.m3u8").unwrap(),
        &HashMap::new(),
        "hls",
        permits(),
    )
    .await
    .unwrap();
    let response = live
        .clone()
        .serve("index.m3u8", Method::GET, HeaderMap::new())
        .await
        .unwrap();
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let segment = String::from_utf8(bytes(response).await.to_vec())
        .unwrap()
        .lines()
        .find(|l| l.starts_with("d-"))
        .unwrap()
        .to_owned();
    let response = live
        .clone()
        .serve(&segment, Method::GET, HeaderMap::new())
        .await
        .unwrap();
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "public, max-age=3600"
    );
    assert_eq!(bytes(response).await, "original-segment");
    let response = live
        .clone()
        .serve(&segment, Method::GET, HeaderMap::new())
        .await
        .unwrap();
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "public, max-age=3600"
    );
    let vod = Direct::prepare(
        base.join("/vod.m3u8").unwrap(),
        &HashMap::new(),
        "hls",
        permits(),
    )
    .await
    .unwrap();
    let response = vod
        .clone()
        .serve("index.m3u8", Method::GET, HeaderMap::new())
        .await
        .unwrap();
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "public, max-age=3600"
    );
    let original = Direct::prepare(
        base.join("/movie.mp4").unwrap(),
        &HashMap::new(),
        "mp4",
        permits(),
    )
    .await
    .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(header::RANGE, "bytes=0-3".parse().unwrap());
    let response = original
        .clone()
        .serve("source.mp4", Method::GET, headers)
        .await
        .unwrap();
    assert_eq!(response.status(), 206);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "public, max-age=3600"
    );
    task.abort();
}
#[tokio::test]
async fn large_hls_delivery_and_stalled_viewer_isolation() {
    let (base, _, task) = fixture().await;
    let direct = Direct::prepare(
        base.join("/large.m3u8").unwrap(),
        &HashMap::new(),
        "hls",
        permits(),
    )
    .await
    .unwrap();
    let playlist = bytes(
        direct
            .clone()
            .serve("index.m3u8", Method::GET, HeaderMap::new())
            .await
            .unwrap(),
    )
    .await;
    let text = std::str::from_utf8(&playlist).unwrap();
    let segment = text.lines().find(|line| line.starts_with("d-")).unwrap();
    let response = direct
        .clone()
        .serve(segment, Method::GET, HeaderMap::new())
        .await
        .unwrap();
    let received = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(received.len(), 9 * 1024 * 1024);
    assert!(received.iter().all(|b| *b == 7));
    let stalled = direct
        .clone()
        .serve(segment, Method::GET, HeaderMap::new())
        .await
        .unwrap();
    let other = timeout(
        Duration::from_secs(9),
        direct
            .clone()
            .serve("index.m3u8", Method::GET, HeaderMap::new()),
    )
    .await
    .expect("stalled consumer monopolized input")
    .unwrap();
    assert_eq!(other.status(), 200);
    drop(stalled);
    direct.closed.store(true, Ordering::Release);
    task.abort();
}

#[test]
fn hls_keys_maps_and_unsupported_variants_do_not_leak() {
    let base = Url::parse("https://origin.invalid/folder/list.m3u8").unwrap();
    let (text,resources)=rewrite("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"../key?token=secret\"\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXT-X-BYTERANGE:20@0\nfile.ts\n",&base).unwrap();
    assert_eq!(resources.len(), 3);
    assert!(!text.contains("secret"));
    assert!(text.contains("#EXT-X-BYTERANGE:20@0"));
    for text in [
        "#EXTM3U\n#EXT-X-MAP:URI=\"file:///etc/passwd\"",
        "#EXTM3U\n#EXT-X-CONTENT-STEERING:SERVER-URI=\"https://other.invalid\"",
        "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key\"",
        "#EXTM3U\n#EXT-X-SESSION-KEY:METHOD=SAMPLE-AES,URI=\"key\"",
        "#EXTM3U\n#EXT-X-DEFINE:NAME=\"mode\",VALUE=\"live\"",
        "#EXTM3U\n#EXT-X-UNKNOWN:URI=\"thing\"",
    ] {
        assert!(rewrite(text, &base).is_err());
    }
}

#[test]
fn master_playlist_variants_and_renditions_rewrite_to_opaque_keys() {
    let base = Url::parse("https://origin.invalid/folder/master.m3u8").unwrap();
    let (text, resources) = rewrite(
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=100\nvariant.m3u8?token=secret\n#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=50,URI=\"iframe.m3u8\"\n#EXT-X-RENDITION-REPORT:URI=\"report.m3u8\"\n#EXT-X-SESSION-KEY:METHOD=AES-128,URI=\"session.key\"\n#EXT-X-SESSION-DATA:DATA-ID=\"meta\",URI=\"meta.json\"\n#EXT-X-PART:DURATION=1,URI=\"part.m4s\"\n#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"pre.m4s\"\n",
        &base,
    )
    .unwrap();
    assert_eq!(resources.len(), 7);
    assert!(text.contains("#EXT-X-STREAM-INF:BANDWIDTH=100"));
    assert!(!text.contains("secret"));
    for leaked in [
        "variant.m3u8",
        "iframe.m3u8",
        "report.m3u8",
        "session.key",
        "meta.json",
        "part.m4s",
        "pre.m4s",
    ] {
        assert!(!text.contains(leaked), "{leaked} leaked upstream");
    }
    // Variant playlists map to .m3u8 keys so serve() recurses into them.
    let variant = text.lines().find(|l| l.starts_with("d-")).unwrap();
    assert!(variant.ends_with(".m3u8"));
    for (tag, suffix) in [
        ("#EXT-X-I-FRAME-STREAM-INF:", ".m3u8\""),
        ("#EXT-X-RENDITION-REPORT:", ".m3u8\""),
        ("#EXT-X-SESSION-KEY:", ".key\""),
        ("#EXT-X-SESSION-DATA:", ".ts\""),
        ("#EXT-X-PART:", ".m4s\""),
        ("#EXT-X-PRELOAD-HINT:", ".m4s\""),
    ] {
        let line = text.lines().find(|l| l.starts_with(tag)).unwrap();
        assert!(
            line.contains("URI=\"d-") && line.ends_with(suffix),
            "{line}"
        );
    }
}
#[test]
fn original_range_semantics() {
    assert_eq!(range_bounds(None, 16), Some((0, 15)));
    assert_eq!(range_bounds(Some("bytes=-4"), 16), Some((12, 15)));
    assert_eq!(range_bounds(Some("bytes=4-"), 16), Some((4, 15)));
    assert_eq!(range_bounds(Some("bytes=4-900"), 16), Some((4, 15)));
    for raw in [
        "bytes=-0",
        "bytes=20-30",
        "bytes=4-2",
        "bytes=0-1,5-6",
        "garbage",
    ] {
        assert_eq!(range_bounds(Some(raw), 16), None);
    }
}

#[tokio::test]
async fn sequential_live_reports_bytes_backpressure_and_eof() {
    let app = Router::new().route(
        "/stream",
        get(|| async {
            Body::from_stream(async_stream::stream! {
                yield Ok::<_, std::io::Error>(Bytes::from(vec![0x47; 1880]));
                tokio::time::sleep(Duration::from_millis(30)).await;
                yield Ok::<_, std::io::Error>(Bytes::from(vec![0x47; 1880]));
            })
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/stream", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let direct = Direct::prepare(
        Url::parse(&url).unwrap(),
        &HashMap::new(),
        "auto",
        permits(),
    )
    .await
    .unwrap();
    let response = direct
        .clone()
        .serve("source.bin", Method::GET, HeaderMap::new())
        .await
        .unwrap();
    let mut body = response.into_body().into_data_stream();
    assert!(body.next().await.unwrap().is_ok());
    assert!(direct.progress().await.is_some());
    assert_eq!(direct.waiting_consumers.load(Ordering::Acquire), 1);
    while body.next().await.is_some() {}
    assert_eq!(direct.waiting_consumers.load(Ordering::Acquire), 0);
    assert!(
        direct.failed.load(Ordering::Acquire),
        "live EOF requires recovery"
    );
    task.abort();
}
