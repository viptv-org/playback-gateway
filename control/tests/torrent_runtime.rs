#![cfg(target_os = "linux")]
//! Actual enabled service binary, public API and HTTP media in network-none Docker.
use librqbit::{create_torrent, CreateTorrentOptions};
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    process::{Child, Command},
};
const BOOTSTRAP: &str = "torrent-runtime-fixture-bootstrap-not-production";
#[path = "support/torrent.rs"]
mod torrent_fixture;
use torrent_fixture::seed_peer;
mod support;
use support::isolated_address;

async fn json_reply(response: reqwest::Response) -> Value {
    let status = response.status();
    let value: Value = response
        .json()
        .await
        .map_err(|_| "invalid fixture JSON response")
        .unwrap();
    if !status.is_success() {
        // Only fixed documented error codes may enter fixture diagnostics.
        let code = match value["error"]["code"].as_str() {
            Some("processing_failed") => "processing_failed",
            Some("source_preparation_failed") => "source_preparation_failed",
            Some("startup_timeout") => "startup_timeout",
            Some("input_capacity") => "input_capacity",
            Some("output_capacity") => "output_capacity",
            Some("viewer_capacity") => "viewer_capacity",
            _ => "other",
        };
        panic!("public API rejected fixture with status {status}, code {code}");
    }
    value
}
async fn request(
    client: &reqwest::Client,
    base: &str,
    key: &str,
    source: &str,
    id: &str,
    position: u32,
) -> Value {
    json_reply(client.post(format!("{base}/v1/sessions")).bearer_auth(key)
        .header("Idempotency-Key", id)
        .json(&json!({"namespace":"fixture","input":{"url":source,"live":false,"file_index":0},
            "output":{"protocol":"hls","video_codecs":["h264"],"audio_codecs":["aac"],"max_width":320,"max_height":180},
            "position_seconds":position}))
        .send().await.map_err(|_| "session request failed").unwrap()).await
}
async fn capacities(client: &reqwest::Client, base: &str, key: &str) -> Value {
    json_reply(
        client
            .get(format!("{base}/v1/capabilities"))
            .bearer_auth(key)
            .send()
            .await
            .map_err(|_| "capability request failed")
            .unwrap(),
    )
    .await
}
async fn decoded_frame(ffmpeg: &str, input: &str, position: u32) -> Vec<u8> {
    let mut command = Command::new(ffmpeg);
    command.args([
        "-v",
        "error",
        "-nostdin",
        "-protocol_whitelist",
        "file,http,tcp,crypto,data",
    ]);
    if position > 0 {
        command.args(["-ss", &position.to_string()]);
    }
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        command
            .arg("-i")
            .arg(input)
            .args([
                "-threads",
                "2",
                "-frames:v",
                "1",
                "-pix_fmt",
                "rgb24",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("HTTP media decode deadline")
    .unwrap();
    assert!(result.status.success(), "public HTTP HLS must decode");
    assert_eq!(result.stdout.len(), 320 * 180 * 3);
    result.stdout
}
async fn service_ready(child: &mut Child, client: &reqwest::Client, base: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "service exited before ready"
            );
            if client
                .get(format!("{base}/ready"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("service readiness deadline");
}
fn media_url(base: &str, viewer: &Value) -> String {
    format!(
        "{base}{}",
        viewer["playback"]["url"]
            .as_str()
            .expect("public media URL")
    )
}
fn control_url(base: &str, viewer: &Value) -> String {
    format!(
        "{base}/v1/sessions/{}",
        viewer["id"].as_str().expect("viewer ID")
    )
}
#[tokio::test]
#[ignore = "requires real service/FFmpeg image, Docker network none and NET_ADMIN"]
async fn standalone_enabled_torrent_public_api_shares_seeks_and_reclaims() {
    isolated_address();
    let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").unwrap();
    let root = tempfile::tempdir().unwrap();
    let seed_dir = root.path().join("seed");
    std::fs::create_dir(&seed_dir).unwrap();
    let movie = seed_dir.join("clip.mkv");
    let generated = Command::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-nostdin",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=25",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "8",
            "-c:v",
            "libx264",
            "-threads",
            "2",
            "-preset",
            "ultrafast",
            "-g",
            "25",
            "-c:a",
            "aac",
        ])
        .arg(&movie)
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(generated.status.success(), "generated clip must encode");
    let movie_size = std::fs::metadata(&movie).unwrap().len();
    let torrent = create_torrent(
        &movie,
        CreateTorrentOptions {
            piece_length: Some(16 * 1024),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let metainfo = torrent.as_bytes().unwrap();
    let info_hash = torrent.info_hash().0;
    let listener = TcpListener::bind("11.255.255.1:0").await.unwrap();
    let source_url = format!("http://{}/clip.torrent", listener.local_addr().unwrap());
    let route_bytes = metainfo.clone();
    let route = axum::Router::new().route(
        "/clip.torrent",
        axum::routing::get(move || {
            let bytes = route_bytes.clone();
            async move { bytes }
        }),
    );
    let http = tokio::spawn(async move {
        let _ = axum::serve(listener, route).await;
    });
    struct AbortHttp(tokio::task::JoinHandle<()>);
    impl Drop for AbortHttp {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _http = AbortHttp(http);
    let reservation = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let reservation = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_address = reservation.local_addr().unwrap();
    drop(reservation);
    let mut child = Command::new(
        std::env::var("PLAYBACK_TEST_SERVICE_BINARY")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_playback-gateway").into()),
    )
    .env("API_KEY", BOOTSTRAP)
    .env("DATA_DIR", root.path().join("state"))
    .env("BIND_ADDRESS", address.to_string())
    .env("TORRENT_ENABLED", "true")
    .env("TORRENT_LISTEN_PORT", peer_address.port().to_string())
    .env("FFMPEG_PATH", &ffmpeg)
    .env(
        "FFPROBE_PATH",
        std::env::var("PLAYBACK_TEST_FFPROBE").unwrap(),
    )
    .env("MAX_OUTPUTS", "2")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .kill_on_drop(true)
    .spawn()
    .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(25))
        .build()
        .unwrap();
    let base = format!("http://{address}");
    service_ready(&mut child, &client, &base).await;
    let issued = json_reply(client.post(format!("{base}/v1/keys")).bearer_auth(BOOTSTRAP)
        .json(&json!({"label":"actual torrent HTTP fixture","namespaces":["fixture"],
            "scopes":["capabilities","create","read","renew","release"],"quotas":{"inputs":2,"outputs":2,"viewers":4}}))
        .send().await.map_err(|_|"key request failed").unwrap()).await;
    let key = issued["secret"]
        .as_str()
        .expect("fixture integration key")
        .to_owned();
    assert_eq!(capacities(&client, &base, &key).await["torrent"], true);
    let first_request = {
        let client = client.clone();
        let base = base.clone();
        let key = key.clone();
        let source = source_url.clone();
        tokio::spawn(async move { request(&client, &base, &key, &source, "first", 0).await })
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if TcpStream::connect(peer_address).await.is_ok() {
                break;
            }
            assert!(
                !first_request.is_finished(),
                "first admission finished before peer listener"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("actual service inbound peer listener deadline");
    let seeder = tokio::spawn(seed_peer(
        peer_address,
        info_hash,
        std::fs::read(&movie).unwrap(),
        Duration::ZERO,
    ));
    struct AbortSeed(tokio::task::JoinHandle<()>);
    impl Drop for AbortSeed {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _seed = AbortSeed(seeder);
    let first = tokio::time::timeout(Duration::from_secs(25), first_request)
        .await
        .expect("torrent source admission deadline")
        .unwrap();
    println!("Actual enabled service admitted the locally seeded torrent through HTTP.");
    assert_eq!(first["input_file"]["name"], "clip.mkv");
    assert_eq!(first["input_file"]["index"], 0);
    assert_eq!(first["input_file"]["size"], movie_size);
    let public = first.to_string();
    let hash = info_hash
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert!(
        !public.contains(&source_url) && !public.contains(&hash),
        "public replies must not expose source identity"
    );
    let other_key = json_reply(
        client
            .post(format!("{base}/v1/keys"))
            .bearer_auth(BOOTSTRAP)
            .json(
                &json!({"label":"other namespace fixture","namespaces":["other"],
            "scopes":["read"],"quotas":{"inputs":1,"outputs":1,"viewers":1}}),
            )
            .send()
            .await
            .map_err(|_| "other key request failed")
            .unwrap(),
    )
    .await;
    let denied_control = client
        .get(control_url(&base, &first))
        .bearer_auth(other_key["secret"].as_str().expect("other integration key"))
        .send()
        .await
        .map_err(|_| "scoped control request failed")
        .unwrap();
    assert_eq!(denied_control.status(), reqwest::StatusCode::NOT_FOUND);
    let second = request(&client, &base, &key, &source_url, "second", 0).await;
    assert!(
        first["job_id"] == second["job_id"],
        "compatible public viewers must share one output"
    );
    let first_frame = decoded_frame(&ffmpeg, &media_url(&base, &first), 0).await;
    let late = request(&client, &base, &key, &source_url, "late", 0).await;
    assert!(
        first["job_id"] == late["job_id"],
        "late public viewer must share the ready output"
    );
    let available = capacities(&client, &base, &key).await;
    assert_eq!(available["available"]["inputs"], 1);
    assert_eq!(available["available"]["outputs"], 1);
    let sought = request(&client, &base, &key, &source_url, "seek", 3).await;
    assert!(
        sought["job_id"] != first["job_id"],
        "seek origin must reserve a distinct output"
    );
    let sought_frame = decoded_frame(&ffmpeg, &media_url(&base, &sought), 0).await;
    let expected_frame = decoded_frame(&ffmpeg, movie.to_str().unwrap(), 3).await;
    let mse = |a: &[u8], b: &[u8]| -> f64 {
        a.iter()
            .zip(b)
            .map(|(a, b)| {
                let d = f64::from(*a) - f64::from(*b);
                d * d
            })
            .sum::<f64>()
            / a.len() as f64
    };
    assert!(
        mse(&sought_frame, &expected_frame) < 100.0,
        "HTTP seek frame must match source frame at three seconds"
    );
    assert!(
        mse(&sought_frame, &expected_frame) < mse(&sought_frame, &first_frame) * 0.1,
        "HTTP seek must not restart at zero"
    );
    let denied = client
        .get(format!(
            "{base}/media/{}/invalid/index.m3u8",
            first["id"].as_str().unwrap()
        ))
        .send()
        .await
        .map_err(|_| "invalid media credential request failed")
        .unwrap();
    assert!(
        !denied.status().is_success(),
        "invalid viewer media token must be denied"
    );
    json_reply(
        client
            .post(format!("{}/renew", control_url(&base, &late)))
            .bearer_auth(&key)
            .send()
            .await
            .map_err(|_| "renewal failed")
            .unwrap(),
    )
    .await;
    let released_media = media_url(&base, &first);
    for viewer in [&first, &second, &late, &sought] {
        let response = client
            .delete(control_url(&base, viewer))
            .bearer_auth(&key)
            .send()
            .await
            .map_err(|_| "release failed")
            .unwrap();
        assert!(response.status().is_success(), "release must succeed");
    }
    assert!(
        !client
            .get(&released_media)
            .send()
            .await
            .map_err(|_| "released media request failed")
            .unwrap()
            .status()
            .is_success(),
        "released viewer media credential must stop authorizing reads"
    );
    tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            let available = capacities(&client, &base, &key).await;
            if available["available"]["inputs"] == 2
                && available["available"]["outputs"] == 2
                && available["available"]["viewers"] == 4
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("last public release must reclaim source and output admission");
    assert_eq!(
        unsafe { libc::kill(child.id().unwrap() as libc::pid_t, libc::SIGTERM) },
        0
    );
    assert!(tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
    println!("Two viewers plus late join shared one output; authenticated HTTP HLS decoded; three-second seek matched; renewal and last release reclaimed all admission; actual service shut down.");
}
