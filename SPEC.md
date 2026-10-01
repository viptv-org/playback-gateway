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
delivery is rejected with `unsupported_output`; issue #1 implements it as
same-container lossless remux (decision recorded on that issue, 2026-09-30).

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
