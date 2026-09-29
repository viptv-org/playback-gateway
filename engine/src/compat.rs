use super::*;

#[derive(Clone, Deserialize)]
pub(super) struct Probe {
    pub(super) streams: Vec<ProbeStream>,
    #[serde(default)]
    pub(super) format: serde_json::Value,
}
#[derive(Clone, Debug, Deserialize)]
pub(super) struct ProbeStream {
    pub(super) index: Option<u32>,
    pub(super) disposition: Option<ProbeDisposition>,
    #[serde(default)]
    pub(super) tags: HashMap<String, String>,
    pub(super) codec_type: Option<String>,
    pub(super) codec_name: Option<String>,
    pub(super) width: Option<u32>,
    pub(super) height: Option<u32>,
    pub(super) pix_fmt: Option<String>,
    pub(super) sample_aspect_ratio: Option<String>,
    pub(super) profile: Option<String>,
    #[serde(default, deserialize_with = "probe_level")]
    pub(super) level: Option<u32>,
    pub(super) channels: Option<u32>,
    pub(super) avg_frame_rate: Option<String>,
    pub(super) r_frame_rate: Option<String>,
    pub(super) color_transfer: Option<String>,
    pub(super) field_order: Option<String>,
}
/// ffprobe prints `level` as a signed integer and uses -99 for "unknown", which
/// every embedded cover picture reports. Reading it as unsigned failed the whole
/// probe for any file with cover art; the reduced retry then lacked profile and
/// level, so copyable video was re-encoded after two full source inspections.
fn probe_level<'de, D: serde::Deserializer<'de>>(value: D) -> Result<Option<u32>, D::Error> {
    Ok(Option::<i64>::deserialize(value)?.and_then(|level| u32::try_from(level).ok()))
}

/// The highest H.264 level this engine passes through. Modern browser H.264
/// decoders cover level 5.1, and the envelope's dimension, frame-rate, profile,
/// pix-fmt, interlace and SDR gates bound the actual decode load, so a level
/// tag alone never forces a re-encode of otherwise compatible video.
pub(super) const H264_COPY_LEVEL: u32 = 51;

pub(super) fn conservative_frame_rate(rate: Option<&str>) -> bool {
    frame_rate_within(rate, 60)
}
pub(super) fn frame_rate_within(rate: Option<&str>, maximum: u32) -> bool {
    let Some((numerator, denominator)) = rate.and_then(|r| r.split_once('/')) else {
        return false;
    };
    if ![numerator, denominator]
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    let (Ok(numerator), Ok(denominator)) = (numerator.parse::<u64>(), denominator.parse::<u64>())
    else {
        return false;
    };
    numerator > 0
        && denominator > 0
        && u128::from(numerator) <= u128::from(denominator) * u128::from(maximum)
}

impl ProbeStream {
    /// A real video track: ffprobe also lists embedded cover art (MKV
    /// attachments, MP4 `covr`) as video, flagged as an attached picture.
    pub(super) fn is_video(&self) -> bool {
        self.codec_type.as_deref() == Some("video")
            && !self
                .disposition
                .as_ref()
                .is_some_and(|disposition| disposition.attached_pic == 1)
    }
    pub(super) fn text_subtitle(&self) -> bool {
        self.codec_type.as_deref() == Some("subtitle")
            && matches!(
                self.codec_name.as_deref(),
                Some("subrip" | "ass" | "ssa" | "webvtt" | "mov_text" | "text")
            )
    }
    pub(super) fn selectable(&self) -> bool {
        if self.codec_type.as_deref() == Some("subtitle") {
            self.text_subtitle()
        } else {
            self.codec_type.as_deref() == Some("audio")
        }
    }
    pub(super) fn language(&self) -> Option<String> {
        self.tags
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("language"))
            .map(|(_, v)| v.trim().to_ascii_lowercase())
            .filter(|v| {
                (2..=3).contains(&v.len())
                    && v.bytes().all(|b| b.is_ascii_alphabetic())
                    && v != "und"
                    && v != "zxx"
                    && v != "mul"
            })
    }
    pub(super) fn language_status(&self) -> &'static str {
        match self.language().as_deref() {
            Some("en" | "eng") => "tagged_english",
            Some(_) => "tagged_non_english",
            None => "unknown",
        }
    }
}

