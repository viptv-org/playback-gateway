use super::*;

/// Formats a WebCodecs client can demux itself, mapped to their file extension.
pub(super) fn direct_file_extension(format: &str) -> Option<&'static str> {
    // Exactly the WebCodecs demuxer's (mediabunny's) format set: ISOBMFF/QTFF,
    // Matroska/WebM, MPEG-TS, Ogg, ADTS, FLAC, WAVE and MP3. FLV, AVI, ASF and
    // plain MPEG are deliberately absent: this client cannot demux them, so
    // serving those files wholesale fails late with a confusing
    // unsupported-format error after several fallback restarts. A missing entry
    // is not a decode failure, only a lost direct path to managed delivery.
    for (name, extension) in [
        ("mp4", "mp4"),
        ("mov", "mov"),
        ("m4v", "m4v"),
        ("matroska", "mkv"),
        ("webm", "webm"),
        ("mpegts", "ts"),
        ("ogg", "ogg"),
        ("adts", "aac"),
        ("flac", "flac"),
        ("wav", "wav"),
        ("mp3", "mp3"),
    ] {
        if format.split(',').any(|f| f == name) {
            return Some(extension);
        }
    }
    None
}

/// ffprobe's codec name against the codec identifiers a WebCodecs client reports.
fn declared_codec(declared: &[String], codec: Option<&str>) -> bool {
    let Some(codec) = codec else {
        return false;
    };
    declared.iter().any(|name| {
        let normalized = name.trim().to_ascii_lowercase();
        let normalized = normalized.as_str();
        match normalized {
            "avc" | "h264" => codec == "h264",
            "hevc" | "h265" => codec == "hevc",
            "aac" | "mp4a" => codec == "aac",
            "dts" => codec == "dts",
            other => codec == other || codec.starts_with(other),
        }
    })
}

/// The original file may be served when the client reads the container and every
/// codec in it, and the picture is one this path can hand over unmodified.
/// The original-container path for a WebCodecs client. This client demuxes the
/// file itself and its decoders handle HDR and wide-gamut sources, so unlike the
/// native envelope it is not restricted by transfer characteristics or primaries.
/// Interlacing is still excluded because the demuxer feeds a progressive decoder.
fn direct_file_format(
    probe: &Probe,
    caps: &Capabilities,
    audio: Option<&ProbeStream>,
) -> Option<&'static str> {
    if caps.direct_files != Some(true)
        || probe.streams.iter().filter(|s| s.is_video()).count() != 1
        || probe.interlaced()
    {
        return None;
    }
    let video = probe.video().ok()?;
    let extension = direct_file_extension(probe.format.get("format_name")?.as_str()?)?;
    let width = caps.max_width.min(3840);
    let height = caps.max_height.min(2160);
    let fits =
        |value: Option<u32>, limit: u32| value.is_some_and(|v| v >= 2 && v <= limit && v % 2 == 0);
    if !fits(video.width, width) || !fits(video.height, height) {
        return None;
    }
    let video_codecs = caps.direct_video_codecs.as_deref().unwrap_or_default();
    if !declared_codec(video_codecs, video.codec_name.as_deref()) {
        return None;
    }
    let audio_codecs = caps.direct_audio_codecs.as_deref().unwrap_or_default();
    if audio.is_some_and(|stream| !declared_codec(audio_codecs, stream.codec_name.as_deref())) {
        return None;
    }
    Some(extension)
}

/// The upstream headers a direct-url client must present to fetch the source
/// itself: the provider's cookie and user agent, if the server holds any.
pub(super) fn source_authorization(
    headers: &HashMap<String, String>,
) -> Option<PlaybackAuthorization> {
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    };
    // Every upstream header the engine may need (Referer and friends) rides
    // along; cookie and user agent keep their dedicated fields.
    let rest: HashMap<String, String> = headers
        .iter()
        .filter(|(key, _)| {
            !key.eq_ignore_ascii_case("Cookie") && !key.eq_ignore_ascii_case("User-Agent")
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let authorization = PlaybackAuthorization {
        cookie: header("Cookie"),
        user_agent: header("User-Agent"),
        headers: (!rest.is_empty()).then_some(rest),
    };
    (authorization.cookie.is_some()
        || authorization.user_agent.is_some()
        || authorization.headers.is_some())
    .then_some(authorization)
}

pub(super) fn direct_format(
    probe: &Probe,
    caps: &Capabilities,
    audio: Option<&ProbeStream>,
    selection: &TrackSelection,
) -> Option<&'static str> {
    // Chosen subtitle tracks are delivered by a managed session, never by handing
    // the untouched file to the client.
    if selection.subtitle_track_index.is_some() || selection.preferred_subtitle_language.is_some() {
        return None;
    }
    // A WebCodecs client reads the original container itself, so it is not bound
    // by the native envelope's single-audio-stream or AAC-only rules.
    if let Some(extension) = direct_file_format(probe, caps, audio) {
        return Some(extension);
    }
    if probe
        .streams
        .iter()
        .filter(|s| s.codec_type.as_deref() == Some("audio"))
        .count()
        > 1
        || probe.streams.iter().filter(|s| s.is_video()).count() != 1
        || !probe.compatible_audio_stream(audio)
    {
        return None;
    }
    if let (Some(audio), Some(language)) = (audio, selection.preferred_audio_language.as_deref()) {
        // A known different language still remains manually playable, but do not
        // advertise a preferred-language native match without track evidence.
        if audio.language().is_some_and(|actual| {
            normalize_audio_language(&actual) != normalize_audio_language(language)
        }) {
            return None;
        }
    }
    let video = probe.video().ok()?;
    let width = caps.max_width.min(3840);
    let height = caps.max_height.min(2160);
    let h264 = probe.compatible_video(width.min(1920), height.min(1080), H264_COPY_LEVEL);
    let hevc = caps.hevc
        && caps.hevc_sdr
        && video.codec_name.as_deref() == Some("hevc")
        && video.profile.as_deref() == Some("Main")
        && video.pix_fmt.as_deref() == Some("yuv420p")
        && video.level.is_some_and(|l| l > 0 && l <= 150)
        && video
            .width
            .is_some_and(|w| w >= 2 && w <= width && w % 2 == 0)
        && video
            .height
            .is_some_and(|h| h >= 2 && h <= height && h % 2 == 0)
        && conservative_frame_rate(video.avg_frame_rate.as_deref())
        && conservative_frame_rate(video.r_frame_rate.as_deref())
        && !probe.interlaced()
        && matches!(probe.hdr_transfer(), Ok(None));
    if !h264 && !hevc {
        return None;
    }
    let format = probe.format.get("format_name")?.as_str()?;
    if format.split(',').any(|f| f == "hls") {
        caps.direct_hls.unwrap_or(true).then_some("hls")
    } else if format.split(',').any(|f| f == "mp4" || f == "mov") {
        caps.direct_mp4.unwrap_or(true).then_some("mp4")
    } else {
        None
    }
}
