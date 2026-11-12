# Validation

`cargo test --locked --workspace` is the default gate. Real FFmpeg, Docker,
hardware and trusted-HTTPS fixtures are opt-in: they prove their named source,
format and platform only, and mocked process tests do not certify them.

Run the native opt-in checks with `PLAYBACK_TEST_FFMPEG` and
`PLAYBACK_TEST_FFPROBE` pointing at qualified binaries; TLS fixtures also need
`openssl`. Each area documents its own fixtures and commands:

- [gateway torrent runtime](docs/GATEWAY_TORRENT_RUNTIME.md) and
  [torrent/archive qualification](docs/TORRENT_QUALIFICATION.md)
- [progressive live delivery](docs/PROGRESSIVE_QUALIFICATION.md)
- [bounded media](docs/BOUNDED_MEDIA.md) and
  [sustained 4K copy](docs/CBR4K_QUALIFICATION.md)
- [container envelope](docs/CONTAINER_QUALIFICATION.md) and
  [Intel Quick Sync](docs/INTEL_QUICK_SYNC.md)
- [live sharing formats](docs/LIVE_SHARING_FORMATS.md) and
  [shared subtitles](docs/SHARED_SUBTITLES.md)

Source URLs, headers, API keys, media URLs and test or production databases must
never be committed. Fixture loopback exceptions compile only under cfg(test).
Generated media belongs in a dedicated volume.

Physical devices, public swarms, hardware/4K and production cutover are not
covered by these fixtures and stay separately qualified and approved.

## Image-named HLS through the validating egress proxy

The control plane now identifies its own destination-validating proxy separately
from a generic upstream proxy. This lets the sanitizer follow public CDN
playlists and segments through the same HTTP/CONNECT DNS/IP policy used by
ordinary playback. Cross-origin requests strip provider headers. Generic proxies
still refuse cross-origin original media; clients cannot set either reserved
proxy header. No FFmpeg extension-check relaxation is introduced.

The workspace gate passes 278 tests (62 opt-in checks ignored). The new transport
regression covers CDN traversal, credential and internal-header stripping,
private child refusal and conservative generic-proxy behavior. A real FFmpeg 9
fixture also passes: H.264/AAC fragmented MP4, a cross-origin image-named variant,
GIF-named initialization map and JPEG-named segments with image MIME types. That
fixture contacts only an isolated local proxy. It establishes the sanitizer and
probe path, separately from external source and physical-device qualification.


Protected `.m3u8`/`.m3u` inputs now use that nested-resource transport before
inspection instead of the binary-only range relay. Neither the probe nor later
readers receive provider headers as process arguments. The real fragmented-MP4
fixture runs with and without a provider header; regressions cover route
classification and the process-argument boundary.

Playlist rewriting removes only AUDIO references whose group is not declared.
Declared separate/in-band audio groups and quoted CODECS values containing
commas are preserved. This addresses malformed masters that prevent Roku from
finding muxed AAC. It does not replace missing upstream media: the investigated
1080p source returns HTTP 404 for its sixth video segment even when fetched
directly from the CDN. A ready admission and short decode do not establish
whole-title continuity; physical playback evidence is recorded separately.


Authenticated HLS inputs can also use opaque URLs without a playlist suffix.
When the binary range relay refuses such an input, HLS output may attempt the
bounded nested-playlist mediator through a reserved server-only instruction.
Actual playlist bytes are mandatory. Invalid/HTML inputs never fall back to a
raw FFmpeg header block; the instruction is rejected from public input headers
and stripped from origin requests and process arguments. The real cross-CDN
fragmented-MP4 fixture passes with an authenticated extensionless root as well
as the ordinary playlist suffix. The workspace gate passes 278 tests with 62
opt-in checks ignored, plus that explicitly executed real-media fixture.


## Consumption-paced caption cache regression

A physical paused/resumed WebVTT run exposed a separate cache leak: watched AV
segments were reclaimed, but VTT segments accumulated until the unchanged
watchdog retired the output. Viewer renewals were healthy; later media 401s were
secondary to that processing failure. On-demand cleanup now reclaims caption
prefixes using their own EXTINF timeline and the same consumed-time grace as AV,
including after the producer has exited. A simulated 20-minute AV/VTT filesystem
run remains below the original watchdog and retains current captions. Irregular
caption durations and repeat cleanup are covered. The workspace gate passes
280 tests; cache file-count, byte and per-file limits are unchanged.

## Rapid track replacement and resumed H.264 synchronization

