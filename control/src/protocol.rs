use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt};

pub const API_VERSION: u32 = 1;
pub const VIEWER_LEASE_SECONDS: u64 = 60;
pub const RENEW_AFTER_SECONDS: u64 = 20;
pub const IDLE_GRACE_SECONDS: u64 = 15;
pub const STARTUP_DEADLINE_SECONDS: u64 = 30;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub live: bool,
}
impl fmt::Debug for Input {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Input")
            .field("live", &self.live)
            .field("source", &"<redacted>")
            .finish()
    }
}
impl Input {
    /// Structural validation only. DNS, redirects and child resources must also
    /// pass the media network policy before any engine is allowed to fetch them.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.url.len() > 8192 || self.url.chars().any(char::is_control) {
            return Err("invalid_input");
        }
        let url = url::Url::parse(&self.url).map_err(|_| "invalid_input")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err("invalid_input");
        }
        if self.headers.len() > 32
            || self
                .headers
                .keys()
                .map(|key| key.to_ascii_lowercase())
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.headers.len()
            || self.headers.iter().any(|(key, value)| {
                key.is_empty()
                    || key.len() > 128
                    || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    || value.len() > 4096
                    || value.chars().any(char::is_control)
                    || matches!(
                        key.to_ascii_lowercase().as_str(),
                        "host"
                            | "connection"
                            | "content-length"
                            | "transfer-encoding"
                            | "proxy-authorization"
                            | "x-playback-egress-proxy"
                    )
            })
        {
            return Err("invalid_headers");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputProtocol {
    Hls,
    Progressive,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Output {
    pub protocol: OutputProtocol,
    pub video_codecs: Vec<String>,
    pub audio_codecs: Vec<String>,
    pub max_width: u32,
    pub max_height: u32,
    pub audio_track: Option<u32>,
    pub subtitle_track: Option<u32>,
}
impl Output {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(2..=16384).contains(&self.max_width)
            || !(2..=16384).contains(&self.max_height)
            || self.video_codecs.is_empty()
            || self.audio_codecs.is_empty()
            || self.video_codecs.len() > 16
            || self.audio_codecs.len() > 16
            || self.video_codecs.iter().chain(&self.audio_codecs).any(|c| {
                c.is_empty()
                    || c.len() > 32
                    || !c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            })
        {
            return Err("invalid_output");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSession {
    pub namespace: String,
    pub input: Input,
    pub output: Output,
    #[serde(default)]
    pub position_seconds: f64,
}
impl CreateSession {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_namespace(&self.namespace)?;
        self.input.validate()?;
        self.output.validate()?;
        if !self.position_seconds.is_finite()
            || !(0.0..=604800.0).contains(&self.position_seconds)
            || (self.input.live && self.position_seconds != 0.0)
        {
            return Err("invalid_position");
        }
        Ok(())
    }
}
pub fn validate_namespace(value: &str) -> Result<(), &'static str> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':'))
    {
        Err("invalid_namespace")
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Starting,
    Ready,
    Failed,
    Released,
    Expired,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sensitive_inputs_never_appear_in_debug() {
        let input = Input {
            url: "https://provider.invalid/password/movie".into(),
            headers: BTreeMap::from([("Authorization".into(), "Bearer private".into())]),
            live: false,
        };
        assert!(input.validate().is_ok());
        let display = format!("{input:?}");
        assert!(
            !display.contains("provider.invalid")
                && !display.contains("private")
                && !display.contains("password")
        );
    }
    #[test]
    fn rejects_non_network_sources_credentials_and_reserved_transport_headers() {
        for url in [
            "file:///etc/passwd",
            "https://user:password@provider.invalid/movie",
            "https://provider.invalid/video#fragment",
        ] {
            assert!(Input {
                url: url.into(),
                headers: BTreeMap::new(),
                live: false
            }
            .validate()
            .is_err());
        }
        assert!(Input {
            url: "https://provider.invalid/video".into(),
            headers: BTreeMap::from([("Host".into(), "private".into())]),
            live: false
        }
        .validate()
        .is_err());
    }
}
