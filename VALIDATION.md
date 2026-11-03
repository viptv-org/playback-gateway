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