use super::*;
use axum::{
    body::Body,
    http::{HeaderMap, Method},
    response::Response,
};

impl PlaybackManager {
    pub(super) async fn serve_tracks(
        &self,
        direct: Arc<direct::Direct>,
        file: &str,
        method: Method,
        headers: HeaderMap,
    ) -> Result<Response, String> {
        let (url, source_headers, permits) = direct.inspection_source();
        let probe = self
            .cached_probe(&url, &source_headers, false, Some(permits.clone()))
            .await
            .ok_or("Media metadata unavailable")?;
        if file == "tracks.json" {
            let mut tracks = probe.tracks("subtitle");
            for track in &mut tracks {
                track.title = track
                    .language
                    .clone()
                    .unwrap_or_else(|| format!("Subtitle {}", track.input_index + 1));
            }
            let mut audio = probe.tracks("audio");
            for track in &mut audio {
                track.title = format!(
                    "Audio {} · {}",
                    track.input_index + 1,
                    track.language.as_deref().unwrap_or("und")
                );
            }
            let body =
                serde_json::to_vec(&serde_json::json!({"subtitles": tracks, "audio": audio}))
                    .map_err(|_| "Media metadata unavailable")?;
            return Response::builder()
                .header("Content-Type", "application/json")
                .header("Cache-Control", "no-store")
                .body(if method == Method::HEAD {
                    Body::empty()
                } else {
                    Body::from(body)
                })
                .map_err(|_| "Invalid media response".into());
        }
        let index = file
            .strip_prefix("subtitle-")
            .and_then(|v| v.strip_suffix(".vtt"))
            .and_then(|v| v.parse::<u32>().ok())
            .ok_or("Subtitle not found")?;
        probe
            .select_subtitle(Some(index))?
            .ok_or("Subtitle not found")?;
        let position = headers
            .get("x-viptv-subtitle-position")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);
        if !position.is_finite() || !(0.0..=604800.0).contains(&position) {
            return Err("Invalid subtitle position".into());
        }
        let start = (position / 60.0).floor() * 60.0;
        let mut command = Command::new(&self.config.ffmpeg);
        command.args(["-v", "error", "-nostdin"]);
        input_args(&mut command, &source_headers);
        command
            .args([
                "-ss",
                &start.to_string(),
                "-i",
                &url,
                "-t",
                "120",
                "-map",
                &format!("0:{index}"),
                "-vn",
                "-an",
                "-c:s",
                "webvtt",
                "-f",
                "webvtt",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|_| "Subtitle extraction unavailable")?;
        let stdout = child.stdout.take().ok_or("Subtitle output unavailable")?;
        let mut owned = ProbeChild {
            child: Some(child),
            permits: Some(permits),
            cleanup_tasks: self.cleanup_tasks.clone(),
        };
        let result = timeout(Duration::from_secs(15), async {
            let bytes = probe_output(stdout, 2 * 1024 * 1024)
                .await
                .map_err(|_| "Subtitle output unavailable")?;
            if !owned
                .child
                .as_mut()
                .unwrap()
                .wait()
                .await
                .is_ok_and(|status| status.success())
            {
                return Err("Subtitle extraction failed");
            }
            Ok(bytes)
        })
        .await;
        owned.reap().await;
        let bytes = result.map_err(|_| "Subtitle extraction timed out")??;
        Response::builder()
            .header("Content-Type", "text/vtt")
            .header("Cache-Control", "no-store")
            .header("X-VIPTV-Subtitle-Offset", start.to_string())
            .body(Body::from(bytes))
            .map_err(|_| "Invalid subtitle response".into())
    }
}
