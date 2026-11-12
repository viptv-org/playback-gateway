use super::*;
use std::time::Duration;
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

#[tokio::test]
async fn subtitle_sync_is_viewer_scoped_monotonic_and_manual_fenced() {
    let (_root,workers,p)=fixture(false).await;
    let key=workers.credentials.issue(BOOTSTRAP,p.key.spec.clone(),0).unwrap();
    let foreign=workers.credentials.issue(BOOTSTRAP,p.key.spec.clone(),0).unwrap();
    let app=router(Api{workers:workers.clone(),ready:Arc::new(AtomicBool::new(true))});
    let mut input=request(1920);input.input.live=false;
    let (status,_,created)=call(&app,"POST","/v1/sessions",&key.secret,Some("sync-viewer"),serde_json::to_string(&input).unwrap()).await;
    assert_eq!(status,StatusCode::CREATED);
    let created:Value=serde_json::from_slice(&created).unwrap();let id=created["id"].as_str().unwrap();
    let endpoint=format!("/v1/sessions/{id}/subtitle-sync");
    let body=json!({"content_vtt":"WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nDialogue\n","playback_generation":"generation"}).to_string();
    assert_eq!(call(&app,"POST",&endpoint,&foreign.secret,None,body.clone()).await.0,StatusCode::NOT_FOUND);
    let (status,_,record)=call(&app,"POST",&endpoint,&key.secret,None,body).await;assert_eq!(status,StatusCode::OK);
    let record:Value=serde_json::from_slice(&record).unwrap();assert_eq!(record["revision"],1);
    assert_eq!(record["state"],"unavailable");assert_eq!(record["manual"],false);
    let timestamp=now().unwrap();
    let principal=workers.credentials.authenticate(&key.secret,timestamp as i64).unwrap();
    let expected=workers.sessions.lock().unwrap().sync(&principal,id,timestamp).unwrap();
    assert_eq!(call(&app,"GET",&endpoint,&foreign.secret,None,String::new()).await.0,StatusCode::NOT_FOUND);
    assert_eq!(call(&app,"PUT",&endpoint,&key.secret,None,json!({"revision":1,"manual":true}).to_string()).await.0,StatusCode::OK);
    assert_eq!(call(&app,"PUT",&endpoint,&key.secret,None,json!({"revision":1,"manual":false}).to_string()).await.0,StatusCode::CONFLICT);
    let (_,_,record)=call(&app,"GET",&endpoint,&key.secret,None,String::new()).await;
    let record:Value=serde_json::from_slice(&record).unwrap();assert_eq!(record["revision"],2);assert_eq!(record["manual"],true);
    assert!(workers.sessions.lock().unwrap().finish_sync(&principal,id,&expected,Ok(json!({"algorithmVersion":"activity-offset-affine-v1","state":"accepted","correction":{"offsetUs":2000000,"anchorUs":0,"scale":1.0}})),timestamp).is_err());
    let replacement=json!({"content_vtt":"WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nDialogue\n","playback_generation":"replacement"}).to_string();
    assert_eq!(call(&app,"POST",&endpoint,&key.secret,None,replacement).await.0,StatusCode::OK);
    assert_eq!(call(&app,"DELETE",&endpoint,&key.secret,None,json!({"playback_generation":"generation"}).to_string()).await.0,StatusCode::CONFLICT);
    call(&app,"DELETE",&format!("/v1/sessions/{id}"),&key.secret,None,String::new()).await;
    assert_ne!(call(&app,"GET",&endpoint,&key.secret,None,String::new()).await.0,StatusCode::OK);
}

