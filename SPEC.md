# Playback gateway

Independent, generic HTTP(S) media execution service. An integration (such as the
VIPTV backend) creates playback sessions through a scoped control API; the
gateway inspects the input, plans passthrough/remux/transcode output, shares
compatible jobs and authorizes each viewer's media separately.

Contract: [API_V1.md](API_V1.md). Evidence: [VALIDATION.md](VALIDATION.md) and
`docs/`. Engine provenance: [PROVENANCE.md](PROVENANCE.md).

## Owns

- **Credentials:** an environment bootstrap key that only manages revocable
  integration keys; integration keys scoped by exact namespaces, operation
  scopes (`capabilities`, `create`, `read`, `renew`, `release`) and
  input/output/viewer quotas; per-viewer media capability URLs that never reach
  control routes.
- **Ingestion:** HTTP and HTTPS inputs with private/reserved upstream addresses
  denied (including redirects and child resources), bounded upstream headers,
  and qualified GnuTLS FFmpeg/ffprobe with TLS verification on.
- **Disguised HLS:** original (direct) HLS delivery classifies each whole
  segment, init section or part by its first 64 KiB, drops junk before the
  first MPEG-TS packet run, fMP4 box chain or ADTS/ID3 audio (IPTV origins
  prepend PNG/JPEG/GIF headers or CSS text), and serves `video/mp2t`,
  `video/mp4` or `audio/aac` instead of the origin's lying type. Byte ranges
  stay in origin coordinates (EXT-X-BYTERANGE offsets are unchanged): a range
  from byte 0 is answered as the clean sub-range past the junk (its
  `Content-Range` and `Content-Length` say so), and a later range passes
  through. Keys and session data (`.key` resources) and WebVTT pass through
  byte-for-byte. FFmpeg readers of a disguised source (refused segment names,
  an image-typefound segment, or an image-only HLS probe without audio) read
  it through a gateway-internal loopback sanitizer built on the same
  transport: `127.0.0.1` on an ephemeral port behind a 244-bit capability
  token compared in constant time (any other path is 404), at most 16
  concurrent requests, and the transport's bounds (6 upstream fetches, 8 MiB
  segment cache, 4 MiB playlists, 4096 resources, private-destination
  refusal, consumed egress-proxy route, cross-origin `Authorization`/`Cookie`
  stripping, opaque resource names). FFmpeg receives only the loopback URL:
  no headers, no `-http_proxy`, `-protocol_whitelist http,tcp,crypto` and no
  `-extension_picky` (rewritten names are `.ts`/`.m4s`/`.aac`, so FFmpeg
  before 7.1 works too). A reconnect range after a stripped whole segment is
  shifted back to origin coordinates. The token is never logged and is
  redacted from FFmpeg diagnostics. Lifecycle: the probe's sanitizer ends
  with the probe; every reader (output job and its seek probe, shared replay
  ingest — which also feeds progressive delivery — and each subtitle
  extraction) starts its own and drops it with the job, child or session.
  The probe result records only that the source is disguised, never a
  sanitizer, so cached and session-held probe copies hold no listener or
  reservations. A source the sanitizer cannot prepare (for example
  cross-origin segments behind an egress proxy) fails inspection as before.
- **Output jobs:** inspected, normalized output plans with closed conversion
  choices (`auto`, `audio`, `video`, `audio_video`) and explicit track/language
  selection; compatible viewers share one job; eligible live outputs share one
  upstream ingest through the bounded private replay; unqualified inputs keep
  independent execution and fail explicitly rather than claiming support.
- **Viewer leases:** 60-second leases renewed by the integration, independent
  release/expiry/revocation, 15-second zero-viewer grace, and quota held until
  worker teardown.
- **Bounded media:** retained-segment replay, operator byte watchdog and the
  supplied Compose tmpfs bounds (docs/BOUNDED_MEDIA.md).
- **Packaging:** the Docker image (non-root, read-only root, dedicated data
  volume, loopback-only Compose binding). Public HTTPS ingress is an operator
  action, never automatic.

## Does not own

Accounts, profiles, catalog matching, provider configuration semantics or any
client-brand policy. Those stay with the integration. No VIPTV-specific
behavior is added here.

## Design pin

The gateway has no user interface and no `DESIGN_REF`. Product-facing playback
policy (direct-first, conversion last) is defined in design's
PLAYBACK_CAPABILITIES.md and enforced by the integration; this repository
implements the generic mechanism.

## Current scope

Qualified: shared live HLS/TS ingest for the documented H.264/HEVC/MPEG-2 video
and AAC/MP2/MP3/AC3/EAC3 audio sets, private lossless Matroska replay for
verified FFV1/FLAC, and Linux SubRip shared cue carryover (issue #2). Progressive
live delivery is a same-container lossless remux for live MPEG-TS and for live
Matroska whose private replay is Matroska (issues #1 and #11, decision recorded
2026-09-30); every other progressive live request is rejected with
`unsupported_output`.

## Acceptance

- `cargo test --locked --workspace` passes; real media and container fixtures
  stay opt-in and separately documented.
- Every capability claim names its tested formats, transports and platforms;
  unsupported inputs fail explicitly.
- No raw source URLs, headers, credentials, media URLs or response bodies are
  logged or committed; inherited GPL-2.0-only notices are preserved.
- Remaining items in VALIDATION.md (admission hardening, upstream failure
  classification, remaining formats, crash containment, bounded shutdown,
  hardware/4K and backend integration) are complete before public deployment
  or client cutover, which each require separate approval.
