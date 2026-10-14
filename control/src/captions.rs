//! Bounded plain caption conversion. No URLs, network clients or application identities.
use serde::{Deserialize, Serialize};
pub(crate) const MAX_BYTES: usize = 2 * 1024 * 1024;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Upload {
    pub content_vtt: String,
}
#[derive(Serialize)]
pub(crate) struct Delivery {
    pub url: String,
    pub format: &'static str,
    pub timeline_offset_seconds: f64,
}
fn clock(raw: &str) -> Option<u64> {
    let (time, fraction) = raw.split_once('.')?;
    if fraction.len() != 3 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let parts = time.split(':').collect::<Vec<_>>();
    if !(2..=3).contains(&parts.len())
        || parts
            .iter()
            .any(|p| p.len() != 2 || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let mut numbers = parts.iter().map(|p| p.parse::<u64>().ok());
    let (hour, minute, second) = if parts.len() == 3 {
        (numbers.next()??, numbers.next()??, numbers.next()??)
    } else {
        (0, numbers.next()??, numbers.next()??)
    };
    if minute > 59 || second > 59 {
        return None;
    }
    Some(((hour * 60 + minute) * 60 + second) * 1000 + fraction.parse::<u64>().ok()?)
}
fn timestamp(ms: u64) -> String {
    format!(
        "{:02}:{:02}:{:02},{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}
pub(crate) fn srt(vtt: &str, offset: f64) -> Result<String, &'static str> {
    if vtt.len() > MAX_BYTES
        || !offset.is_finite()
        || !(0.0..=1e9).contains(&offset)
        || vtt
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t'))
    {
        return Err("invalid_caption");
    }
    let normalized = vtt
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut blocks = normalized.split("\n\n");
    let header = blocks.next().ok_or("invalid_caption")?;
    let first = header.lines().next().unwrap_or_default();
    if !(first == "WEBVTT" || first.starts_with("WEBVTT ") || first.starts_with("WEBVTT\t")) {
        return Err("invalid_caption");
    }
    // VTT CSS, regions and notes have no SRT equivalent and never become cue text.
    let offset = (offset * 1000.0).round() as u64;
    let mut output = String::new();
    let mut count = 0;
    let mut observed = 0;
    for block in blocks.filter(|b| !b.trim().is_empty()) {
        let mut lines = block.trim_matches('\n').lines();
        let first = lines.next().ok_or("invalid_caption")?;
        if first == "STYLE"
            || first == "REGION"
            || first == "NOTE"
            || first.starts_with("NOTE ")
            || first.starts_with("NOTE\t")
        {
            continue;
        }
        let timing = if first.contains("-->") {
            first
        } else {
            lines.next().ok_or("invalid_caption")?
        };
        let (start, end) = timing.split_once("-->").ok_or("invalid_caption")?;
        let start = clock(start.trim()).ok_or("invalid_caption")?;
        let end = clock(end.split_whitespace().next().ok_or("invalid_caption")?)
            .ok_or("invalid_caption")?;
        let text = lines.collect::<Vec<_>>().join("\n");
        observed += 1;
        if end <= start || text.is_empty() || observed > 20_000 {
            return Err("invalid_caption");
        }
        if end <= offset {
            continue;
        }
        count += 1;
        output.push_str(&format!(
            "{count}\n{} --> {}\n{text}\n\n",
            timestamp(start.saturating_sub(offset)),
            timestamp(end - offset)
        ));
        if output.len() > MAX_BYTES {
            return Err("invalid_caption");
        }
    }
    if observed == 0 {
        return Err("invalid_caption");
    }
    Ok(output)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cut_rebases_cues_keeps_overlap_and_utf8_without_rendering_vtt_metadata() {
        let vtt="WEBVTT\n\nSTYLE\n::cue { color: red }\n\nNOTE source\nprivate note\n\nold\n00:00:01.000 --> 00:00:02.000\nOld\n\noverlap\n00:00:02.000 --> 00:00:04.000 align:start\nCafé\n\n00:05.000 --> 00:06.000\nNext\n";
        assert_eq!(
            srt(vtt, 3.0).unwrap(),
            "1\n00:00:00,000 --> 00:00:01,000\nCafé\n\n2\n00:00:02,000 --> 00:00:03,000\nNext\n\n"
        );
        assert!(srt(vtt, 7.0).unwrap().is_empty());
        assert!(srt(vtt, 0.0)
            .unwrap()
            .contains("00:00:01,000 --> 00:00:02,000"));
    }
    #[test]
    fn malformed_binary_oversized_or_ambiguous_timing_is_rejected() {
        for vtt in [
            "bad",
            "WEBVTT\n\n00:00:01.000 --> 00:00:00.000\nx",
            "WEBVTT\n\n00:60:00.000 --> 00:61:00.000\nx",
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n\0",
        ] {
            assert!(srt(vtt, 0.0).is_err());
        }
        assert!(srt(&"x".repeat(MAX_BYTES + 1), 0.0).is_err());
        assert!(srt("WEBVTT", f64::NAN).is_err());
    }
}
