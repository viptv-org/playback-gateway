use super::*;
#[tokio::test]
async fn viewer_caption_http_is_srt_scoped_nonrenewing_and_revocable() {
    let (root, workers, p) = fixture(false).await;
    let key = workers
        .credentials
        .issue(BOOTSTRAP, p.key.spec.clone(), 0)
        .unwrap();
    let foreign = workers
        .credentials
        .issue(BOOTSTRAP, p.key.spec.clone(), 0)
        .unwrap();
    let app = router(Api {
        workers: workers.clone(),
        ready: Arc::new(AtomicBool::new(true)),
    });
    let mut input = request(1920);
    input.input.live = false;
    input.position_seconds = 3.0;
    let (status, _, created) = call(
        &app,
        "POST",
        "/v1/sessions",
        &key.secret,
        Some("caption-viewer"),
        serde_json::to_string(&input).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let created: Value = serde_json::from_slice(&created).unwrap();
    assert_eq!(created["external_captions"], json!(["srt"]));
    let id = created["id"].as_str().unwrap();
    let endpoint = format!("/v1/sessions/{id}/captions");
    let body=json!({"content_vtt":"WEBVTT\n\n00:00:02.000 --> 00:00:05.000\nCafé\n\n00:00:07.000 --> 00:00:08.000\nNext\n"}).to_string();
    assert_eq!(
        call(&app, "POST", &endpoint, &foreign.secret, None, body.clone())
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let (status, _, delivery) =
        call(&app, "POST", &endpoint, &key.secret, None, body.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let delivery: Value = serde_json::from_slice(&delivery).unwrap();
    assert_eq!(delivery["format"], "srt");
    assert_eq!(delivery["timeline_offset_seconds"], 3.0);
    let url = delivery["url"].as_str().unwrap();
    assert!(url.starts_with(&format!("/media/{id}/pgm_")));
    assert!(url.ends_with(".srt"));
    let (_, _, again) = call(&app, "POST", &endpoint, &key.secret, None, body).await;
    assert_eq!(serde_json::from_slice::<Value>(&again).unwrap()["url"], url);
    let (status, headers, caption) = call(&app, "GET", url, "", None, String::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "application/x-subrip; charset=utf-8"
    );
    assert_eq!(
        String::from_utf8(caption).unwrap(),
        "1\n00:00:00,000 --> 00:00:02,000\nCafé\n\n2\n00:00:04,000 --> 00:00:05,000\nNext\n\n"
    );
    assert!(call(&app, "HEAD", url, "", None, String::new())
        .await
        .2
        .is_empty());
    let (_, _, after) = call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}"),
        &key.secret,
        None,
        String::new(),
    )
    .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&after).unwrap()["expires_at"],
        created["expires_at"]
    );
    for body in [
        json!({"url":"https://private.invalid/subtitle"}),
        json!({"content_vtt":"not subtitles"}),
        json!({"content_vtt":"x".repeat(crate::captions::MAX_BYTES+1)}),
    ] {
        assert_eq!(
            call(&app, "POST", &endpoint, &key.secret, None, body.to_string())
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    let parts = url.split('/').collect::<Vec<_>>();
    assert_eq!(
        workers
            .media(
                id,
                parts[3],
                parts[4],
                created["expires_at"].as_u64().unwrap(),
                Method::GET,
                HeaderMap::new()
            )
            .await
            .err(),
        Some("media_unauthorized")
    );
    assert_eq!(
        call(
            &app,
            "POST",
            &endpoint,
            parts[3],
            None,
            json!({"content_vtt":"WEBVTT"}).to_string()
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &app,
            "DELETE",
            &format!("/v1/keys/{}", key.key.id),
            BOOTSTRAP,
            None,
            String::new()
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&app, "GET", url, "", None, String::new()).await.0,
        StatusCode::UNAUTHORIZED
    );
    workers.shutdown().await;
    drop(root);
}
