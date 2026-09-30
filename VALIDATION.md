# Validation checkpoint — 2026-09-29

## Conversion and track contract extension

VOD input admission now includes the inspected plan's normalized timeline origin.
Different seeks consume different input reservations, including while the prior
worker is stopping; admission is released only after confirmed teardown. Focused
ledger fixtures cover that boundary, different live output plans sharing input
admission, and source header credential changes reserving distinct inputs. These
fixtures verify accounting only: live output workers still open separate source
connections when their normalized plans differ. Multi-output single-ingest remains
incomplete, and no new media/container/device qualification is claimed. The full
locked workspace suite passes: 89 passed, 20 opt-in fixtures ignored.

Generic output conversion now exposes auto/audio/video/audio_video; the worker
passes its force/reason pair to the existing inspected planner. Explicit tracks,
language preferences and subtitle-off validation reach that same planner.
Bounded schema fixtures cover defaults, supported conversion choices, 4K facts,
invalid language/index values and conflicting subtitle choices. Full workspace:
87 passed, 20 opt-in fixtures ignored; strict all-target Clippy passed. A prior
collapsible-if warning in the reaper was corrected without changing its condition.
No new real-codec/hardware qualification or Docker image build is claimed here;
cross-output ingest sharing and other open acceptance requirements remain open.

## Verified locally

- cargo test --workspace --locked: 66 engine tests, 20 control/API tests and one
  service-storage test pass. Eighteen engine fixtures, one real-worker fixture
  and one standalone-service fixture are opt-in and excluded from that count.
- HTTP route tests cover key issuance, redacted listing, revocation, operation
  scopes, foreign session IDs, stable idempotent media URLs, viewer-only media
  credentials, child segments, HEAD, renewal/release, readiness, bounded JSON
  bodies and no secret echo. Credential-store reads use blocking-task offload.
- Engine preparation exposes an inspected output plan before starting a worker.
  Equivalent capability headroom shares a plan. The control bridge uses the
  engine plan directly and keeps quota until cancellation cleanup completes.
- Original delivery accepts 4K/8K within declared dimensions without product
  quality clamps. These are plan tests, not a complete real 4K encoder audit.
- With installed FFmpeg/ffprobe, the real remux/transcode/cleanup fixture passes
  and verifies prepared-plan versus started-response consistency.
- The real HTTP live fixture proves five compatible viewers share one active
  upstream connection; adding the next four viewers opens no additional source
  connection. A distinct authorization namespace creates another input. Releasing
  one viewer preserves the others; releasing all viewers closes their input.
- Real subprocess fixtures exercise proxy use for root URLs, redirects, HLS
  segments and AES keys, including hostile inherited proxy variables. Raw TCP/TLS
  HLS segment attempts do not contact the protected destination.
- Real TLS fixtures accept a matching, explicitly trusted HTTPS media source,
  reject untrusted certificates and trusted-but-wrong hostnames, and reject
  untrusted HTTPS segments nested inside plain-HTTP HLS. Both FFmpeg and ffprobe
  are exercised. The same TLS fixture also passes with the container's binaries.
- The standalone binary serves health/readiness over TCP, rejects unauthenticated
  control access, refuses a second owner of its data directory, handles SIGTERM
  and releases the OS-held storage lock. Existing data directories must be private.
- Docker image playback-gateway:development built successfully (local image
  2551a1586f24). FFmpeg 9.0.2's release signature is verified during the build.
  The image reports healthy as UID/GID 10001 with a read-only root, dropped Linux
  capabilities and no-new-privileges. Temporary smoke containers/data were removed.
  The real HTTP five-viewer fixture also passes using this image's media binaries.

HTTP IPTV inputs remain supported. Public gateway endpoints require operator
HTTPS ingress; the HTTP provider hop is not encrypted by the gateway.
The service rejects media binaries that do not enable TLS verification by
default or do not use the currently qualified GnuTLS backend. An unconditional
TLS-verification input option was rejected during testing because it breaks plain
HTTP inputs; default-enabled verification avoids that regression.

## Remaining acceptance work

Capability responses now expose the authenticated key's operation scopes so an
integrator can validate permissions without starting media. The 86-test default
suite passes with scope assertions. The backend's opt-in executable integration
test verifies issuance/registration/check against the current native service.
The earlier image 2551a1586f24 predates this metadata addition and must be rebuilt
before using it with the new backend registration flow; it is not a current-image
deployment claim. A subsequent local rebuild, f215fa924b8f, includes scope and
per-key available-capacity reporting. The backend's isolated real-media fixture
passed with that image: creation, HLS/segment delivery, renewal and release,
without backend embedded execution. The fixture uses network none and a
container-only address alias; no production policy override or deployment occurred.

This is not approval for public deployment or VIPTV cutover. Pending work includes
pending-inspection quota/rate admission, detailed upstream failure classification,
all-format/redirect/DNS network enforcement, multi-output single-input fan-out,
crash/process containment, bounded shutdown with active/stalled media responses,
real hardware/4K qualification and backend integration.
The existing continuous fMP4 transport is single-consumer and is not advertised
as a shareable gateway output. HTTP/TLS checks above cover their named fixtures,
not every decoder, demuxer, media format or hostile upstream behavior.

Run native opt-in checks with PLAYBACK_TEST_FFMPEG and PLAYBACK_TEST_FFPROBE
pointing to qualified binaries. TLS fixtures additionally require openssl.
Source URLs, headers, API keys, media URLs and test/production databases must never
be committed. Fixture loopback exceptions are compiled only under cfg(test).
