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

The workspace gate passes 274 tests (62 opt-in checks ignored). The new transport
regression covers CDN traversal, credential and internal-header stripping,
private child refusal and conservative generic-proxy behavior. A real FFmpeg 9
fixture also passes: H.264/AAC fragmented MP4, a cross-origin image-named variant,
GIF-named initialization map and JPEG-named segments with image MIME types. That
fixture contacts only an isolated local proxy. It establishes the sanitizer and
probe path, separately from external source and physical-device qualification.
