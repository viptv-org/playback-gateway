# Live sharing format qualification

The shared-ingest eligibility test is private prepared-plan state, not public
v1 JSON. Ineligible independent output plans still charge separate physical
inputs; matching plans share their existing output job. VOD origins remain
distinct. Namespace, source headers/credential revision and egress route remain
part of the shared source identity.

## MPEG-2 extension

The lossless private MPEG-TS/HLS replay now also admits `mpeg2video`. This does
not claim native MPEG-2 decoder support or change client constraints: the real
fixture requests H264 outputs, and both are encoded from the same ingest.

```sh
PLAYBACK_TEST_FFMPEG=/path/to/qualified/ffmpeg \
PLAYBACK_TEST_FFPROBE=/path/to/qualified/ffprobe \
cargo test --locked -p playback-gateway --lib \
 workers::tests::real_mpeg2_live_differing_encoded_outputs_share_ingest_and_input_quota \
 -- --ignored --exact
```

2026-09-30: this named test passed exactly once using the extracted FFmpeg
9.0.2/GnuTLS image binaries. A controlled 320x180/25fps MPEG-2 TS source produces
distinct 320x180 and 160x90 H264 outputs, both fetched through viewer-authorized
media and actually demuxed/decoded. One physical input quota is sufficient;
joining an identical job or starting the different plan opens no new upstream.
Releasing the first job preserves the second; final real teardown closes the
upstream and reclaims input capacity.

The retained fallback fixture now uses FFV1/Matroska, not newly eligible MPEG-2.
It independently passed the same-plan-one-job/different-plans-two-inputs case.
Existing actual HTTP five-viewer/namespace-isolation and HLS/multiple-audio
different-output single-ingest regressions also passed with those binaries.
The default suite passes 100 tests (30 opt-in ignored), with strict all-target
Clippy and diff checks. These native process/library fixtures do not alone
qualify a newly built service image, physical device or hardware decoder.

## Remaining transport seams

- Other video/audio codecs still require lossless replay that supports their
  actual codec. Do not replace this with an unrequested encode at the ingest.
- Subtitle-bearing inputs still use independent pipelines. Subtitles must be
  retained/remapped, not silently discarded even when the first output omits them.
- Progressive v1 currently accepts compatible original-container transport,
  with no silent switch to HLS. Existing continuous fMP4 is single-consumer and
  not a shareable gateway output; flipping its eligibility is not fanout.

A bounded local experiment with the same FFmpeg 9 binaries verified that the
official segment-muxer Matroska/M3U8 seam retains H264, FLAC and subrip as indices
0/1/2 and is readable with only `file` protocols and explicit `mkv,m3u8`
extensions. This is **not** a production implementation or full qualification.
The experiment exposed an active-cue late-join issue: a subtitle packet begins
in an evicted segment but its display interval extends into the retained window.
Plain segment replacement loses that still-active cue. A subtitle implementation
must preserve carryover, timestamp alignment, sparse-track metadata and bounded
storage/cancellation, not simply add `.mkv` to the reader list. The segment muxer
also needs gateway-owned old-file deletion; list size alone does not delete files.

[Current official FFmpeg segment documentation](https://ffmpeg.org/ffmpeg-all.html#segment_002c-stream_005fsegment_002c-ssegment)
documents Matroska segments with a live M3U8 list. Real cue, track, reload and
teardown tests remain required before claiming this path shares live inputs.
