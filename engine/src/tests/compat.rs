use super::*;

#[test]
fn direct_transport_capabilities_preserve_legacy_and_gate_inspected_format() {
    let mut probe: Probe = serde_json::from_value(serde_json::json!({
        "streams": [
            {"codec_type":"video","codec_name":"h264","width":1280,"height":720,
             "pix_fmt":"yuv420p","profile":"High","level":41,
             "avg_frame_rate":"30/1","r_frame_rate":"30/1"},
            {"codec_type":"audio","codec_name":"aac","profile":"LC","channels":2}
        ],
        "format":{"format_name":"mov,mp4"}
    }))
    .unwrap();
    let legacy: Capabilities = serde_json::from_value(serde_json::json!({
        "direct_play":true,"h264":true,"aac":true
    }))
    .unwrap();
    assert!(legacy.direct_play);
    assert!(!Capabilities::default().direct_play);
    let selection = TrackSelection::default();
    for (format, expected) in [("mov,mp4", "mp4"), ("hls", "hls")] {
        probe.format["format_name"] = serde_json::json!(format);
        assert_eq!(
            direct_format(&probe, &legacy, probe.streams.get(1), &selection),
            Some(expected)
        );
        for (mp4, hls) in [(true, false), (false, true), (false, false)] {
            let caps = Capabilities {
                direct_mp4: Some(mp4),
                direct_hls: Some(hls),
                ..legacy.clone()
            };
            let supported = if expected == "mp4" { mp4 } else { hls };
            assert_eq!(
                direct_format(&probe, &caps, probe.streams.get(1), &selection),
                supported.then_some(expected)
            );
        }
    }
}

#[test]
fn a_webcodecs_client_gets_the_original_container_it_declares() {
    let probe: Probe = serde_json::from_value(serde_json::json!({
        "streams": [
            {"codec_type":"video","codec_name":"h264","width":1920,"height":1080,
             "pix_fmt":"yuv420p","profile":"High","level":41,
             "avg_frame_rate":"24/1","r_frame_rate":"24/1"},
            {"codec_type":"audio","codec_name":"eac3","channels":6}
        ],
        "format":{"format_name":"matroska,webm"}
    }))
    .unwrap();
    let selection = TrackSelection::default();
    let declared: Capabilities = serde_json::from_value(serde_json::json!({
        "direct_play": true, "h264": true, "aac": true,
        "max_width": 3840, "max_height": 2160,
        "direct_files": true,
        "direct_video_codecs": ["avc", "hevc", "av1"],
        "direct_audio_codecs": ["aac", "ac3", "eac3", "dts"]
    }))
    .unwrap();
    // Matroska with Dolby audio was previously remuxed; a client that reads the
    // container and both codecs now receives the original file.
    assert_eq!(
        direct_format(&probe, &declared, probe.streams.get(1), &selection),
        Some("mkv")
    );
    for (caps, reason) in [
        (
            Capabilities {
                direct_files: Some(false),
                ..declared.clone()
            },
            "no file path",
        ),
        (
            Capabilities {
                direct_video_codecs: Some(vec!["hevc".into()]),
                ..declared.clone()
            },
            "video codec not declared",
        ),
        (
            Capabilities {
                direct_audio_codecs: Some(vec!["aac".into()]),
                ..declared.clone()
            },
            "audio codec not declared",
        ),
        (
            Capabilities {
                max_height: 720,
                ..declared.clone()
            },
            "above the declared envelope",
        ),
        (
            Capabilities {
                direct_files: Some(true),
                h264: true,
                aac: true,
                ..Capabilities::default()
            },
            "nothing declared",
        ),
    ] {
        assert_eq!(
            direct_format(&probe, &caps, probe.streams.get(1), &selection),
            None,
            "{reason} must stay on managed delivery"
        );
    }
    // An interlaced picture is still converted rather than handed over raw.
    let interlaced: Probe = serde_json::from_value(serde_json::json!({
        "streams": [
            {"codec_type":"video","codec_name":"h264","width":1920,"height":1080,
             "pix_fmt":"yuv420p","profile":"High","level":41,"field_order":"tt",
             "avg_frame_rate":"25/1","r_frame_rate":"50/1"},
            {"codec_type":"audio","codec_name":"eac3","channels":6}
        ],
        "format":{"format_name":"matroska,webm"}
    }))
    .unwrap();
    assert_eq!(
        direct_format(
            &interlaced,
            &declared,
            interlaced.streams.get(1),
            &selection
        ),
        None
    );
}

