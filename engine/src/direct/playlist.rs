use super::*;

pub(super) fn public_host(url: &Url) -> bool {
    let host = url.host_str().unwrap_or("").trim_matches(['[', ']']);
    if host.eq_ignore_ascii_case("localhost")
        || host.ends_with(".localhost")
        || host.ends_with(".local")
    {
        return false;
    }
    host.parse::<std::net::IpAddr>()
        .map(public_ip)
        .unwrap_or(true)
}
pub(super) fn public_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let octets = ip.octets();
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_unspecified()
                && !ip.is_multicast()
                && !ip.is_broadcast()
                && octets[0] != 0
                && octets[0] < 224
                && !(octets[0] == 100 && (64..=127).contains(&octets[1]))
        }
        std::net::IpAddr::V6(ip) => {
            !ip.is_loopback()
                && !ip.is_unspecified()
                && !ip.is_unique_local()
                && !ip.is_unicast_link_local()
                && !ip.is_multicast()
                && ip.to_ipv4_mapped().is_none()
        }
    }
}

pub(super) async fn bounded(response: reqwest::Response, limit: usize) -> Result<Bytes, String> {
    let mut output = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "Media read failed")?;
        if output.len() + chunk.len() > limit {
            return Err("Media resource exceeds limit".into());
        }
        output.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(output))
}
pub(super) fn content_range(headers: &HeaderMap) -> Option<(u64, u64, u64)> {
    let raw = headers
        .get(header::CONTENT_RANGE)?
        .to_str()
        .ok()?
        .strip_prefix("bytes ")?;
    let (range, total) = raw.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?, total.parse().ok()?))
}
pub(super) fn range_bounds(raw: Option<&str>, size: u64) -> Option<(u64, u64)> {
    if size == 0 {
        return None;
    }
    let Some(raw) = raw else {
        return Some((0, size - 1));
    };
    let (start, end) = raw.strip_prefix("bytes=")?.split_once('-')?;
    if start.is_empty() {
        let count = end.parse::<u64>().ok()?;
        return (count > 0).then_some((size.saturating_sub(count), size - 1));
    }
    let start = start.parse::<u64>().ok()?;
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().ok()?.min(size - 1)
    };
    (start < size && start <= end).then_some((start, end))
}
fn resource(
    base: &Url,
    raw: &str,
    playlist: bool,
    map: &mut HashMap<String, Url>,
) -> Result<String, String> {
    if raw.contains("{$") {
        return Err("Unsupported HLS variables".into());
    }
    let url = base.join(raw).map_err(|_| "Invalid HLS resource")?;
    crate::validate_url(url.as_str())?;
    let hash = Sha256::digest(url.as_str().as_bytes());
    // Some native demuxers validate segment suffixes before inspecting bytes.
    // Preserve only a small safe extension, never the upstream filename/query.
    let suffix = url
        .path()
        .rsplit('.')
        .next()
        .filter(|s| ["ts", "m4s", "mp4", "aac", "vtt", "key"].contains(s))
        .unwrap_or("ts");
    let key = format!("d-{:x}.{}", hash, if playlist { "m3u8" } else { suffix });
    map.insert(key.clone(), url);
    Ok(key)
}
// URI-bearing tags the proxy can follow. Rendition playlists recurse through
// serve() as .m3u8 keys; every other kind is a bounded immutable resource.
fn uri_kind(line: &str) -> Option<bool> {
    [
        ("#EXT-X-MEDIA:", true),
        ("#EXT-X-I-FRAME-STREAM-INF:", true),
        ("#EXT-X-RENDITION-REPORT:", true),
        ("#EXT-X-KEY:", false),
        ("#EXT-X-MAP:", false),
        ("#EXT-X-SESSION-KEY:", false),
        ("#EXT-X-SESSION-DATA:", false),
        ("#EXT-X-PART:", false),
        ("#EXT-X-PRELOAD-HINT:", false),
    ]
    .iter()
    .find_map(|(tag, kind)| line.starts_with(tag).then_some(*kind))
}
pub(super) fn rewrite(text: &str, base: &Url) -> Result<(String, HashMap<String, Url>), String> {
    if !text.trim_start().starts_with("#EXTM3U") {
        return Err("Invalid HLS playlist".into());
    }
    let mut out = String::new();
    let mut map = HashMap::new();
    // Set between a variant descriptor and the playlist URI line after it.
    let mut variant = false;
    for line in text.lines() {
        let line = line.trim();
        // Variable substitution would bypass the opaque key mapping.
        if line.starts_with("#EXT-X-DEFINE:") {
            return Err("Unsupported HLS variables".into());
        }
        if (line.starts_with("#EXT-X-KEY:") || line.starts_with("#EXT-X-SESSION-KEY:"))
            && !line.contains("METHOD=AES-128,")
            && !line.contains("METHOD=NONE")
        {
            return Err("Unsupported HLS encryption".into());
        }
        if !line.is_empty() && !line.starts_with('#') {
            // The URI after a variant descriptor is itself a playlist, so
            // serve() rewrites it recursively through its .m3u8 key.
            out.push_str(&resource(
                base,
                line,
                std::mem::take(&mut variant),
                &mut map,
            )?);
        } else if line.starts_with("#EXT-X-STREAM-INF") {
            // Variant descriptors carry no URI attribute; the next bare line
            // does, so one claiming otherwise must not pass through raw.
            if line.contains("URI=") {
                return Err("Unsupported HLS reference".into());
            }
            variant = true;
            out.push_str(line);
        } else if let Some(at) = line.find("URI=\"") {
            // Unknown URI-bearing tags (content steering, private extensions)
            // stay on managed playback instead of leaking an origin URL.
            let playlist = uri_kind(line).ok_or("Unsupported HLS reference")?;
            let start = at + 5;
            let end = start + line[start..].find('"').ok_or("Invalid HLS URI")?;
            out.push_str(&line[..start]);
            out.push_str(&resource(base, &line[start..end], playlist, &mut map)?);
            out.push_str(&line[end..]);
        } else {
            if line.contains("URI=") {
                return Err("Unsupported HLS reference".into());
            }
            out.push_str(line);
        }
        out.push('\n');
    }
    if map.len() > RESOURCE_LIMIT {
        return Err("Too many HLS resources".into());
    }
    Ok((out, map))
}
