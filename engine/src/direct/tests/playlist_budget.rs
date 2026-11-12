use super::*;

async fn origin(playlist: String) -> (Url, tokio::task::JoinHandle<()>) {
    let (disguised, _) = disguised_segment();
    let app = Router::new()
        .route(
            "/vod.m3u8",
            get(move || {
                let playlist = playlist.clone();
                async move { playlist }
            }),
        )
        .route(
            "/seg-0.png",
            get(move || {
                let body = disguised.clone();
                async move { ([(header::CONTENT_TYPE, "image/png")], body) }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, task)
}

fn vod_playlist(segments: usize, signature_length: usize) -> String {
    let signature = "signed-token-".repeat(signature_length / 13 + 1);
    let mut text = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-PLAYLIST-TYPE:VOD\n".to_owned();
    for index in 0..segments {
        text.push_str(&format!(
            "#EXTINF:3.2,\nseg-{index}.png?sig={}\n",
            &signature[..signature_length]
        ));
    }
    text.push_str("#EXT-X-ENDLIST\n");
    text
}

#[tokio::test]
async fn large_vod_playlist_preserves_opaque_routes_and_disguised_segment_validation() {
    let playlist = vod_playlist(2_800, 180);
    assert!(playlist.len() > 256 * 1024 && playlist.len() < 1024 * 1024);
    let (base, task) = origin(playlist).await;
    let result = Direct::prepare(
        base.join("/vod.m3u8").unwrap(),
        &HashMap::new(),
        "hls",
        permits(),
    )
    .await;
    let direct = result.expect("ordinary long VOD playlists must fit the bounded manifest budget");
    let response = direct
        .clone()
        .serve("index.m3u8", Method::GET, HeaderMap::new())
        .await
        .unwrap();
    let text = String::from_utf8(bytes(response).await.to_vec()).unwrap();
    let segments: Vec<_> = text.lines().filter(|line| line.starts_with("d-")).collect();
    assert_eq!(segments.len(), 2_800);
    assert!(text.contains("#EXT-X-ENDLIST"));
    assert!(!text.contains("signed-token"));
    assert!(segments.iter().all(|segment| segment.ends_with(".ts")));
    let response = direct
        .clone()
        .serve(segments[0], Method::GET, HeaderMap::new())
        .await
        .unwrap();
    assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp2t");
    assert_eq!(bytes(response).await.as_ref(), disguised_segment().1);
    task.abort();
}

#[tokio::test]
async fn manifest_body_budget_still_rejects_more_than_one_mebibyte() {
    let playlist = format!("#EXTM3U\n#{}\n", "x".repeat(1024 * 1024));
    let (base, task) = origin(playlist).await;
    let result = Direct::prepare(
        base.join("/vod.m3u8").unwrap(),
        &HashMap::new(),
        "hls",
        permits(),
    )
    .await;
    assert!(matches!(result, Err(error) if error == "Media resource exceeds limit"));
    task.abort();
}

#[tokio::test]
async fn resource_count_budget_remains_independent_of_manifest_bytes() {
    let playlist = vod_playlist(4_097, 0);
    assert!(playlist.len() < 256 * 1024);
    let (base, task) = origin(playlist).await;
    let result = Direct::prepare(
        base.join("/vod.m3u8").unwrap(),
        &HashMap::new(),
        "hls",
        permits(),
    )
    .await;
    assert!(matches!(result, Err(error) if error == "Too many HLS resources"));
    task.abort();
}