#[test]
fn original_file_containers_cover_the_observed_catalogue() {
    // Every container in the WebCodecs demuxer's format set must have a
    // direct path; a missing entry silently costs the raw-file rung.
    for (format, expected) in [
        ("mov,mp4,m4a,3gp,3g2,mj2", "mp4"),
        ("matroska,webm", "mkv"),
        ("mpegts", "ts"),
        ("ogg", "ogg"),
        ("wav", "wav"),
        ("mp3", "mp3"),
        ("flac", "flac"),
        ("adts", "aac"),
    ] {
        assert_eq!(direct_file_extension(format), Some(expected), "{format}");
    }
    // Containers outside the demuxer's set keep managed delivery rather
    // than failing in a client that cannot read them.
    for format in ["avi", "flv", "asf", "mpeg", "unknown,container"] {
        assert_eq!(direct_file_extension(format), None, "{format}");
    }
}

/// A real live 720p60 channel must be stream-copied, not re-encoded.
///
/// Every field below is the value ffprobe reports for production IPTV live
/// sources (H.264 High 4.1, yuv420p, progressive, BT.709 SDR, 1280x720 at
/// 59.94fps, AAC-LC stereo in MPEG-TS). Re-encoding this in realtime cannot
/// keep up, so the managed HLS window underruns and playback stutters.
#[test]
fn a_720p60_live_channel_is_copied_instead_of_re_encoded() {
    let probe: Probe = serde_json::from_value(serde_json::json!({
        "streams": [
            {"codec_type":"video","codec_name":"h264","profile":"High","level":41,
             "pix_fmt":"yuv420p","width":1280,"height":720,"field_order":"progressive",
             "color_transfer":"bt709","color_primaries":"bt709","color_space":"bt709",
             "avg_frame_rate":"60000/1001","r_frame_rate":"60000/1001"},
            {"codec_type":"audio","codec_name":"aac","profile":"LC","channels":2}
        ],
        "format":{"format_name":"mpegts"}
    }))
    .unwrap();
    assert!(
        probe.compatible_audio(1280, 720, probe.streams.get(1)),
        "a 720p60 H.264/AAC live channel must take the copy/remux path"
    );
}

/// A level tag alone no longer gates copying: modern browsers decode H.264
/// level 5.1, and the envelope's dimension/fps/profile/pix-fmt/interlace/
/// SDR gates bound the real decode load. Level-4.2 720p60 and 1080p60
/// sources — exactly what a flat per-resolution level cap re-encoded —
/// must stream-copy instead.
#[test]
fn level_42_720p60_and_1080p60_sources_copy_without_re_encode() {
    for (width, height) in [(1280, 720), (1920, 1080)] {
        let probe: Probe = serde_json::from_value(serde_json::json!({
            "streams": [
                {"codec_type":"video","codec_name":"h264","profile":"High","level":42,
                 "pix_fmt":"yuv420p","width":width,"height":height,"field_order":"progressive",
                 "color_transfer":"bt709","color_primaries":"bt709","color_space":"bt709",
                 "avg_frame_rate":"60000/1001","r_frame_rate":"60000/1001"},
                {"codec_type":"audio","codec_name":"aac","profile":"LC","channels":2}
            ],
            "format":{"format_name":"mpegts"}
        }))
        .unwrap();
        assert!(
            probe.compatible_audio(width, height, probe.streams.get(1)),
            "{width}x{height} level 4.2 at 60fps must take the copy/remux path"
        );
    }
    // Frame rate ceiling admits 60fps but still refuses far-out values.
    for rate in ["60000/1001", "60/1", "30/1", "24/1", "25/1", "50/1"] {
        assert!(
            conservative_frame_rate(Some(rate)),
            "{rate} must pass through"
        );
    }
    for rate in ["120/1", "0/1", "bogus", ""] {
        assert!(
            !conservative_frame_rate(Some(rate)),
            "{rate} must not be treated as a copyable rate"
        );
    }
    assert!(!conservative_frame_rate(None));
}

