use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// Dedicated media directory: never share with another running server/manager.
    pub root: PathBuf,
    pub max_sessions: usize,
    pub ttl: Duration,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    pub browser: Option<BrowserCapabilities>,
    pub max_width: u32,
    pub max_height: u32,
    pub h264: bool,
    pub hevc: bool,
    pub aac: bool,
    /// Opt-in only: old clients retain managed HLS.
    pub direct_play: bool,
    /// Omitted by legacy clients; runtime probes may disable either transport.
    pub direct_mp4: Option<bool>,
    pub direct_hls: Option<bool>,
    /// A WebCodecs client reads original containers itself. When set, the
    /// original file may be served as a byte-range resource for any container
    /// this client can demux, with the codecs it declares below.
    pub direct_files: Option<bool>,
    pub direct_video_codecs: Option<Vec<String>>,
    pub direct_audio_codecs: Option<Vec<String>>,
    /// A native client (the Tauri engine) fetches sources itself. When set, the
    /// original URL is delivered with the server's upstream authorization
    /// instead of a proxy: no transcoding and no server-side media path.
    pub direct_urls: Option<bool>,
    pub hevc_sdr: bool,
}
impl Default for Capabilities {
    fn default() -> Self {
        Self {
            browser: None,
            max_width: 1280,
            max_height: 720,
            h264: true,
            hevc: false,
            aac: true,
            direct_play: false,
            direct_mp4: None,
            direct_hls: None,
            direct_files: None,
            direct_video_codecs: None,
            direct_audio_codecs: None,
            direct_urls: None,
            hevc_sdr: false,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserCapabilities {
    pub version: u8,
    pub inspect_original: bool,
    pub local_remux: bool,
    pub fmp4: bool,
    pub engines: Vec<EngineEvidence>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EngineEvidence {
    pub engine: String,
    pub codec: String,
    pub evidence: String,
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    pub max_frame_rate: Option<u32>,
    pub bit_depth: Option<u8>,
    pub hdr: Option<bool>,
    pub profile: Option<String>,
    pub max_level: Option<u32>,
    pub containers: Option<Vec<String>>,
    pub max_channels: Option<u8>,
    pub sample_rates: Option<Vec<u32>>,
}
impl BrowserCapabilities {
    pub(super) fn valid(&self) -> bool {
        self.version == 1
            && self.engines.len() <= 32
            && self.engines.iter().all(|entry| {
                matches!(entry.engine.as_str(), "native" | "mse" | "webcodecs")
                    && matches!(
                        entry.evidence.as_str(),
                        "unknown" | "advertised" | "decoded"
                    )
                    && entry.codec.len() <= 64
                    && entry.codec.is_ascii()
                    && entry.max_width.is_none_or(|v| v <= 8192)
                    && entry.max_height.is_none_or(|v| v <= 8192)
                    && entry.max_frame_rate.is_none_or(|v| v <= 240)
                    && entry
                        .profile
                        .as_ref()
                        .is_none_or(|v| v.len() <= 64 && v.is_ascii())
                    && entry.max_level.is_none_or(|v| v <= 255)
                    && entry.containers.as_ref().is_none_or(|v| {
                        v.len() <= 16 && v.iter().all(|s| s.len() <= 32 && s.is_ascii())
                    })
                    && entry.max_channels.is_none_or(|v| v <= 32)
                    && entry
                        .sample_rates
                        .as_ref()
                        .is_none_or(|v| v.len() <= 8 && v.iter().all(|rate| *rate <= 384000))
            })
    }
}
/// Absolute INPUT stream indices from ffprobe, never native/output audio indices.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackSelection {
    /// Scoped browser failure; absent retains legacy forced-conversion behavior.
    pub conversion_reason: Option<String>,
    pub audio_track_index: Option<u32>,
    pub audio_language: Option<String>,
    pub subtitle_track_index: Option<u32>,
    pub preferred_audio_language: Option<String>,
    pub preferred_subtitle_language: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct TrackDisposition {
    pub default: bool,
    pub commentary: bool,
    pub hearing_impaired: bool,
    pub visual_impaired: bool,
    pub forced: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct ProbeDisposition {
    pub(super) default: u8,
    pub(super) comment: u8,
    pub(super) hearing_impaired: u8,
    pub(super) visual_impaired: u8,
    pub(super) forced: u8,
    /// Embedded cover art: ffprobe lists it as a video stream, but it is a
    /// still picture, never the programme's video track.
    pub(super) attached_pic: u8,
}
impl ProbeDisposition {
    pub(super) fn public(&self) -> TrackDisposition {
        TrackDisposition {
            default: self.default == 1,
            commentary: self.comment == 1,
            hearing_impaired: self.hearing_impaired == 1,
            visual_impaired: self.visual_impaired == 1,
            forced: self.forced == 1,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MediaTrack {
    pub input_index: u32,
    pub codec: Option<String>,
    pub language: Option<String>,
    pub language_status: String,
    pub title: String,
    /// None means these disposition flags were not provided by the probe.
    pub disposition: Option<TrackDisposition>,
    pub selected: bool,
    pub supported: bool,
    pub selectable: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SelectedAudio {
    pub input_index: u32,
    /// Compatibility alias for output_audio_ordinal; never a native Roku track ID.
    pub output_index: u32,
    /// Ordinal within output AUDIO tracks; native IDs come from the player itself.
    pub output_audio_ordinal: u32,
    pub output_stream_index: u32,
    pub language: Option<String>,
    pub language_status: String,
    pub title: String,
    /// None means these disposition flags were not provided by the probe.
    pub disposition: Option<TrackDisposition>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SelectedSubtitle {
    pub input_index: u32,
    /// Subtitle ordinal, never a native player track ID.
    pub output_index: u32,
    pub output_stream_index: u32,
    pub language: Option<String>,
    pub language_status: String,
    pub title: String,
    /// None means these disposition flags were not provided by the probe.
    pub disposition: Option<TrackDisposition>,
}

/// Upstream authorization for a delivered original URL. The client applies it
/// to its own engine's requests; browser deliveries never carry one.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlaybackAuthorization {
    pub cookie: Option<String>,
    pub user_agent: Option<String>,
    /// Every other upstream header the client's engine may need to fetch the
    /// source itself (Referer and friends); cookie and user agent are excluded
    /// because they ride in the dedicated fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlaybackResponse {
    pub id: String,
    pub url: String,
    pub format: String,
    pub mode: String,
    /// Whether the source video packets are copied or encoded.
    pub video_mode: String,
    /// Whether selected source audio is copied, encoded, or absent.
    pub audio_mode: String,
    pub position: f64,
    /// True only for explicitly indexed/discovered live sources.
    pub live: bool,
    /// Full source duration in seconds; zero means unknown or live.
    pub duration: f64,
    pub audio_tracks: Vec<MediaTrack>,
    pub subtitle_tracks: Vec<MediaTrack>,
    pub selected_audio: Option<SelectedAudio>,
    pub subtitles_supported: bool,
    pub selected_subtitle: Option<SelectedSubtitle>,
    /// Present only when the original URL is delivered for a client that
    /// fetches it itself: the headers that client must present upstream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization: Option<PlaybackAuthorization>,
}

pub(super) type CleanupTasks = Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>;
