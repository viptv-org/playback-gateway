use super::*;
#[tokio::test]
async fn caption_storage_is_per_viewer_bounded_and_release_removes_its_authority() {
    let (_root, workers, p) = fixture(false).await;
    let mut input = request(1920);
    input.input.live = false;
    let viewer = workers
        .create(&p, &input, "caption-capacity", 0)
        .await
        .unwrap();
    let mut first = String::new();
    for index in 0..8 {
        let result = workers
            .attach_caption(
                &p,
                &viewer.id,
                crate::captions::Upload {
                    content_vtt: format!("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nCue {index}\n"),
                },
                1,
            )
            .await
            .unwrap();
        if index == 0 {
            first = result.url;
        }
    }
    let result = workers
        .attach_caption(
            &p,
            &viewer.id,
            crate::captions::Upload {
                content_vtt: "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nOverflow\n".into(),
            },
            1,
        )
        .await;
    assert_eq!(result.err(), Some("caption_capacity"));
    let parts = first.split('/').collect::<Vec<_>>();
    assert!(workers
        .media(
            &viewer.id,
            parts[3],
            parts[4],
            2,
            Method::GET,
            HeaderMap::new()
        )
        .await
        .is_ok());
    let other = workers
        .create(&p, &input, "caption-other-viewer", 2)
        .await
        .unwrap();
    let delivered = workers.delivery(&p, &other, 2).unwrap().unwrap();
    let token = delivered.url.split('/').nth(3).unwrap();
    assert_eq!(
        workers
            .media(&other.id, token, parts[4], 2, Method::GET, HeaderMap::new())
            .await
            .err(),
        Some("media_not_found")
    );
    workers
        .sessions
        .lock()
        .unwrap()
        .release(&p, &viewer.id, 3)
        .unwrap();
    assert_eq!(
        workers
            .media(
                &viewer.id,
                parts[3],
                parts[4],
                3,
                Method::GET,
                HeaderMap::new()
            )
            .await
            .err(),
        Some("media_unauthorized")
    );
    workers.shutdown().await;
}
