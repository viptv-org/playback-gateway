use super::*;
impl Workers {
    pub(crate) async fn attach_caption(
        &self,
        principal: &Principal,
        id: &str,
        upload: crate::captions::Upload,
        now: u64,
    ) -> Result<crate::captions::Delivery, &'static str> {
        self.require_active(principal, now).await?;
        let viewer = self
            .sessions
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .get(principal, id, now)?;
        if viewer.status != Status::Ready {
            return Err("session_not_ready");
        }
        let offset = self
            .active
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .get(&viewer.job_id)
            .map(|media| {
                if media.live || media.format == "mp4" {
                    0.0
                } else {
                    media.position
                }
            })
            .ok_or("media_unavailable")?;
        let text =
            tokio::task::spawn_blocking(move || crate::captions::srt(&upload.content_vtt, offset))
                .await
                .map_err(|_| "invalid_caption")??;
        self.require_active(principal, now).await?;
        self.sessions
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .store_caption(principal, id, text, offset, now)
    }

    pub(super) async fn caption_media(
        &self,
        viewer: &str,
        token: &str,
        file: &str,
        now: u64,
        method: axum::http::Method,
    ) -> Result<axum::response::Response, &'static str> {
        let sessions = self.sessions.clone();
        let credentials = self.credentials.clone();
        let viewer = viewer.to_owned();
        let token = token.to_owned();
        let file = file.to_owned();
        let text = tokio::task::spawn_blocking(move || {
            sessions.lock().map_err(|_| "media_unauthorized")?.caption(
                &credentials,
                &viewer,
                &token,
                &file,
                now,
            )
        })
        .await
        .map_err(|_| "media_unauthorized")??;
        use axum::response::IntoResponse;
        let length = text.len();
        let mut response = (
            [
                (
                    axum::http::header::CONTENT_TYPE,
                    "application/x-subrip; charset=utf-8",
                ),
                (axum::http::header::CACHE_CONTROL, "no-store"),
                (axum::http::header::REFERRER_POLICY, "no-referrer"),
                (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            if method == axum::http::Method::HEAD {
                String::new()
            } else {
                text.to_string()
            },
        )
            .into_response();
        response
            .headers_mut()
            .insert(axum::http::header::CONTENT_LENGTH, length.into());
        Ok(response)
    }
}
