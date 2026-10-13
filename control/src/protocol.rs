use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt};

pub const API_VERSION: u32 = 1;
pub const VIEWER_LEASE_SECONDS: u64 = 60;
pub const RENEW_AFTER_SECONDS: u64 = 20;
pub const IDLE_GRACE_SECONDS: u64 = 15;
pub const STARTUP_DEADLINE_SECONDS: u64 = 120;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub live: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_file_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trackers: Vec<String>,
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
    pub fn torrent(&self) -> bool {
        self.url.starts_with("magnet:") || source_extension(&self.url) == Some("torrent".into())
    }
    pub fn mediated_http(&self) -> bool {
        !self.headers.is_empty() && !self.torrent() && !self.archive()
    }
    pub fn owned_source(&self) -> bool {
        self.torrent() || self.archive() || self.mediated_http()
    }
    pub fn startup_seconds(&self) -> u64 {
        if self.torrent() {
            STARTUP_DEADLINE_SECONDS
        } else {
            30
        }
    }
    pub fn archive(&self) -> bool {
        source_extension(&self.url).is_some_and(|extension| extension == "rar")
    }
    /// Structural validation only. DNS, redirects and child resources must also
    /// pass the media network policy before any engine is allowed to fetch them.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.url.len() > 8192 || self.url.chars().any(char::is_control) {
            return Err("invalid_input");
        }
        let url = url::Url::parse(&self.url).map_err(|_| "invalid_input")?;
        if url.scheme() == "magnet" {
            playback_engine::input::validate_magnet(&self.url).map_err(|_| "invalid_input")?;
            if !self.headers.is_empty() {
                return Err("invalid_headers");
            }
        } else if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err("invalid_input");
        }
        if self.file_index.is_some_and(|index| index > 65535)
            || (self.file_index.is_some() && !self.torrent())
        {
            return Err("invalid_input");
        }
        if self.archive_index.is_some_and(|index| index > 65535)
            || self.expected_file_size.is_some_and(|size| {
                size == 0 || size > 9007199254740991 || self.file_index.is_none()
            })
            || self.trackers.len() > 32
            || (!self.torrent()
                && (self.archive_index.is_some()
                    || self.expected_file_size.is_some()
                    || !self.trackers.is_empty()))
            || self.trackers.iter().any(|tracker| {
                tracker.len() > 2048
                    || tracker.chars().any(char::is_control)
                    || url::Url::parse(tracker).map_or(true, |url| {
                        !matches!(url.scheme(), "http" | "https" | "udp")
                            || url.host_str().is_none()
                            || !url.username().is_empty()
                            || url.password().is_some()
                            || url.fragment().is_some()
                    })
            })
        {
            return Err("invalid_input");
        }
        if (self.torrent() || self.archive()) && self.live {
            return Err("unsupported_output");
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

fn source_extension(source: &str) -> Option<String> {
    let url = url::Url::parse(source).ok()?;
    let path = url.path();
    Some(path.rsplit_once('.')?.1.to_ascii_lowercase())
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputProtocol {
    Hls,
    Progressive,
    /// Prefer verified original H264/AAC MP4; otherwise prepare ordinary HLS.
    NativeMp4,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Conversion {
    #[default]
    Auto,
    Audio,
    Video,
    AudioVideo,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Output {
    #[serde(default)]
    pub conversion: Conversion,
    pub protocol: OutputProtocol,
    pub video_codecs: Vec<String>,
    pub audio_codecs: Vec<String>,
    pub max_width: u32,
    pub max_height: u32,
    pub audio_track: Option<u32>,
    pub subtitle_track: Option<u32>,
    pub audio_language: Option<String>,
    pub preferred_audio_language: Option<String>,
    pub preferred_subtitle_language: Option<String>,
    #[serde(default)]
    pub subtitles_off: bool,
}
impl Output {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(2..=16384).contains(&self.max_width)
            || self.audio_track.is_some_and(|index| index > 65535)
            || self.subtitle_track.is_some_and(|index| index > 65535)
            || (self.subtitles_off
                && (self.subtitle_track.is_some() || self.preferred_subtitle_language.is_some()))
            || [
                &self.audio_language,
                &self.preferred_audio_language,
                &self.preferred_subtitle_language,
            ]
            .into_iter()
            .flatten()
            .any(|language| {
                language.is_empty()
                    || language.len() > 35
                    || !language
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
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
    #[serde(skip)]
    pub client_frame_ack: bool,
    pub namespace: String,
    pub input: Input,
    pub output: Output,
    #[serde(default)]
    pub position_seconds: f64,
}
impl CreateSession {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.client_frame_ack && !self.input.torrent() {
            return Err("invalid_input");
        }
        validate_namespace(&self.namespace)?;
        self.input.validate()?;
        self.output.validate()?;
        if self.output.protocol == OutputProtocol::NativeMp4
            && (self.input.live || self.input.torrent() || self.input.archive())
        {
            return Err("unsupported_output");
        }
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
    fn torrent_discovery_and_archive_fields_are_bounded_and_identity_preserving() {
        let base = serde_json::json!({"url":"magnet:?xt=urn:btih:1111111111111111111111111111111111111111", "live":false, "file_index":2,
            "archive_index":4, "expected_file_size":20000000000u64, "trackers":["https://tracker.example/announce"]});
        let input: Input = serde_json::from_value(base.clone()).unwrap();
        assert!(input.validate().is_ok());
        assert_eq!(
            serde_json::to_value(&input).unwrap()["trackers"],
            base["trackers"]
        );
        for (field, value) in [
            ("archive_index", serde_json::json!(65536)),
            ("expected_file_size", serde_json::json!(0)),
            ("trackers", serde_json::json!(["file:///etc/passwd"])),
            (
                "url",
                serde_json::json!("https://provider.example/movie.mp4"),
            ),
        ] {
            let mut invalid = base.clone();
            invalid[field] = value;
            assert!(serde_json::from_value::<Input>(invalid)
                .unwrap()
                .validate()
                .is_err());
        }
    }
    #[test]
    fn torrent_file_selection_is_vod_only_and_bounded() {
        let base = serde_json::json!({"url":"magnet:?xt=urn:btih:1111111111111111111111111111111111111111","live":false,"file_index":2});
        assert!(serde_json::from_value::<Input>(base.clone())
            .unwrap()
            .validate()
            .is_ok());
        for (field, value) in [
            ("live", serde_json::json!(true)),
            ("file_index", serde_json::json!(65536)),
        ] {
            let mut invalid = base.clone();
            invalid[field] = value;
            assert!(serde_json::from_value::<Input>(invalid)
                .unwrap()
                .validate()
                .is_err());
        }
        for source in ["magnet:?xt=urn:btih:bad", "magnet:?xt=urn:btih:1111111111111111111111111111111111111111&x.pe=127.0.0.1:80", "magnet:?xt=urn:btih:1111111111111111111111111111111111111111&tr=https%3A%2F%2Fuser%3Asecret%40tracker.invalid"] {
            let mut invalid = base.clone();
            invalid["url"] = serde_json::json!(source);
            assert!(serde_json::from_value::<Input>(invalid).unwrap().validate().is_err());
        }
    }
    #[test]
    fn output_conversion_and_track_policy_is_explicit_and_bounded() {
        let base = serde_json::json!({"protocol":"hls","video_codecs":["h264"],"audio_codecs":["aac"],"max_width":3840,"max_height":2160,"audio_track":null,"subtitle_track":null});
        let automatic: Output = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(automatic.conversion, Conversion::Auto);
        assert!(automatic.validate().is_ok());
        for conversion in ["auto", "audio", "video", "audio_video"] {
            let mut value = base.clone();
            value["conversion"] = serde_json::json!(conversion);
            value["preferred_audio_language"] = serde_json::json!("pt-BR");
            assert!(serde_json::from_value::<Output>(value)
                .unwrap()
                .validate()
                .is_ok());
        }
        for (field, bad) in [
            ("audio_track", serde_json::json!(65536)),
            ("subtitle_track", serde_json::json!(65536)),
            ("audio_language", serde_json::json!("en\nsecret")),
            ("preferred_audio_language", serde_json::json!("")),
        ] {
            let mut value = base.clone();
            value[field] = bad;
            assert!(serde_json::from_value::<Output>(value)
                .unwrap()
                .validate()
                .is_err());
        }
        let mut value = base;
        value["subtitles_off"] = serde_json::json!(true);
        value["subtitle_track"] = serde_json::json!(2);
        assert!(serde_json::from_value::<Output>(value.clone())
            .unwrap()
            .validate()
            .is_err());
        value["conversion"] = serde_json::json!("shell-command");
        assert!(serde_json::from_value::<Output>(value).is_err());
    }
    #[test]
    fn sensitive_inputs_never_appear_in_debug() {
        let input = Input {
            url: "https://provider.invalid/password/movie".into(),
            headers: BTreeMap::from([("Authorization".into(), "Bearer private".into())]),
            live: false,
            file_index: None,
            archive_index: None,
            expected_file_size: None,
            trackers: Vec::new(),
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
                live: false,
                file_index: None,
                archive_index: None,
                expected_file_size: None,
                trackers: Vec::new(),
            }
            .validate()
            .is_err());
        }
        assert!(Input {
            url: "https://provider.invalid/video".into(),
            headers: BTreeMap::from([("Host".into(), "private".into())]),
            live: false,
            file_index: None,
            archive_index: None,
            expected_file_size: None,
            trackers: Vec::new(),
        }
        .validate()
        .is_err());
    }
}