#[test]
fn an_hdr_original_file_is_delivered_to_a_client_that_can_decode_it() {
    // Production sources are commonly HDR10/HLG with partial or absent colour
    // tags. A WebCodecs client decodes these itself, so the native SDR
    // envelope must not refuse the original file before it is offered.
    let selection = TrackSelection::default();
    let declared: Capabilities = serde_json::from_value(serde_json::json!({
        "direct_play": true, "h264": true, "aac": true,
        "max_width": 3840, "max_height": 2160, "hevc": true, "hevc_sdr": false,
        "direct_files": true,
        "direct_video_codecs": ["avc", "hevc", "av1"],
        "direct_audio_codecs": ["aac", "ac3", "eac3", "dts"]
    }))
    .unwrap();
    for (label, colour) in [
        (
            "tagged HDR10",
            serde_json::json!({"color_transfer":"smpte2084","color_primaries":"bt2020","color_space":"bt2020nc"}),
        ),
        (
            "untagged HDR10",
            serde_json::json!({"color_transfer":"smpte2084"}),
        ),
        ("HLG", serde_json::json!({"color_transfer":"arib-std-b67"})),
        (
            "wide gamut without transfer",
            serde_json::json!({"color_primaries":"bt2020","color_space":"bt2020nc"}),
        ),
        (
            "mastering-display side data only",
            serde_json::json!({"side_data_list":[{"side_data_type":"Mastering display metadata"}]}),
        ),
    ] {
        let mut video = serde_json::json!({
            "codec_type":"video","codec_name":"hevc","width":3840,"height":1608,
            "pix_fmt":"yuv420p10le","profile":"Main 10","level":153,
            "avg_frame_rate":"24/1","r_frame_rate":"24/1"
        });
        for (key, value) in colour.as_object().unwrap() {
            video[key] = value.clone();
        }
        let probe: Probe = serde_json::from_value(serde_json::json!({
            "streams": [
                video,
                {"codec_type":"audio","codec_name":"eac3","channels":6}
            ],
            "format":{"format_name":"matroska,webm"}
        }))
        .unwrap();
        assert_eq!(
            direct_format(&probe, &declared, probe.streams.get(1), &selection),
            Some("mkv"),
            "{label} must reach the client as the original file"
        );
    }
}

/// Interlacing still forces conversion even for a WebCodecs client: the
/// demuxer feeds a progressive decoder that cannot reconstruct fields.
#[test]
fn hdr_is_permitted_for_original_files_but_never_above_the_declared_envelope() {
    let selection = TrackSelection::default();
    let declared: Capabilities = serde_json::from_value(serde_json::json!({
        "direct_play": true, "h264": true, "aac": true,
        "max_width": 1920, "max_height": 1080,
        "direct_files": true,
        "direct_video_codecs": ["hevc"],
        "direct_audio_codecs": ["eac3"]
    }))
    .unwrap();
    let probe: Probe = serde_json::from_value(serde_json::json!({
        "streams": [
            {"codec_type":"video","codec_name":"hevc","width":3840,"height":2160,
             "pix_fmt":"yuv420p10le","profile":"Main 10","level":153,
             "color_transfer":"smpte2084","color_primaries":"bt2020","color_space":"bt2020nc",
             "avg_frame_rate":"24/1","r_frame_rate":"24/1"},
            {"codec_type":"audio","codec_name":"eac3","channels":6}
        ],
        "format":{"format_name":"matroska,webm"}
    }))
    .unwrap();
    // HDR is no longer a refusal, but a resolution above the declared
    // envelope still is.
    assert_eq!(
        direct_format(&probe, &declared, probe.streams.get(1), &selection),
        None
    );
}

#[test]
fn dynamic_hdr_metadata_reads_as_its_transfer() {
    // PQ/HLG transfer characteristics are HDR with or without complete
    // colour tags; tone-mapping does not need the primaries to be present.
    for transfer in ["smpte2084", "arib-std-b67"] {
        let probe: Probe = serde_json::from_value(serde_json::json!({"streams":[{
            "codec_type":"video", "pix_fmt":"yuv420p", "color_transfer": transfer
        }]}))
        .unwrap();
        assert_eq!(probe.hdr_transfer().unwrap(), Some(transfer));
    }
    // Mislabeled encodes carry bt2020 primaries or mastering-display side
    // data on an SDR transfer; treating those as HDR refused playable
    // sources outright.
    let mislabeled: Probe = serde_json::from_value(serde_json::json!({"streams":[{
        "codec_type":"video", "pix_fmt":"yuv420p",
        "color_transfer":"bt709", "color_primaries":"bt2020", "color_space":"bt2020nc",
        "side_data_list":[{"side_data_type":"Mastering display metadata"}]
    }]}))
    .unwrap();
    assert_eq!(mislabeled.hdr_transfer().unwrap(), None);
    // Dolby Vision and other dynamic-HDR side data no longer refuse
    // playback: the decoder drops the RPU and the base layer's transfer
    // decides, so a DoVi wrap around PQ tone-maps and DoVi SDR plays.
    let dynamic_pq: Probe = serde_json::from_value(serde_json::json!({"streams":[{
        "codec_type":"video", "color_transfer":"smpte2084",
        "side_data_list":[{"side_data_type":"DOVI configuration record"}]
    }]}))
    .unwrap();
    assert_eq!(dynamic_pq.hdr_transfer().unwrap(), Some("smpte2084"));
    let dynamic_sdr: Probe = serde_json::from_value(serde_json::json!({"streams":[{
        "codec_type":"video",
        "side_data_list":[{"side_data_type":"DOVI configuration record"}]
    }]}))
    .unwrap();
    assert_eq!(dynamic_sdr.hdr_transfer().unwrap(), None);
    assert!(dynamic_sdr.ensure_supported().is_ok());
}

