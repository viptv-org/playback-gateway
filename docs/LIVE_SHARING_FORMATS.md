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

The retained fallback fixture now uses MPEG4/Matroska, not newly eligible MPEG-2
or FFV1 (the preceding MPEG-2 checkpoint used FFV1 as its fallback).
It independently passed the same-plan-one-job/different-plans-two-inputs case.
Existing actual HTTP five-viewer/namespace-isolation and HLS/multiple-audio
different-output single-ingest regressions also passed with those binaries.
The default suite passes 104 tests (33 opt-in ignored), with strict all-target
Clippy and diff checks. These native process/library fixtures do not alone
qualify a newly built service image, physical device or hardware decoder.

## Private Matroska AV adapter

Verified `ffv1` video or `flac` audio now selects private Matroska segments and a
rolling M3U8 list; other eligible AV codecs retain the original private TS path.
Public HLS format and JSON are unchanged. Only gateway-owned `.mkv,.m3u8` local
files are allowed to private readers; network protocols/headers never propagate.
This is actual FFmpeg internal demux adaptation, not standard public Matroska HLS.

The two real native fixtures use FFV1+two FLAC tracks, and H264+two FLAC tracks.
Source indices audio0/video1/audio2 normalize to private video0/audio1/audio2.
Both choose original audio2 and actually decode its 440 Hz content, not audio0's
880 Hz. FFV1 produces two differently sized H264/AAC outputs; the H264 source
retains copy at its original size while the smaller output encodes. Each late
output joins only after old-file eviction, without another upstream or input slot.
Private packet PTS stays on the absolute media clock after eviction and available
DTS is monotonic per track; actual public output A/V start times differ by <100 ms.
Both outputs demux/decode, and release/reap reclaims the input and media directory.

The segment muxer limits list entries but does not delete files. Owned cleanup
validates generated numeric names, retains 12 listed/three evicted segments and
never removes the unlisted new/open segment. It runs during startup, inspection
and steady state. Unit tests reject external/traversal/unsorted names without
deletion and count open/unlisted `.mkv` lengths toward the configured watchdog.
A real 2 MiB logical-budget fixture terminates the producer safely and retains
input admission until actual output cleanup. This sampled watchdog is **not**
the hard filesystem bound. All Matroska/list/temp writes remain under the same
`DATA_DIR/media` child tmpfs; no new writable path or runtime privilege is added.

Named opt-in tests:

```text
workers::tests::real_matroska_av_live_outputs_share_indices_late_join_eviction_and_reap
workers::tests::real_matroska_h264_flac_copy_and_encoded_live_outputs_share_ingest
workers::tests::real_matroska_budget_failure_holds_input_until_actual_output_teardown
```

For exact-image physical allocation failure/reclamation/restart, the existing
container fixture additionally supports:

```sh
sudo -n bash scripts/check-bounded-media-container.sh LOCAL_GATEWAY_IMAGE matroska
sudo -n bash scripts/check-runtime-container.sh LOCAL_GATEWAY_IMAGE matroska
```

It must observe actual private `.mkv` writes under a 2 MiB kernel mount, not merely
run the TS fixture under a different name. Its test-only controller privileges
do not broaden the actual UID10001/no-capabilities service envelope.

An extra exact-image crash test caught missing orphan recognition for `.mkv`:
processes were killed/reaped, but restart retained its private UUID directory.
Startup cleanup now recognizes only strictly named private Matroska segments
(and partials) inside its existing canonical UUIDv4/regular-file guards. Foreign
files prevent deletion, and `.mkv` remains rejected by public `media_type`.
The unit regression verifies both cleanup and preservation; the runtime script
supports the same actual source kind for crash and active/stalled response tests.

### Exact-image AV checkpoint

2026-09-30: committed runtime source
`3b53c84407e4bb2cd08d1e7e3d9940a98d6d0def`, image
`sha256:caecba4a56298346ccf839229b8b4ddbdea9ab63e4eb961cce2bed51a7748d5d`.
The image passed all three named runtime fixtures exactly once in Matroska mode:
readiness/lock/shutdown; one ingest/two outputs with active and unread/stalled
HTTP response shutdown (11.15 seconds); SIGKILL child reap/orphan cleanup/restart
and fresh playback (6.01 seconds). The same exact image passed real 2 MiB tmpfs
ENOSPC/safe failure/process teardown/reclamation/persistent-key/restart in
Matroska mode (10.01 seconds), and the production non-root/read-only/no-caps,
private-volume lock and stop/start credential-persistence envelope.

The scripts remove only their newly returned container IDs and anonymous fixture
volumes. The envelope preserves a private stopped-data archive before removing
its own named test volume. No provider, production data, deployment, Android or
hardware access is involved. These AV checks do not close subtitle/progressive
or all-codec sharing, long-run/high-bitrate Matroska, TLS-to-Matroska source,
physical decoder or hardware acceptance.

## Remaining transport seams

- Unlisted video/audio codecs still require lossless replay that supports their
  actual codec. Do not replace this with an unrequested encode at the ingest.
- Linux SubRip-bearing inputs share the bounded cue adapter described in
  [shared subtitles](SHARED_SUBTITLES.md), including an initially caption-disabled
  output. Other subtitle formats/platforms retain independent pipelines and
  truthful physical-input accounting; none are silently discarded.
- Progressive v1 is a same-container lossless remux of the shared private
  replay for live MPEG-TS inputs and Matroska-replay Matroska inputs only (see
  [API v1](../API_V1.md#progressive-live-delivery)). Each viewer owns one
  bounded copy-only reader; other inputs fail with `unsupported_output`, with no
  silent switch to HLS. Arbitrary byte ranges, seeking and HLS or MP4 inputs
  remain unsupported. Existing continuous fMP4 is single-consumer and not a
  shareable gateway output; flipping its eligibility is not fanout.
- Progressive evidence is the real-FFmpeg fixture
  `real_progressive_viewers_share_one_upstream_late_join_decodes_and_last_release_reaps`
  (MPEG-TS H264/AAC over plain loopback HTTP: two viewers, one upstream, a late
  join that starts on a keyframe and decodes, and last-release cleanup).
  [Progressive qualification](PROGRESSIVE_QUALIFICATION.md) records the
  built-image evidence: MPEG-TS progressive passes over HTTP and HTTPS and
  under a 2 MiB media tmpfs; Matroska progressive stalls on the built image and
  is not qualified.

Before the shared subtitle implementation, a bounded local experiment with the same FFmpeg 9 binaries verified that the
official segment-muxer Matroska/M3U8 seam retains H264, FLAC and subrip as indices
0/1/2 and is readable with only `file` protocols and explicit `mkv,m3u8`
extensions. This subtitle experiment is **not** a runtime implementation or full qualification.
That historical AV-only path excluded the subtitle case; the qualified SubRip
adapter now owns decoded cue carryover rather than relying on packets alone.
The experiment exposed an active-cue late-join issue: a subtitle packet begins
in an evicted segment but its display interval extends into the retained window.
Plain segment replacement loses that still-active cue. A subtitle implementation
must preserve carryover, timestamp alignment, sparse-track metadata and bounded
storage/cancellation, not simply add `.mkv` to the reader list. The segment muxer
requires gateway-owned old-file deletion (now implemented for AV); list size
alone does not delete files.

[Current official FFmpeg segment documentation](https://ffmpeg.org/ffmpeg-all.html#segment_002c-stream_005fsegment_002c-ssegment)
documents Matroska segments with a live M3U8 list. Real cue, track, reload and
teardown tests were required beyond that experiment; the subsequently qualified
SubRip path and its explicit unsupported formats are recorded in shared subtitles.
