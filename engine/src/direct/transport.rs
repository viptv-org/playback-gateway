use super::playlist::{public_host, public_ip};
use super::*;

impl Direct {
    async fn destination_client(&self, url: &Url) -> Result<reqwest::Client, String> {
        if url.origin() == self.root.origin() {
            return Ok(self.client.clone());
        }
        if !public_host(url) {
            return Err("Unsafe media destination".into());
        }
        // An HTTP egress proxy resolves hostnames itself. Until its resolved
        // destination can be enforced, cross-origin WARP media uses managed HLS.
        if self.proxy.is_some() {
            return Err("Cross-origin proxy media requires managed playback".into());
        }
        let key = url.origin().ascii_serialization();
        if let Some(client) = self.destinations.lock().await.get(&key) {
            return Ok(client.clone());
        }
        let host = url.host_str().ok_or("Invalid media host")?;
        let port = url.port_or_known_default().ok_or("Invalid media port")?;
        let addresses = timeout(
            Duration::from_secs(3),
            tokio::net::lookup_host((host, port)),
        )
        .await
        .map_err(|_| "Media DNS timed out")?
        .map_err(|_| "Media DNS unavailable")?
        .collect::<Vec<_>>();
        if addresses.is_empty() || addresses.iter().any(|address| !public_ip(address.ip())) {
            return Err("Unsafe media destination".into());
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(host, &addresses)
            .build()
            .map_err(|_| "Media transport unavailable")?;
        let mut destinations = self.destinations.lock().await;
        if destinations.len() >= 32 {
            return Err("Too many media destinations".into());
        }
        destinations.insert(key, client.clone());
        Ok(client)
    }
    pub(super) async fn request(
        &self,
        mut url: Url,
        range: Option<&str>,
    ) -> Result<reqwest::Response, String> {
        for _ in 0..4 {
            if self.closed.load(Ordering::Acquire) {
                return Err("Media expired".into());
            }
            crate::validate_url(url.as_str())?;
            let client = self.destination_client(&url).await?;
            let mut headers = self.headers.clone();
            if url.origin() != self.root.origin() {
                headers.remove(header::AUTHORIZATION);
                headers.remove(header::COOKIE);
            }
            let mut request = client
                .get(url.clone())
                .headers(headers)
                .header(header::ACCEPT_ENCODING, "identity");
            if let Some(range) = range {
                request = request.header(header::RANGE, range);
            }
            if let Some(validator) = &self.validator {
                request = request.header(header::IF_MATCH, validator);
            }
            let response = request
                .send()
                .await
                .map_err(|_| "Media origin unavailable")?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or("Invalid media redirect")?;
                url = url.join(location).map_err(|_| "Invalid media redirect")?;
                continue;
            }
            if !response.status().is_success() {
                return Err(format!("Media origin HTTP {}", response.status().as_u16()));
            }
            return Ok(response);
        }
        Err("Too many media redirects".into())
    }
    pub(super) async fn playlist(&self, url: Url) -> Result<Bytes, String> {
        let key = url.as_str().to_owned();
        // A one-second TTL absorbs poll storms; the live window still advances.
        if let Some((at, bytes)) = self.playlists.lock().await.get(&key) {
            if at.elapsed() < PLAYLIST_TTL {
                return Ok(bytes.clone());
            }
        }
        let permit = self
            .fetch
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "Media expired")?;
        let response = self.request(url, None).await?;
        let base = response.url().clone();
        let bytes = bounded(response, 256 * 1024).await?;
        // Admission covers origin I/O only; rewriting and map updates are local.
        drop(permit);
        let text = std::str::from_utf8(&bytes).map_err(|_| "Invalid HLS playlist")?;
        let (output, resources) = rewrite(text, &base)?;
        let mut origins = HashSet::new();
        for url in resources.values() {
            if origins.insert(url.origin()) {
                self.destination_client(url).await?;
            }
        }
        let mut map = self.resources.lock().await;
        if map.len() + resources.len() > RESOURCE_LIMIT {
            map.clear();
        }
        map.extend(resources);
        *self.progress.lock().await = Some(
            text.lines()
                .filter(|l| l.starts_with("#EXT-X-MEDIA-SEQUENCE:") || !l.starts_with('#'))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let output = Bytes::from(output);
        let mut playlists = self.playlists.lock().await;
        playlists.retain(|_, (at, _)| at.elapsed() < PLAYLIST_TTL);
        if playlists.values().map(|(_, b)| b.len()).sum::<usize>() + output.len()
            > PLAYLIST_CACHE_LIMIT
        {
            playlists.clear();
        }
        playlists.insert(key, (Instant::now(), output.clone()));
        Ok(output)
    }
    async fn cached_range(&self, url: Url, start: u64, end: u64) -> Result<Bytes, String> {
        let key = format!("{}:{start}:{end}", url.as_str());
        if let Some((at, data)) = self.cache.lock().await.get(&key) {
            if at.elapsed() < Duration::from_secs(15) {
                return Ok(data.clone());
            }
        }
        // One reservation per chunk: parallel range reads interleave instead
        // of queueing behind whichever viewer arrived first.
        let _permit = self
            .fetch
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "Media expired")?;
        let response = self
            .request(url, Some(&format!("bytes={start}-{end}")))
            .await?;
        if response.status() != StatusCode::PARTIAL_CONTENT
            || content_range(response.headers())
                != Some((start, end, self.size.ok_or("Missing source length")?))
        {
            return Err("Invalid source range response".into());
        }
        let bytes = bounded(response, (end - start + 1) as usize).await?;
        if bytes.len() as u64 != end - start + 1 {
            return Err("Incomplete source range".into());
        }
        self.cache_insert(key, bytes.clone()).await;
        Ok(bytes)
    }
    // One range read with bounded retries: transient upstream errors or
    // stalls must not fail an in-flight body.
    pub(super) async fn retrying_range(&self, start: u64, end: u64) -> Result<Bytes, String> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.cached_range(self.root.clone(), start, end).await {
                Ok(bytes) => return Ok(bytes),
                Err(error) => {
                    if attempt >= CHUNK_ATTEMPTS || self.closed.load(Ordering::Acquire) {
                        return Err(error);
                    }
                    let backoff = CHUNK_BACKOFF
                        .saturating_mul(1u32 << (attempt - 1).min(3) as u32)
                        .min(CHUNK_BACKOFF_MAX);
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }

    pub(super) async fn cache_insert(&self, key: String, bytes: Bytes) {
        let mut cache = self.cache.lock().await;
        cache.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(15));
        if cache.values().map(|(_, b)| b.len()).sum::<usize>() + bytes.len() > CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key, (Instant::now(), bytes));
    }
}
