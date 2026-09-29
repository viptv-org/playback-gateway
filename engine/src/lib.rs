//! Managed, capability-addressed HLS sessions. No upstream URL is returned to clients.
//!
//! The engine is identity-free: it accepts a source URL, bounded headers, a
//! position and client capabilities, and owns input processes and connection
//! reservations throughout preparation. Callers own authentication, identity
//! and policy — the audit that motivated this crate confirmed zero database
//! and no account dependencies beyond the three seams below.
mod compat;
mod direct;
mod direct_gate;
mod fmp4;
mod hardware;
mod hls;
mod lease;
mod lifecycle;
mod manager;
mod probe;
mod start;
mod prepared;
mod subtitles;
#[cfg(test)]
mod tests;
mod types;

pub use manager::{PlaybackManager, SampleLimits};
pub use prepared::{OutputPlan, PreparedPlayback};
pub use types::{
    Capabilities, Config, MediaTrack, PlaybackResponse, SelectedAudio,
    SelectedSubtitle, TrackDisposition, TrackSelection,
};

use compat::{
    conservative_frame_rate, normalize_audio_language, Probe, ProbeStream, H264_COPY_LEVEL,
};
#[cfg(test)]
use direct_gate::direct_file_extension;
use direct_gate::direct_format;
use hls::{
    cache_safe, dimensions, hdr_filter, media_type, playback_ready, scale_filter,
    stable_hls_target_duration, Throttle, HLS_DELETE_GRACE_SECONDS, HLS_INITIAL_SEGMENT_SECONDS,
    HLS_SEGMENT_SECONDS, HLS_THROTTLE_TICK, HLS_WINDOW_SECONDS,
};
#[cfg(test)]
use hls::{
    hdr_size, newest_segment, segment_number, HLS_THROTTLE_AHEAD_SEGMENTS,
    HLS_THROTTLE_RESUME_SEGMENTS,
};
use lease::{cleanup_orphans, constant_time_eq, header_block, input_args, InputPermits, Session};
#[cfg(test)]
use probe::{probe_failure, ProbeFailure, LIVE_PROBE_CACHE_TTL, PROBE_CACHE_TTL};
use probe::{probe_output, ProbeCacheEntry, ProbeChild};
use types::{CleanupTasks, ProbeDisposition};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    sync::{Mutex, OnceCell, OwnedSemaphorePermit, RwLock, Semaphore},
    time::{sleep, timeout},
};
use url::Url;
use uuid::Uuid;

/// Session-capacity message that controls wire behavior: the server maps it to
/// a 503 so clients do not retry a full ladder. Defined here because the
/// engine's error type is a plain string.
pub const MSG_PLAYBACK_CAPACITY: &str = "Playback capacity reached";
/// Upstream egress proxy routing header. The provider layer sets it on
/// upstream requests; the engine consumes it locally (converting it to a
/// proxy route) and never forwards it upstream.
pub const EGRESS_PROXY_HEADER: &str = "x-playback-egress-proxy";

/// Route a client through the configured egress proxy (for example a WARP
/// route) with all other proxying disabled.
pub(crate) fn egress_proxy_builder(
    builder: reqwest::ClientBuilder,
    proxy: Option<&str>,
) -> Result<reqwest::ClientBuilder, String> {
    match proxy {
        Some(url) => Ok(builder
            .no_proxy()
            .proxy(reqwest::Proxy::all(url).map_err(|_| "Invalid WARP route")?)),
        None => Ok(builder.no_proxy()),
    }
}

/// Only HTTP(S) URLs without userinfo or fragments are accepted as inputs.
pub(crate) fn validate_url(raw: &str) -> Result<Url, String> {
    let u = Url::parse(raw).map_err(|_| "Invalid HTTP URL".to_string())?;
    if !matches!(u.scheme(), "http" | "https")
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.fragment().is_some()
    {
        return Err("Only HTTP(S) URLs without userinfo or fragments are supported".into());
    }
    Ok(u)
}

/// Display text for a source with its URL and credential-bearing header
/// values redacted; never log the raw values this protects.
pub fn source_display_text(
    text: &str,
    limit: usize,
    url: &str,
    headers: &HashMap<String, String>,
) -> String {
    let mut text = text.replace(url, "[link omitted]");
    // Ordinary negotiation/client-identification values are display text too:
    // e.g. Accept-Language: en must not erase en/eng or letters inside French.
    // Origin/Referer links are handled by the URL redaction below.
    for (_, value) in headers.iter().filter(|(key, value)| {
        (key.eq_ignore_ascii_case("authorization") || key.eq_ignore_ascii_case("x-csrf-token"))
            && !value.is_empty()
    }) {
        text = text.replace(value, "[private value omitted]");
        if let Some((scheme, credential)) = value.split_once(' ') {
            if (scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("basic"))
                && !credential.is_empty()
            {
                text = text.replace(credential, "[private value omitted]");
            }
        }
    }
    let text = text
        .lines()
        .map(|line| {
            line.split_whitespace()
                .map(|word| {
                    if word.contains("://") || word.to_ascii_lowercase().contains("magnet:") {
                        "[link omitted]".to_owned()
                    } else {
                        word.chars().filter(|c| !c.is_control()).collect::<String>()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
