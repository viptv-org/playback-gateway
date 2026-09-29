use super::*;

mod browser;
mod compat;
mod hdr;
mod lifecycle;
mod probe;
mod recovery;
mod remux;
mod selection;
mod throttle;
mod vaapi;

#[cfg(unix)]
fn scripted_probe(root: &std::path::Path, body: &str) -> Arc<PlaybackManager> {
    use std::os::unix::fs::PermissionsExt;
    let script = root.join("probe.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nprintf x >> \"$0.count\"\n{body}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    PlaybackManager::new(Config {
        ffmpeg: root.join("missing-ffmpeg"),
        ffprobe: script,
        root: root.join("media"),
        max_sessions: 1,
        ttl: Duration::from_secs(60),
    })
}

#[cfg(unix)]
async fn wait_probe_file(path: &std::path::Path) -> String {
    timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(value) = tokio::fs::read_to_string(path).await {
                if !value.is_empty() {
                    return value;
                }
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}
