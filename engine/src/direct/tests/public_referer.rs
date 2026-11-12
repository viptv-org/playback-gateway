use super::*;

#[tokio::test]
async fn split_host_media_keeps_only_public_origin_referer() {
    let root_headers = Arc::new(std::sync::Mutex::new(None::<HeaderMap>));
    let child_headers = Arc::new(std::sync::Mutex::new(None::<HeaderMap>));
    let root_seen = root_headers.clone();
    let child_seen = child_headers.clone();
    let (disguised, media) = disguised_segment();
    let app = Router::new()
        .route("/root.m3u8", get(move |headers: HeaderMap| {
            let seen = root_seen.clone();
            async move {
                *seen.lock().unwrap() = Some(headers);
                "#EXTM3U\n#EXTINF:3.2,\nhttp://cdn.example/asset.png?token=fixture\n#EXT-X-ENDLIST\n"
            }
        }))
        .route("/asset.png", get(move |headers: HeaderMap| {
            let seen = child_seen.clone();
            let body = disguised.clone();
            async move {
                let allowed = headers.get(header::REFERER).is_some_and(|value| value == "http://player.example/");
                *seen.lock().unwrap() = Some(headers);
                Response::builder()
                    .status(if allowed { StatusCode::OK } else { StatusCode::NOT_FOUND })
                    .header(header::CONTENT_TYPE, "image/png")
                    .body(if allowed { Body::from(body) } else { Body::empty() })
                    .unwrap()
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let source_headers = HashMap::from([
        (crate::VALIDATED_EGRESS_PROXY_HEADER.to_owned(), proxy),
        (
            "Referer".into(),
            "http://player.example/private/viewer?key=fixture#private".into(),
        ),
        ("Authorization".into(), "Bearer fixture-secret".into()),
        ("Cookie".into(), "session=fixture-cookie".into()),
        ("X-api-key".into(), "fixture-api-key".into()),
        ("X-custom-secret".into(), "fixture-custom-secret".into()),
        ("Origin".into(), "http://private-origin.example".into()),
        ("User-Agent".into(), "fixture-private-agent".into()),
    ]);
    let direct = Direct::prepare(
        Url::parse("http://origin.example/root.m3u8").unwrap(),
        &source_headers,
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
    let playlist = String::from_utf8(bytes(response).await.to_vec()).unwrap();
    let file = playlist
        .lines()
        .find(|line| line.starts_with("d-"))
        .unwrap();
    let response = direct
        .clone()
        .serve(file, Method::GET, HeaderMap::new())
        .await
        .expect("public origin Referer must allow the split-host media request");
    assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp2t");
    assert_eq!(bytes(response).await, media);
    let root = root_headers.lock().unwrap().clone().unwrap();
    let child = child_headers.lock().unwrap().clone().unwrap();
    assert_eq!(root[header::AUTHORIZATION], "Bearer fixture-secret");
    assert_eq!(root[header::COOKIE], "session=fixture-cookie");
    assert_eq!(
        root[header::REFERER],
        "http://player.example/private/viewer?key=fixture#private"
    );
    assert_eq!(child[header::REFERER], "http://player.example/");
    for refused in [
        "authorization",
        "cookie",
        "x-api-key",
        "x-custom-secret",
        "origin",
        "user-agent",
    ] {
        assert!(
            !child.contains_key(refused),
            "{refused} escaped the root origin"
        );
    }
    assert!(!child[header::REFERER].to_str().unwrap().contains("private"));
    assert!(!child[header::REFERER].to_str().unwrap().contains("key="));
    task.abort();
}