#[test]
fn continuation_audio_language_is_verified_and_manual_index_overrides() {
    let probe: Probe = serde_json::from_value(serde_json::json!({"streams":[
        {"index":1,"codec_type":"audio","tags":{"language":"jpn"},"disposition":{"default":1}},
        {"index":2,"codec_type":"audio","tags":{"language":"eng"}},
        {"index":3,"codec_type":"audio","tags":{"language":"eng"},"disposition":{"comment":1}}
    ]}))
    .unwrap();
    let mut selection = TrackSelection {
        audio_language: Some("en-US".into()),
        ..Default::default()
    };
    assert_eq!(
        probe.select_audio(&selection).unwrap().unwrap().index,
        Some(2)
    );
    selection.audio_language = None;
    selection.preferred_audio_language = Some("ja".into());
    assert_eq!(
        probe.select_audio(&selection).unwrap().unwrap().index,
        Some(1)
    );
    selection.preferred_audio_language = Some("fr".into());
    assert!(
        probe.select_audio(&selection).unwrap().is_some(),
        "Unavailable profile preference is best-effort, unlike strict continuation matching"
    );
    selection.audio_language = Some("ita".into());
    assert!(probe.select_audio(&selection).is_err());
    selection.audio_track_index = Some(1);
    assert_eq!(
        probe.select_audio(&selection).unwrap().unwrap().index,
        Some(1)
    );
}

#[test]
fn default_main_precedes_commentary_and_reports_dispositions() {
    let mut probe:Probe=serde_json::from_value(serde_json::json!({"streams":[
        {"index":1,"codec_type":"audio","tags":{"language":"eng","title":"Commentary"},"disposition":{"default":1,"comment":1}},
        {"index":2,"codec_type":"audio","tags":{"language":"eng","title":"Main"},"disposition":{"default":0,"comment":0}},
        {"index":3,"codec_type":"audio","tags":{"language":"eng","title":"Default main"},"disposition":{"default":1,"comment":0}},
        {"index":4,"codec_type":"audio","tags":{"language":"ita"},"disposition":{"default":1,"comment":0}}
    ]})).unwrap();
    assert_eq!(
        probe
            .select_audio(&TrackSelection::default())
            .unwrap()
            .unwrap()
            .index,
        Some(3)
    );
    probe.streams.retain(|stream| stream.index != Some(3));
    assert_eq!(
        probe
            .select_audio(&TrackSelection::default())
            .unwrap()
            .unwrap()
            .index,
        Some(2),
        "English main audio outranks a different-language default"
    );
    probe.streams.retain(|stream| stream.index != Some(4));
    assert_eq!(
        probe
            .select_audio(&TrackSelection::default())
            .unwrap()
            .unwrap()
            .index,
        Some(2),
        "a clean main track must outrank default commentary"
    );
    let explicit = probe
        .select_audio(&TrackSelection {
            audio_track_index: Some(1),
            ..Default::default()
        })
        .unwrap()
        .unwrap();
    assert!(explicit.disposition.as_ref().unwrap().public().commentary);
    let value = serde_json::to_value(probe.tracks("audio")).unwrap();
    assert_eq!(value[0]["disposition"]["default"], true);
    assert_eq!(value[0]["disposition"]["commentary"], true);
    assert_eq!(value[1]["disposition"]["commentary"], false);
    assert_eq!(value[0]["disposition"]["forced"], false);
}

