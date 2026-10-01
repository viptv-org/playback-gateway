# Shared subtitle replay contract

Gateway issue 2 owns generic media behavior only. The intended public seams are
the actual control/media HTTP(S) API and decoded fetched media. No VIPTV UI or
live seek origin changes are included.

One source ingest captures all eligible subtitle tracks, preserving original
sparse track identities independently of deterministic private replay indices.
FFmpeg decodes caption packets to local WebVTT pipes on the absolute media clock.
A bounded gateway-owned cue cache retains long active intervals after the AV
segment containing the original packet is evicted. Authorized output renditions
select overlapping cues by actual AV segment timestamps. Stable cue identifiers
let players recognize a cue repeated across consecutive HLS subtitle segments;
there is one copy of each cue per segment. Public caption stream index is zero
within the actual separate WebVTT rendition.

Qualified shared captions are SubRip on Linux with already
eligible AV codecs. ASS/SSA/WebVTT, bitmap/DVB/PGS/teletext, mov_text/text, unlisted
AV codecs and other platforms retain independent execution. Plain WebVTT
conversion has the existing styling limitations. At most 32 subtitle tracks,
32 KiB per decoded block and 1 MiB/8192 cached cues are accepted. Completed cues
have 180 seconds of reader grace; active intervals survive regardless of age
within those byte/count limits. Overflow stops the ingest with the existing safe
processing failure. A separate kernel media bound is still supplied by Compose;
caption cache limits do not replace the filesystem quota or replay watchdog.

The implementation must prove actual decoded timing, long overlaps, late joining
after original cue-segment eviction, seeking among retained output segments,
single compatible upstream, independent authorization/release/expiry and actual
process/storage teardown against its built image. HTTP and trusted HTTPS sources
must be exercised. Qualification evidence will be recorded after these checks;
this contract document alone does not claim deployment.

## Built-image qualification — 2026-09-30

Runtime source `f056a0dd31d6443c45706ac35dc08406d28a710b` was built from a Git
archive as image
`sha256:5efbd97e78573879a696f4bd3c4c22165c3a65e500bf5695e1cb4881ca95eed8`.
Subsequent commits change fixtures, documentation and mechanical Rust formatting;
runtime behavior is unchanged. Image evidence remains tied to this exact source
and image, not a claim that an image was rebuilt after the formatting commit.
The source-pinned fixture `44c7449181e39e9dabc5ab21029aba0bdf92da4f` exercises the real service binary and
its exact FFmpeg 9.0.2/runtime libraries inside disposable network-none Docker.

The source has video index 2, audio indices 0/3 and SubRip indices 1/4. Actual
private replay indices are verified before readers start. Public selected audio
retains source 3 while output audio is stream 1 and actually decodes its 440-Hz
tone rather than source audio 0's 880-Hz tone; selected English captions retain
source 4 while the fetched WebVTT demuxer reports stream 0. The original
source-off output and a same-job viewer run before a differently sized late
caption-enabled output joins. One input reservation and one active upstream
serve them; the upstream open count does not increase on compatible joins.

After the actual original cue `.mkv` segment disappears, the late encoded output
decodes at 64 pixels wide on absolute AV timestamps beyond 25 seconds. Fetched
WebVTT actually demuxes the 1-second/74-second and 2-second/63-second source cue
intervals, including their long overlap, within 50 ms start/1 ms duration bounds.
Expired and wrong-language cues are absent. Requests for two retained AV segments
decode real video frames after a 0.5-second local decoder seek; their caption
segments contain each active cue once and use stable cue IDs. The control API
still rejects nonzero live origins.

Release and a tampered capability deny reads while the other output continues.
A viewer expires after its real 60-second lease even while it reads media; its
renewal returns 410 while the independently renewed late output remains usable.
SIGTERM reaps actual ingest/output PIDs, closes the upstream and removes all owned
replay/output directories. A separate source with seventy overlapping 20-KiB
decoded cue bodies exceeds the 1-MiB cache budget: an initially ready viewer
transitions to safe `processing_failed`, the real upstream closes, and physical
input/output reservations return. Overflow is not a promise that first-segment
admission always fails before returning 201.

HTTP fixtures passed all three named checks once against the image above
(9.48 seconds identity, 70.14 seconds late activation/expiry, 1.97 seconds cache
overflow). Trusted HTTPS control/media and provider fixtures also passed all
three checks (9.67 seconds, 70.30 seconds, 2.11 seconds respectively) and use a
fixture-only CA-signed leaf and a Python standard-library TLS proxy layered onto
the same runtime image. Verification remains enabled, no host trust changes,
external network access or runtime TLS exemptions are introduced. The earlier
self-signed CA-as-server-leaf fixture failed actual client verification and was
replaced with a valid distinct leaf. This does not claim public ingress deployment.

The same image also passed the existing real 2-MiB child tmpfs fixture in subtitle
mode (27.58 seconds): actual SubRip private `.mkv` writes, UID/GID10001/caps0,
kernel ENOSPC/exact allocation bound, readiness503, safe affected-viewer failure,
actual process/upstream teardown, reservation reclamation, persistent keys and
fresh playback after restart. Private cue pipes create no filesystem pathname.
Only newly returned fixture container IDs and their own anonymous volumes are
removed; no production resources are involved.

Reproduce with the existing exact-image harness:

```sh
sudo -n bash scripts/check-runtime-container.sh LOCAL_GATEWAY_IMAGE subtitles
sudo -n bash scripts/check-runtime-container.sh LOCAL_GATEWAY_IMAGE subtitles-tls
sudo -n bash scripts/check-bounded-media-container.sh LOCAL_GATEWAY_IMAGE subtitles
```

The default suite passes 104 tests with 36 explicit opt-in fixtures ignored;
strict all-target Clippy passes. Scoped `rustfmt --check` with `skip_children=true`
passes for all twelve Rust files changed by this ticket. Whole-workspace
`cargo fmt --all --check` still fails on five untouched baseline files:
`control/src/lib.rs`, `engine/examples/browser_check.rs`,
`engine/src/direct/tests.rs`, `engine/src/tests/throttle.rs` and
`engine/src/tests/vaapi.rs`. They remain byte-identical to `fb15c39`; no overall
workspace-format pass is claimed. This qualifies Linux SubRip with the tested
H264/AAC source, not ASS/SSA/WebVTT/bitmap sharing, arbitrary encodings/styles,
all AV combinations, native device rendering, universal formats or deployment.
