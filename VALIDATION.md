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