#[test]
fn audio_selection_honors_explicit_track_then_smart_defaults() {
    let p: Probe = serde_json::from_value(serde_json::json!({"streams":[
        {"index":0,"codec_type":"video","codec_name":"h264","pix_fmt":"yuv420p","profile":"Main","level":30,"width":160,"height":90,"avg_frame_rate":"24/1","r_frame_rate":"24/1"},
        {"index":1,"codec_type":"audio","codec_name":"mp3","tags":{"language":"ita","title":"English"}},
        {"index":2,"codec_type":"audio","codec_name":"aac","profile":"LC","channels":2,"tags":{"language":"ENG"}},
        {"index":3,"codec_type":"audio","tags":{"title":"English","language":"und"}},
        {"index":4,"codec_type":"subtitle","codec_name":"subrip","tags":{"language":"eng"}}
    ]})).unwrap();
    assert_eq!(
        p.select_audio(&TrackSelection::default())
            .unwrap()
            .unwrap()
            .index,
        Some(2)
    );
    assert_eq!(
        p.select_audio(&TrackSelection {
            audio_track_index: Some(1),
            ..Default::default()
        })
        .unwrap()
        .unwrap()
        .index,
        Some(1)
    );
    assert!(p
        .select_audio(&TrackSelection {
            audio_track_index: Some(0),
            ..Default::default()
        })
        .is_err());
    assert_eq!(
        p.select_audio(&TrackSelection {
            audio_track_index: Some(3),
            ..Default::default()
        })
        .unwrap()
        .unwrap()
        .language_status(),
        "unknown"
    );
    assert!(!p.compatible(1280, 720));
    assert!(p.compatible_audio(
        1280,
        720,
        p.select_audio(&TrackSelection::default()).unwrap()
    ));
    let tracks = p.tracks("audio");
    assert!(tracks.iter().all(|track| track.selectable));
    assert_eq!(p.tracks("subtitle")[0].input_index, 4);
    assert!(p.tracks("subtitle")[0].selectable);
    assert_eq!(p.select_subtitle(Some(4)).unwrap().unwrap().index, Some(4));
    assert!(p.select_subtitle(Some(2)).is_err());
    assert!(p.select_subtitle(None).unwrap().is_none());
    let bitmap: Probe = serde_json::from_value(serde_json::json!({"streams":[{"index":4,"codec_type":"subtitle","codec_name":"hdmv_pgs_subtitle"}]})).unwrap();
    assert!(!bitmap.tracks("subtitle")[0].supported);
    assert!(bitmap
        .select_subtitle(Some(4))
        .unwrap_err()
        .contains("unsupported bitmap"));
}

/// Embedded cover art (an MKV attachment or MP4 `covr`) is what ffprobe lists
/// as a second video stream with `level: -99`. A real release with it failed
/// the whole probe, paid a second reduced probe (13.7s), lost profile/level,
/// re-encoded copyable H.264 and was refused original-file delivery.
#[test]
fn cover_art_neither_fails_the_probe_nor_blocks_copy_or_direct_delivery() {
    let probe: Probe = serde_json::from_value(serde_json::json!({
        "streams": [
            {"index":0,"codec_type":"video","codec_name":"h264","profile":"High","level":41,
             "pix_fmt":"yuv420p","width":1920,"height":800,"field_order":"progressive",
             "avg_frame_rate":"24000/1001","r_frame_rate":"24000/1001",
             "disposition":{"default":1,"attached_pic":0}},
            {"index":1,"codec_type":"audio","codec_name":"aac","profile":"HE-AAC","channels":6,
             "disposition":{"default":1,"attached_pic":0}},
            {"index":4,"codec_type":"video","codec_name":"png","level":-99,"pix_fmt":"rgba",
             "width":250,"height":140,"avg_frame_rate":"0/0","r_frame_rate":"90000/1",
             "disposition":{"default":0,"attached_pic":1}}
        ],
        "format":{"format_name":"matroska,webm","duration":"8888.107"}
    }))
    .expect("a negative level must not fail the probe");
    assert_eq!(probe.streams[2].level, None, "-99 means unknown");
    assert_eq!(probe.video().unwrap().codec_name.as_deref(), Some("h264"));
    assert!(
        probe.compatible_video(1920, 1080, H264_COPY_LEVEL),
        "compatible H.264 beside cover art must be copied"
    );
    let caps = Capabilities {
        max_width: 3840,
        max_height: 2160,
        direct_play: true,
        direct_files: Some(true),
        direct_video_codecs: Some(vec!["avc".into()]),
        direct_audio_codecs: Some(vec!["aac".into()]),
        ..Default::default()
    };
    assert_eq!(
        direct_format(
            &probe,
            &caps,
            probe.streams.get(1),
            &TrackSelection::default()
        ),
        Some("mkv"),
        "cover art is not a second programme video"
    );
}