#[tokio::test]
#[ignore = "requires owned retained HLS/VAD/estimator fixture; seeds producer ledger without upstream traffic"]
async fn real_retained_worker_publishes_through_authenticated_caption_lifecycle() {
    let (root,workers,p)=fixture(false).await;
    let key=workers.credentials.issue(BOOTSTRAP,p.key.spec.clone(),0).unwrap();
    let foreign=workers.credentials.issue(BOOTSTRAP,p.key.spec.clone(),0).unwrap();
    let app=router(Api{workers:workers.clone(),ready:Arc::new(AtomicBool::new(true))});
    let mut input=request(1920);input.input.live=false;
    let (status,_,created)=call(&app,"POST","/v1/sessions",&key.secret,Some("real-retained-sync"),serde_json::to_string(&input).unwrap()).await;
    assert_eq!(status,StatusCode::CREATED);
    let created:Value=serde_json::from_slice(&created).unwrap();
    let id=created["id"].as_str().unwrap();let job=created["job_id"].as_str().unwrap();
    let engine_id=workers.active.lock().unwrap().get(job).unwrap().id.clone();
    let directory=root.path().join("media").join(engine_id);
    let fixture=std::path::PathBuf::from(std::env::var("PLAYBACK_SYNC_FIXTURE").unwrap());
    for file in std::fs::read_dir(&fixture).unwrap().map(Result::unwrap) {
        let name=file.file_name();let name=name.to_str().unwrap();
        if name=="index.m3u8" || name.starts_with("segment-") && name.ends_with(".ts") {std::fs::copy(file.path(),directory.join(name)).unwrap();}
    }
    for (script,variable) in [("encoder","PLAYBACK_TEST_FFMPEG"),("probe","PLAYBACK_TEST_FFPROBE")] {
        let path=root.path().join(script);std::fs::remove_file(&path).unwrap();std::os::unix::fs::symlink(std::env::var(variable).unwrap(),path).unwrap();
    }
    std::env::set_var("VIPTV_SUBTITLE_SYNC_VAD",std::env::var("PLAYBACK_SYNC_VAD").unwrap());
    std::env::set_var("VIPTV_SUBTITLE_SYNC_ESTIMATOR",std::env::var("PLAYBACK_SYNC_ESTIMATOR").unwrap());
    let content=std::fs::read_to_string(std::env::var("PLAYBACK_SYNC_CAPTIONS").unwrap()).unwrap();
    let endpoint=format!("/v1/sessions/{id}/subtitle-sync");
    let (status,_,record)=call(&app,"POST",&endpoint,&key.secret,None,json!({"content_vtt":content,"playback_generation":"owned-fixture"}).to_string()).await;
    assert_eq!(status,StatusCode::OK);let record:Value=serde_json::from_slice(&record).unwrap();assert_eq!(record["state"],"observing");
    let final_record=tokio::time::timeout(Duration::from_secs(45),async {
        loop {
            let (status,_,record)=call(&app,"GET",&endpoint,&key.secret,None,String::new()).await;assert_eq!(status,StatusCode::OK);
            let record:Value=serde_json::from_slice(&record).unwrap();
            if record["state"]!="observing" {break record;}
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }).await.unwrap();
    assert_eq!(final_record["state"],"accepted","{final_record}");
    assert!((final_record["correction"]["offsetUs"].as_i64().unwrap()-2_000_000).abs()<=250_000);
    assert!(final_record["revision"].as_u64().unwrap()>record["revision"].as_u64().unwrap());
    assert_eq!(call(&app,"GET",&endpoint,&foreign.secret,None,String::new()).await.0,StatusCode::NOT_FOUND);
    let (status,_,viewer)=call(&app,"GET",&format!("/v1/sessions/{id}"),&key.secret,None,String::new()).await;assert_eq!(status,StatusCode::OK);
    let viewer:Value=serde_json::from_slice(&viewer).unwrap();assert_eq!(viewer["status"],"ready");assert_eq!(viewer["job_id"],created["job_id"]);
    if let Ok(path)=std::env::var("PLAYBACK_SYNC_RECORD") {std::fs::write(path,serde_json::to_vec(&json!({"record":final_record,"canonical_vtt":content})).unwrap()).unwrap();}
    call(&app,"DELETE",&format!("/v1/sessions/{id}"),&key.secret,None,String::new()).await;
    workers.shutdown().await;
    std::env::remove_var("VIPTV_SUBTITLE_SYNC_VAD");std::env::remove_var("VIPTV_SUBTITLE_SYNC_ESTIMATOR");
}
