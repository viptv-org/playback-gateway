use super::*;
use crate::captions::Delivery;
use std::sync::Arc;
impl Sessions {
    pub(crate) fn store_caption(
        &mut self,
        principal: &Principal,
        id: &str,
        text: String,
        offset: f64,
        now: u64,
    ) -> Result<Delivery, &'static str> {
        let viewer = self.get(principal, id, now)?;
        if viewer.status != Status::Ready {
            return Err("session_not_ready");
        }
        let owned = self.viewers.get(id).ok_or("session_not_found")?;
        principal.require(&owned.namespace, Scope::Create)?;
        let file = format!(
            "caption_{}.srt",
            hash(&text)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        if !owned.captions.contains_key(&file) {
            let entries = self
                .viewers
                .values()
                .map(|v| v.captions.len())
                .sum::<usize>();
            let bytes = self
                .viewers
                .values()
                .flat_map(|v| v.captions.values())
                .map(|v| v.len())
                .sum::<usize>();
            if owned.captions.len() >= 8
                || entries >= 128
                || bytes.saturating_add(text.len()) > 32 * 1024 * 1024
            {
                return Err("caption_capacity");
            }
        }
        let token = self.ensure_media(principal, id, now)?;
        self.viewers
            .get_mut(id)
            .ok_or("session_not_found")?
            .captions
            .insert(file.clone(), Arc::from(text));
        Ok(Delivery {
            url: format!("/media/{id}/{}/{file}", token.token),
            format: "srt",
            timeline_offset_seconds: offset,
        })
    }
    pub(crate) fn caption(
        &self,
        credentials: &Credentials,
        id: &str,
        token: &str,
        file: &str,
        now: u64,
    ) -> Result<Arc<str>, &'static str> {
        self.authorize_media(credentials, id, token, now)?;
        self.viewers
            .get(id)
            .and_then(|v| v.captions.get(file))
            .cloned()
            .ok_or("media_not_found")
    }
}
