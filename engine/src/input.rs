//! Engine-neutral input descriptors and structural magnet validation.
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, path::PathBuf, time::Duration};

pub const MIN_CACHE_BYTES: u64 = 256 * 1024 * 1024;

/// Input configuration shared with the development comparison facade.
#[derive(Clone)]
pub struct TorrentConfig {
    pub enabled: bool,
    pub cache_dir: PathBuf,
    pub max_bytes: u64,
    /// Legacy comparison engine's fixed disk-slot reservation.
    pub piece_cache_bytes: Option<u64>,
    pub listen_port: Option<u16>,
    /// Legacy comparison discovery settings. The Go worker owns v2 discovery.
    pub enable_dht: bool,
    /// Operator-supplied bootstrap peers (not source URI fields).
    pub initial_peers: Vec<SocketAddr>,
    pub idle_timeout: Duration,
}
impl Default for TorrentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cache_dir: PathBuf::from("data/torrent-cache"),
            max_bytes: 2 * 1024 * 1024 * 1024,
            piece_cache_bytes: None,
            listen_port: None,
            enable_dht: true,
            initial_peers: Vec::new(),
            idle_timeout: Duration::from_secs(60),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TorrentFile {
    pub index: u32,
    pub name: String,
    pub size: u64,
}
impl std::fmt::Debug for TorrentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TorrentConfig(<redacted>)")
    }
}

pub fn validate_magnet(source: &str) -> Result<(), String> {
    if source.len() > 16 * 1024 {
        return Err("Invalid magnet URI".into());
    }
    let url = url::Url::parse(source).map_err(|_| "Invalid magnet URI")?;
    if url.scheme() != "magnet"
        || !url.path().is_empty()
        || url.fragment().is_some()
        || url.host_str().is_some()
    {
        return Err("Invalid magnet URI".into());
    }
    let mut hash = false;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "xt" => {
                if hash || !value.starts_with("urn:btih:") {
                    return Err("Unsupported magnet hash".into());
                }
                let h = &value[9..];
                if !(h.len() == 40 && h.chars().all(|c| c.is_ascii_hexdigit())
                    || h.len() == 32
                        && h.chars()
                            .all(|c| c.is_ascii_alphabetic() || ('2'..='7').contains(&c)))
                {
                    return Err("Unsupported magnet hash".into());
                }
                hash = true;
            }
            "tr" => {
                let u = url::Url::parse(&value).map_err(|_| "Invalid magnet tracker")?;
                if !matches!(u.scheme(), "http" | "https" | "udp")
                    || u.host_str().is_none()
                    || !u.username().is_empty()
                    || u.password().is_some()
                    || u.fragment().is_some()
                {
                    return Err("Invalid magnet tracker".into());
                }
            }
            "dn" => {}
            _ => return Err("Unsupported magnet parameter".into()),
        }
    }
    if !hash {
        return Err("Unsupported magnet hash".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magnet_validation_needs_no_transport_engine() {
        let hex = "magnet:?xt=urn:btih:0123456789012345678901234567890123456789";
        assert!(validate_magnet(hex).is_ok());
        assert!(validate_magnet("magnet:?xt=urn:btih:ABCDEFGHIJKLMNOPQRSTUVWXYZ234567").is_ok());
        assert!(validate_magnet(&format!(
            "{hex}&tr=udp%3A%2F%2Ftracker.example%3A80%2Fannounce"
        ))
        .is_ok());
        for suffix in [
            "&xt=urn:btih:0123456789012345678901234567890123456789",
            "&xs=https://example.com/a",
            "&x.pe=example.com:123",
            "&tr=http://user:pass@example.com/a",
            "#fragment",
        ] {
            assert!(validate_magnet(&format!("{hex}{suffix}")).is_err());
        }
        assert!(validate_magnet("magnet:?xt=urn:btih:ABCDEFGHIJKLMNOPQRSTUVWXYZ234568").is_err());
    }
}