An API-only reproduction of successive Hindi → English → English subtitles
returned `input_capacity` on the third admission: a fully released producer
continued charging the key for the 15-second reuse window. Under input/output
pressure, admission now marks only that key's fully released ready jobs for
teardown and waits for actual resource cleanup. Active viewers, other keys and
normal idle reuse remain intact; quota is never freed before teardown. Ledger
and worker regressions cover immediate replacements and retained quota during
cleanup. Limits are unchanged.

A separate actual-source comparison found a resumed H.264 picture-content shift
of about 0.8 seconds with the deployed QSV decoder, while selected English audio
content matched within 20 milliseconds. Packet PTS alone appeared aligned.
Software H.264 decoding with QSV encoding removed the measured picture shift.
Resumed H.264 transcodes now use that existing hybrid pipeline; initial playback,
remux/copy, HEVC and GPU encoding retain their existing paths. This increases
CPU decoding work for those resumed H.264 transcodes. Physical lip-sync and
subtitle visibility require a separate device check.

The workspace gate passes 282 tests with 62 opt-in cases ignored. A fresh
library-only run after the final authorization guards also passes.

The isolated rebuilt candidate passes two rounds of five immediate actual-source
track changes (Hindi, English, English subtitles, Off, SDH) under a two-input
quota. Every admission returns 201/ready; retired viewer leases and temporary
keys are released. A decoded eight-second prefix after the final resumed SDH
selection matches source picture content at the sampled frames (100 ms grid)
and English audio content within about 20 ms. The old candidate's picture was
about 0.8 seconds behind. This is media evidence, not physical TV lip-sync.

An accelerated actual-source caption run consumes 600.14 seconds of Loki media
through the candidate API: 300 AV segments and 179 successful current-caption
checks, with nonempty WebVTT cues and a ready session at completion. It finishes
in 42.84 wall seconds, so this is ten minutes of media consumption, not ten
minutes of physical viewing. Viewer and temporary key cleanup return 204. The
unchanged cache watchdog remains enabled throughout.

## High-rate VOD HLS cache budget

An actual 1080p CineWave AVC remux played decoded picture and audio, then failed
in a browser after roughly 33 seconds and on Roku with “no valid bitrates”.
Viewer renewals remained healthy. The output reached 409,386,538 bytes with 50
files and no captions, crossing the existing 384 MiB cache watchdog. Its largest
segment was below the separate per-file bound; this was aggregate retention.

Inspection now retains video/container bitrate, Matroska BPS tags, and a
size/duration fallback. Known VOD HLS video rates above 24 Mbit/s use the existing
bounded H.264 encoder (up to 1080p/30 and 5 Mbit/s) so the production lead and
60-second deletion grace fit the cache. This trades original video bitrate for
managed-delivery continuity and uses encoding resources. Ordinary lower-rate
copy, original delivery, streaming fMP4 and Live retain their existing paths.
Unknown rates retain legacy behavior under the same watchdog. No cache limits
or egress protections were relaxed. Watchdog warnings now include numeric
file-count/byte evidence without URLs or filenames.

The workspace gate passes 285 tests with 62 opt-in checks ignored. Planner
regressions cover bitrate strings/numbers, Matroska tags, container fallback,
unknown metadata, threshold boundaries, and the bounded 4K conversion envelope
while retaining ordinary 4K copy.

The isolated rebuilt candidate decodes the same actual source for 160 seconds:
3858 video frames, nonzero PCM audio, a ready lease, and a successful release.
A second 110-second decoded run also stays healthy. An additional accelerated
read consumes 181.01 seconds of media, with 160 cache observations peaking at
54,400,698 bytes (about 51.9 MiB), 53 files, and a 1,704,032-byte largest file.
An eight-second content comparison at title time 20 seconds matches picture on
a 100 ms grid and English audio within about 20 ms. These are explicitly
executed actual-source/candidate checks; physical Roku continuity after the
beta gateway update remains a separate gate.

The qualified source revision `a3dc95d` is deployed to the beta gateway through
the managed Compose entry after a SQLite online backup. Its deployment reports
`done`, the HTTPS readiness check returns 200, and only the beta gateway
container ID changed. The other service container IDs are unchanged. The same
13.2 GiB AVC source then plays on the physical Roku beyond 332 seconds with
H.264/AAC and no error; pause, paused 10-second seek and resume succeed. Its
stored language is English. A post-rollout browser run also decodes 102 seconds
with PCM audio and a ready lease. This is scoped source/device evidence, not
a whole-catalog or whole-title continuity claim.