pub(super) fn normalize_audio_language(language: &str) -> &str {
    let language = language.split('-').next().unwrap_or(language);
    match language {
        "en" | "eng" => "eng",
        "ja" | "jpn" => "jpn",
        "es" | "spa" => "spa",
        "it" | "ita" => "ita",
        "fr" | "fra" | "fre" => "fra",
        "de" | "deu" | "ger" => "deu",
        "pt" | "por" => "por",
        "ko" | "kor" => "kor",
        "zh" | "zho" | "chi" => "zho",
        "hi" | "hin" => "hin",
        "ar" | "ara" => "ara",
        _ => language,
    }
}

impl Probe {
    pub(super) fn tracks(&self, kind: &str) -> Vec<MediaTrack> {
        self.streams
            .iter()
            .filter(|s| s.codec_type.as_deref() == Some(kind))
            .filter_map(|s| s.index.filter(|i| *i <= 65535).map(|index| (s, index)))
            .take(32)
            .map(|(s, input_index)| MediaTrack {
                input_index,
                codec: s
                    .codec_name
                    .as_ref()
                    .filter(|c| {
                        c.len() <= 32 && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    })
                    .cloned(),
                language: s.language(),
                language_status: s.language_status().into(),
                title: format!("{kind} track {input_index}"),
                disposition: s.disposition.as_ref().map(ProbeDisposition::public),
                selected: false,
                supported: s.selectable(),
                selectable: s.selectable(),
            })
            .collect()
    }
    pub(super) fn select_subtitle(
        &self,
        index: Option<u32>,
    ) -> Result<Option<&ProbeStream>, String> {
        let Some(index) = index else {
            return Ok(None);
        };
        let stream = self
            .streams
            .iter()
            .filter(|s| s.codec_type.as_deref() == Some("subtitle"))
            .filter(|s| s.index.is_some_and(|i| i <= 65535))
            .take(32)
            .find(|s| s.index == Some(index))
            .ok_or("Requested input subtitle track is unavailable")?;
        if !stream.text_subtitle() {
            return Err(
                "Requested subtitle track uses an unsupported bitmap or non-text codec".into(),
            );
        }
        Ok(Some(stream))
    }
    pub(super) fn select_audio(
        &self,
        selection: &TrackSelection,
    ) -> Result<Option<&ProbeStream>, String> {
        let audio: Vec<_> = self
            .streams
            .iter()
            .filter(|stream| stream.codec_type.as_deref() == Some("audio"))
            .filter(|stream| stream.index.is_some_and(|index| index <= 65535))
            .take(32)
            .collect();
        if let Some(index) = selection.audio_track_index {
            return audio
                .iter()
                .find(|stream| stream.index == Some(index))
                .copied()
                .map(Some)
                .ok_or_else(|| "Requested input audio track is unavailable".into());
        }
        if let Some(language) = selection.audio_language.as_deref() {
            let language = normalize_audio_language(language);
            let chosen = audio
                .iter()
                .copied()
                .filter(|stream| {
                    stream
                        .language()
                        .is_some_and(|actual| normalize_audio_language(&actual) == language)
                })
                .min_by_key(|stream| {
                    let d = stream.disposition.as_ref();
                    (
                        d.is_some_and(|d| {
                            d.comment == 1 || d.visual_impaired == 1 || d.hearing_impaired == 1
                        }),
                        !d.is_some_and(|d| d.default == 1),
                        stream.index,
                    )
                });
            return chosen.map(Some).ok_or_else(|| {
                "Preferred audio language is unavailable; choose a source or audio track".into()
            });
        }
        Ok(audio.into_iter().min_by_key(|stream| {
            let disposition = stream.disposition.as_ref();
            let alternate = disposition.is_some_and(|value| {
                value.comment == 1 || value.visual_impaired == 1 || value.hearing_impaired == 1
            });
            (
                alternate,
                !stream.language().is_some_and(|actual| {
                    normalize_audio_language(&actual)
                        == normalize_audio_language(
                            selection
                                .preferred_audio_language
                                .as_deref()
                                .unwrap_or("en"),
                        )
                }),
                !disposition.is_some_and(|value| value.default == 1),
                stream.index.unwrap_or(u32::MAX),
            )
        }))
    }
    pub(super) fn video(&self) -> Result<&ProbeStream, String> {
        self.streams
            .iter()
            .find(|s| s.is_video())
            .ok_or_else(|| "Source has no supported video stream".to_owned())
    }
    pub(super) fn interlaced(&self) -> bool {
        self.video().is_ok_and(|video| {
            matches!(
                video.field_order.as_deref(),
                Some("tt" | "bb" | "tb" | "bt")
            )
        })
    }
    pub(super) fn ensure_supported(&self) -> Result<(), String> {
        self.hdr_transfer().map(|_| ())
    }
    /// PQ or HLG transfer characteristics mean HDR no matter how complete the
    /// colour tagging is: tone-mapping handles missing primaries or matrix.
    /// Mislabeled encodes are common (bt2020 primaries or mastering-display
    /// side data on an SDR transfer), so partial HDR/wide-gamut evidence
    /// without PQ/HLG reads as SDR instead of refusing a playable source.
    /// Dolby Vision and other dynamic-HDR metadata decode as their base layer:
    /// FFmpeg drops the RPU and the transfer characteristics alone decide, so
    /// no metadata refuses playback.
    pub(super) fn hdr_transfer(&self) -> Result<Option<&str>, String> {
        Ok(self
            .video()?
            .color_transfer
            .as_deref()
            .filter(|transfer| matches!(*transfer, "smpte2084" | "arib-std-b67")))
    }
    pub(super) fn duration(&self) -> Option<f64> {
        let v = &self.format["duration"];
        v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            .filter(|d| d.is_finite() && *d > 0.0 && *d <= 604800.0)
    }
    #[cfg(test)]
    pub(super) fn compatible(&self, width: u32, height: u32) -> bool {
        self.compatible_audio(
            width,
            height,
            self.streams
                .iter()
                .find(|s| s.codec_type.as_deref() == Some("audio")),
        )
    }
    #[cfg(test)]
    pub(super) fn compatible_audio(
        &self,
        width: u32,
        height: u32,
        audio: Option<&ProbeStream>,
    ) -> bool {
        self.compatible_video(width, height, H264_COPY_LEVEL) && self.compatible_audio_stream(audio)
    }
    pub(super) fn compatible_video(&self, width: u32, height: u32, max_h264_level: u32) -> bool {
        if self.interlaced() || !matches!(self.hdr_transfer(), Ok(None)) {
            return false;
        }
        let Ok(video) = self.video() else {
            return false;
        };
        video.codec_name.as_deref() == Some("h264")
            && video.pix_fmt.as_deref() == Some("yuv420p")
            && matches!(
                video.profile.as_deref(),
                Some("Constrained Baseline" | "Baseline" | "Main" | "High")
            )
            && video
                .level
                .is_some_and(|level| level > 0 && level <= max_h264_level)
            && conservative_frame_rate(video.avg_frame_rate.as_deref())
            && conservative_frame_rate(video.r_frame_rate.as_deref())
            && video
                .width
                .is_some_and(|w| w >= 2 && w <= width && w % 2 == 0)
            && video
                .height
                .is_some_and(|h| h >= 2 && h <= height && h % 2 == 0)
    }
    pub(super) fn compatible_audio_stream(&self, audio: Option<&ProbeStream>) -> bool {
        audio.is_none_or(|stream| {
            stream.codec_name.as_deref() == Some("aac")
                && stream.profile.as_deref() == Some("LC")
                && stream
                    .channels
                    .is_some_and(|channels| channels > 0 && channels <= 2)
        })
    }
}
