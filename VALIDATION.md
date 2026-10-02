# Validation checkpoint — 2026-10-02

## Joined Matroska progressive delivery (#11)

Live Matroska progressive viewers now consume a joined private replay: fresh
headers once and only media Clusters from subsequent segments. Same-container
eligibility is explicit; other live containers, encoding, selected subtitles,
seeking and ranges remain unsupported. Shared HLS Matroska readers retain their
existing on-disk replay path.

An isolated pre-torrent stage passes fmt, all-target Clippy with warnings denied,
and the workspace suite: 105 engine, 28 control and one main binary test,
45 real-media tests ignored by default. All five real Matroska worker fixtures
pass together in 78.35 seconds, including existing HLS sharing/budget/teardown
and both FFV1/FLAC and H264/FLAC progressive cases. MPEG-TS progressive passes in
30.28 seconds; the exact FFmpeg join-reader test passes in 0.67 seconds with no
backward timestamps, full segment coverage and late keyframe decode.

Runtime source `0382030`, freshly built with Rust build jobs limited to two,
produces local image
`sha256:48be718a12d4e3641b38f28de76391f040c05a790fa7de45edd04c434ac82d6f`.
All five image modes (`matroska`, `matroska-tls`, `bounded-matroska`, `ts` and
`bounded`) pass using fixture/script source `133fa4a`. This qualifies actual
Matroska delivery over HTTP and
verified fixture HTTPS, independent viewer/namespace access, slow-reader
failure with an aborted response and final process/upstream/storage/reservation
reaping. Actual UID10001 Matroska ENOSPC under a 2-MiB media tmpfs fails the
viewer with a transport error, reclaims capacity and permits fresh playback.
See [progressive qualification](docs/PROGRESSIVE_QUALIFICATION.md) for exact
modes, timings, image provenance and limits. No deployment is claimed.

The fixture corrections preserve the real contract: repeating a short complete
Matroska file introduced artificial headers/timestamp resets, and unrenewed
60-second API leases expired before the low-rate image fixture filled its
socket. Native sources now send one long container; the eviction scenario
renews active viewers explicitly and stops renewing on release. Media reads
still do not renew leases, and the separate real expiry fixture is unchanged.

## Opt-in torrent/archive and native library checkpoint

BitTorrent v1 inputs, selected payload metadata, private Range transport, stored
multipart RAR views and complete compressed-member spools are implemented.
Runtime support defaults off. HTTP tracker stripping, private/webseed refusal,
per-namespace sessions and conservative full-payload cache reservations are
documented adaptations to the pinned librqbit API. Metainfo/direct stored-RAR
HTTP still uses the vetted egress proxy; public viewer authorization and the
existing output planner remain the delivery boundary.

The final source gate passes fmt, all-target strict Clippy and 162 default
tests: 109 engine tests, 10 torrent boundary tests, 40 control tests, two startup
tests and one native facade fixture. Fifty real-media/service tests remain
opt-in. Real local-seeder/FFmpeg fixtures separately pass shared viewers/late
join, decoded 3-second seek, stored four-volume archive HLS with no spool, one
checksum-verified compressed spool, independent lifecycle and cleanup.

Review-driven regressions cover admission before source I/O, atomic promotion,
cancellation while blocking extraction owns its directory/budget, actual
blocking Range readers, delayed cleanup retaining key quota, raw HTTP reader
abort after the last public handle, URI alias preservation and cache ownership
until stopped readers settle. Repeated failed metadata namespaces are reaped.
The optional peer-port parser accepts Compose's empty default and rejects
invalid nonempty ports. Generated Range roots are registered by exact URI with
scoped lifetime guards; probing retains the vetted egress proxy for nested HTTP
resources. A real selected-playlist regression records zero private-child
contacts and a redacted failure; ordinary torrent/archive delivery still passes.
This is destination/format-specific evidence, not a general demuxer sandbox.

UniFFI Kotlin bindings are generated and host-tested. All four Android ABI
libraries were rebuilt after the final external-owner/read-settler changes and
pass independent ELF LOAD alignment checks of at least 16 KiB. The native
facade has no axum control/media execution on its selected feature path and
does not yet unpack archives. No device, APK ZIP-alignment or store claim follows.

Fresh local runtime image source `da619d85a4d438b250db27415989a8b5a52e07ee`
produces image
`sha256:3ed14fbbf179d9d4a326b6e798118ad62c209f01a7fdf3d0524233a52e367825`.
All five committed-source torrent/archive modes pass using fixture/script source
`9217d34f1147250578bb6dc1b105de2e55316e75`. That follow-up changes only fixture
directory setup and generated Kotlin EOF normalization; engine/control Rust,
dependency pins and the runtime Dockerfile are unchanged from the image source.

| Mode | Exact fixture result | Test execution time |
| --- | --- | --- |
| `torrent` | one passed, zero failed | 0.49 s |
| `stored-rar` | one passed, zero failed | 0.42 s |
| `compressed-rar` | one passed, zero failed | 0.43 s |
| `bounded-archive` | one passed, zero failed | 0.01 s |
| `egress` | one passed, zero failed | 0.18 s |

The same image and fixture revision also pass all five required inherited
progressive modes, one exact test per mode:

| Inherited progressive mode | Exact fixture result | Test execution time |
| --- | --- | --- |
| `matroska` | one passed, zero failed | 138.04 s |
| `matroska-tls` | one passed, zero failed | 137.91 s |
| `bounded-matroska` | one passed, zero failed | 20.12 s |
| `ts` | one passed, zero failed | 88.76 s |
| `bounded` | one passed, zero failed | 25.13 s |

These inherited fixtures preserve actual progressive HTTP/fixture-HTTPS
delivery, sharing/late join, slow-reader eviction, isolation and cleanup.
Both bounded modes observe actual UID10001 media ENOSPC at 2,097,152 allocated
bytes, a failed viewer transport and final process/upstream/reservation recovery.
Their network-none runtime setup and limited capabilities remain those in
`scripts/check-progressive-container.sh`. These are inherited live-media checks;
they do not establish ready-viewer torrent-cache exhaustion.

Each torrent/archive mode executes exactly once as UID10001 with network none, read-only root,
no Linux capabilities, two CPUs and bounded memory. The bounded mode fills the
actual 2-MiB archive-cache tmpfs, observes `processing_failed`, removes the partial
spool and successfully extracts after space is reclaimed. This is preparation
ENOSPC evidence, not a previously ready viewer's transport-error qualification.

The first stored-RAR image run exposed a fixture packaging defect: the compiled
`/src/control/../engine/assets` path required its `/src/control` parent even though
the media assets were present. The fixture build now creates that parent as root
and immediately restores UID10001; the full five-mode rerun above passes. On this
sudo-only host a PATH-visible Docker wrapper also preserves Docker access through
`timeout`; a shell function alone cannot wrap that executable invocation. Git
retains the repository user's identity and global trust settings are unchanged.

The torrent/archive results are Workers-library/image-FFmpeg qualification;
they do not qualify torrent-specific public service-binary/API paths.
Trusted-HTTPS browser playback, ready-viewer cache exhaustion,
sustained peer/crash workloads and client integration remain separate. See
[torrent qualification](docs/TORRENT_QUALIFICATION.md) and
[provenance](PROVENANCE.md) for the precise scope and licensing restrictions.

## Bounded shared SubRip cue carryover

Linux SubRip-bearing live HLS inputs now use the private Matroska replay with
deterministic verified source/local track maps and one decoded, bounded cue
cache per ingest. Original absolute source identities survive; selected captions
report actual WebVTT rendition stream zero. Shared AV retains the absolute cue
clock. Active long overlapping cues survive original-segment eviction, including
an initially caption-disabled output and late enabled output. Live control
position remains zero; retained-window decoder seeking is qualified separately.

Exact runtime image
`sha256:5efbd97e78573879a696f4bd3c4c22165c3a65e500bf5695e1cb4881ca95eed8`
passed public HTTP identity/demux, long overlap/late activation/eviction/real
frame-seek, independent release/real60s expiry and actual PID/upstream/storage
teardown. A real source exceeds the 1-MiB decoded cue cache budget and receives
safe processing failure with admission reclamation. Actual UID10001/caps0
SubRip replay under a 2-MiB kernel mount passed ENOSPC/reclaim/key persistence
and fresh restart. Trusted HTTPS fixture qualification is recorded with the
completed checks in [shared subtitles](docs/SHARED_SUBTITLES.md).

The default workspace suite passes 104 tests/36 opt-in ignored and strict
all-target Clippy. ASS/SSA/WebVTT, bitmap/teletext, other unqualified source
formats and non-Linux subtitle replay retain independent paths; no silent
discard, new live origin, UI, public deployment or native renderer is claimed.
Later mechanical formatting passes scoped checks for all changed Rust files;
the workspace formatter still reports five untouched baseline files listed in
the qualification document. The exact image proof above predates that
formatting-only commit; runtime behavior did not change.

## Supported Docker kernel-bounded media spool

The supplied Compose deployment now mounts DATA_DIR/media on its own 256 MiB
tmpfs, with UID/GID 10001, mode 0700, noexec/nosuid/nodev; /tmp remains separately
bounded at 64 MiB. Credential SQLite files and instance.lock remain persistent on
the named parent volume. No runtime/public v1 media-path API changed. Native,
custom DATA_DIR and Docker runs without that nested mount do not inherit this
bound. MEDIA_SPOOL_SIZE is operator Compose interpolation, not a client field.

The fixed 64 MiB logical replay watchdog is replaced by LIVE_REPLAY_MAX_BYTES,
operator default 256 MiB, minimum 1 MiB. The library also accepts an explicit
resource budget. This sampled logical-file budget is independent of the kernel
allocation limit; operators must tune both for bitrate/keyframe/concurrency.
Sustained high-bitrate 4K/long-GOP qualification remains open, not inferred from
the earlier small 4K copy fixture.

A real 2 MiB mount fixture passes: actual service UID/GID 10001/caps 0, sparse 1 GiB
logical length distinguished from allocated bytes, active real ingest/output,
kernel ENOSPC and exact allocation cap, readiness 503, no fake playback on a denied
start, safe processing_failed on the affected viewer, process/upstream teardown,
reservation reclamation, successful playback after reclaiming space, persistent
credential/lock files and authenticated fresh playback after restart. The fixture
controller has only its explicitly documented isolated/cross-UID test privileges;
they are not service/deployment privileges. The non-root envelope also passed
with the nested mount. Final committed-image evidence is recorded separately.
The committed eb66916 image is
`sha256:9258fd82bd1dff31b1739e03ade1e7706ec0d699971ea45c9887980b0b8c8b61`.
Both committed-source scripts passed against that exact image, including the
non-root key-persistence/media-disappearance Docker restart checks. The default
suite passes 99 tests with 28 opt-in ignored; strict all-target Clippy passes.

See [bounded media](docs/BOUNDED_MEDIA.md) for the complete write-path audit,
kernel allocation versus logical size/RSS/swap distinctions, tuning, exact
qualification and read-only backup/inspection of old media hidden beneath the
new mount. Existing hidden files are neither deleted nor counted by the child
mount; no production upgrade or data deletion occurred.

## Service/container shutdown qualification

The runtime now bounds graceful HTTP draining at five seconds before terminating
owned media workers. Linux media commands arm parent-death SIGKILL, including
network ingestion, local output readers, inspection and capability checks.
Public v1 JSON is unchanged. The locked workspace suite passes 99 tests with 27
opt-in fixtures ignored, and strict all-target Clippy passes.

Actual a47 service/container regression: an unread large generated-file response
kept SIGTERM pending beyond the eight-second fixture deadline while one real
ingest and two outputs existed. The corrected runtime passes this fixture,
including child reaping, upstream closure, same-storage restart, persistent key
authentication, rejection of old viewers and fresh playback. The separate
SIGKILL/restart fixture passes too; a47 already eventually passed that five-second
check, so Linux parent-death handling is recorded as additional hardening.

The original current-source image was built locally as `qualification-a47c25c`,
content ID `1fbc1c6ffac5`; corrected working-image qualification used `d57b9487786c`.
The actual non-root/read-only envelope passed readiness, Docker health, effective
zero capabilities, no-new-privileges, private volume mode, duplicate owner lock
rejection, SIGTERM and restart. Its stopped fixture volume was archived privately
before removal. Named HTTP/H264-HLS sharing fixtures also passed against exact
FFmpeg 9.0.2 binaries and Debian runtime libraries extracted from `1fbc1c6ffac5`.

See [container qualification](docs/CONTAINER_QUALIFICATION.md) for reproducible
committed-source scripts, exact fixture semantics, isolated public-address setup,
recoverable cleanup and unqualified cases. Final reviewed-source image content
IDs are reported separately after rebuilding; no image push/deployment is claimed.
The committed `f69adb0` runtime was subsequently rebuilt from a Git archive as
`sha256:3f2a6affeba84ad8e2ec56fb0ecc18c4c53cf93bd73048726d6aa9e1f17f7ada`.
Both committed-source qualification scripts passed against that exact image,
including all three named real service fixtures and the separate non-root
envelope. The corresponding stopped fixture volume was privately archived before
removal. This documentation-only evidence record does not change that runtime.
This does not close universal sharing or the hard storage-bound gate.

## Physical input accounting, trusted TLS and small 4K copy

Inspected plans now carry an internal `shared_ingest` execution fact, skipped by
serialization. Only eligible live replay plans deduplicate input admission across
different output plans. Fallback execution and VOD bind input admission to the
normalized output plan as well as namespace/source/credential identity. Identical
fallback plans still join one existing output; distinct physical pipelines consume
distinct input reservations. The public v1 request/response JSON is unchanged.

The locked workspace suite passes 98 tests, with 25 opt-in fixtures ignored;
strict all-target Clippy passes. Engine fixtures explicitly decline shared replay
for subtitle-bearing sources, unlisted codecs and progressive live requests.
Ledger fixtures prove fallback input limits remain charged while stopping and
restore only after confirmed teardown. Three additional real fixtures pass:

- An isolated HTTP MPEG2 source: identical plans use one job/upstream; a smaller
  normalized output creates another connection and consumes a second input. A
  third plan is denied with `input_capacity`. Reaping each released job closes
  its real upstream connection before its capacity becomes available.
- Trusted HTTPS HLS and plain-HTTP HLS containing HTTPS children: differing
  output plans use one remote-reading FFmpeg ingest per cohort, and fetched
  encoded output decodes. Untrusted root certificates, untrusted HTTPS children
  and trusted-but-wrong root hostnames fail with the safe public code
  `source_preparation_failed`, without allocating a viewer job. The existing
  scoped egress proxy remains active for every root/child connection.
- A software-only 3840x2160 H264 source: four constant-color frames at 1 fps,
  generated with two encoder threads. Shared copy delivery preserves actual
  3840x2160 demux dimensions, and one fetched output frame decodes. Declared 8K
  headroom normalizes to the same 4K job without another source connection. This
  qualifies small 4K demux/copy, not 4K encoding, hardware, native renderers, HDR,
  sustained high bitrate or visual parity.

The TLS fixture generates its own one-day localhost CA/key with openssl and uses
a threaded Python standard-library TLS server for concurrent HLS resources. On
the tested Linux/GnuTLS installation, a root `ca_file` option did not establish
trust for nested HLS children. The fixture therefore supplies its anchor and CA
bundle only in per-process bwrap mount namespaces, keeps verification enabled,
uses `--die-with-parent`, and verifies the host trust bundle is unchanged. No host
CA installation, production trust change or public network access occurs. This
fixture requires python3, openssl, bwrap, user namespaces and the documented Linux
trust-store paths in its executable wrapper; it is not a new runtime dependency.
[FFmpeg's TLS documentation](https://ffmpeg.org/ffmpeg-all.html#tls) describes CA
files and default verification, including hostname verification with GnuTLS.

Universal input sharing remains unfinished: subtitle-bearing sources, other
codecs and progressive live transports still execute independently. Truthful
quota charging makes that limitation explicit; it does not replace their future
shared transport. These fixtures do not qualify stalled direct-response drains,
all redirects/DNS/keys/subtitles, native devices or a current Docker image.

## Historical hard storage audit before the supported Docker mount

FFmpeg writes replay segments, partial files and manifests directly. Sampling
their sizes after writes cannot enforce a hard byte bound: writes can exceed the
threshold between polls, while scans run or while scheduling/I/O stalls. Segment
count and duration bounds do not bound bytes or keyframe-starved partial files.
The replay watchdog also does not cover separate output workers or every file in
the shared data volume. No maximum overshoot is certified.

A hard bound requires either an independently tested filesystem/volume quota
covering every write path (including partial/orphan files and separate outputs),
or a transport in which the gateway owns writes and reserves bytes before they
are admitted to a bounded replay. Both require defined full-capacity behavior,
slow-reader eviction/backpressure, timeline/metadata preservation, and teardown
evidence. A per-file process limit or an unqualified FFmpeg CLI flag is not proof
of a total HLS spool bound. No host filesystem quota was configured here. This
remained an acceptance gate at that historical checkpoint. The supported Docker
child tmpfs implementation and its exact limits are recorded above; native/custom
unbounded-volume guarantees and gateway-owned universal replay remain open.

## Shared live ingest implementation

An engine-owned ingest now survives independently of individual output workers.
Its identity includes authorized namespace, source URL, normalized credential
headers and the internal egress route. A single-flight registry retains stopping
generation tombstones until process reap; separate ingest/output slots and
cleanup tasks preserve admission during cancellation. A dead ingest fails every
dependent output; a dead output releases its own slot while the ingest continues.
Integration keys authorized for the same namespace retain intentional sharing.
VOD origins and the public v1 JSON protocol remain unchanged.

The copied replay contains the first real H.264/HEVC video and all eligible audio
tracks in deterministic order. A local-only ffprobe inspection checks replay
indices, codecs, dimensions and channels before outputs start. Selected-track
responses retain original source indices, while FFmpeg maps verified local
indices. Only the ingest can use HTTP(S); output readers are restricted to the
private generated file playlist and segments. It retains 12 keyframe segments
plus three segments of deletion grace. Startup/running storage checks stop the
ingest above 64 MiB, including partial segment files. This sampled watchdog may
overshoot and is not a filesystem hard quota. Late readers can fail if they fall
behind the retained replay; the other outputs continue.

With installed /usr/bin/ffmpeg and /usr/bin/ffprobe, three opt-in fixtures pass:

- A continuous plain-HTTP MPEG-TS source: five compatible viewers share a remux
  output; a late 160x90 encoded output creates another job without increasing
  source connections or source-open count. Releasing the complete original output
  leaves the encoded output alive and its fetched segment decodes. A separate
  namespace creates another input, and final releases close both inputs.
- A plain-HTTP rolling HLS source with two AAC audio tracks: different output
  plans share one ingest. After initial inspection, adding another output neither
  refetches retained provider segments nor fetches new segments more than once;
  original audio index 2 remains selected, and the second output survives release
  of the first. Decoded output contains track 2's 880Hz tone rather than track 1's
  440Hz tone. This exercises HTTP child playlists/segments through the proxy.
- A plain-HTTP HEVC Main/AAC HLS source: native-compatible output retains HEVC
  copy and its fetched local playlist decodes; a smaller H264 encoded output
  shares the same ingest without duplicate provider segment requests. Selected
  audio content is also decoded and checked. The fixture bounds libx265's pool
  and frame threads; it does not certify a native device renderer or hardware.

The locked workspace suite passes 95 tests, with 22 opt-in fixtures ignored;
strict all-target Clippy passes. Unit fixtures cover sparse stream indices,
ineligible subtitles/codecs, partial-file storage accounting, cancelled startup,
ingest crash affecting all outputs, and individual output crash preserving the
other output and releasing only its output reservation.
An inspected-plan regression also preserves the existing native HEVC HLS copy
gate when original delivery is replaced by shared replay; this is not real HEVC
decoder qualification.

This is compatible AV replay, not complete all-format single-ingest acceptance.
Subtitle-bearing sources, unlisted video/audio codecs and progressive live paths
still use independent original execution. HEVC Main10/HDR and MP2/MP3/AC3/EAC3
use the same seam but need new real fixtures. HTTPS and small 4K copy qualification
are recorded above; inherited TLS/SSRF fixtures do not certify every replay path.
Real 4K conversion/hardware, keyframe-starved/high-bitrate storage,
long-lived slow-reader behavior, host crash/process containment and a current
Docker image remain acceptance gaps. No public deployment or client cutover is
claimed by these local tests.

## Conversion and track contract extension

VOD input admission now includes the inspected plan's normalized timeline origin.
Different seeks consume different input reservations, including while the prior
worker is stopping; admission is released only after confirmed teardown. Focused
ledger fixtures cover that boundary, different live output plans sharing input
admission, and source header credential changes reserving distinct inputs. These
fixtures verify accounting only; physical ingest sharing is covered separately
above. All-format multi-output single-ingest remains incomplete, and no new
container/device qualification is claimed. At that checkpoint the full
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
- Disguised HLS ingest (playback-gateway#9): with FFmpeg/ffprobe n9.0.2, real
  fixtures serve h264/aac segments behind real JPEG, GIF (leading `G`), PNG and
  CSS prefixes with `.jpg`/`.gif`/`.png`/`.css`/no-extension names and lying
  types. The probe finds h264 and aac through the loopback sanitizer, and
  `-map 0 -c copy` through it demuxes exactly as many packets as the clean
  source; a live window (no `ENDLIST`, advancing media sequence) reloads
  through loopback with the same exact count, and JPEG-prefixed `.ts`
  segments demux completely. Unit tests prove token-only access (wrong,
  shortened, absent tokens are 404), refusal after drop, refusal of private
  cross-origin children, sanitized arguments without `-headers`,
  `-http_proxy` or `-extension_picky` and with `http,tcp,crypto`, redaction of
  the loopback URL and token, and selection only for refusal/image signals.
  FFmpeg before 7.1 (image-only probes) is covered by selection tests, not a
  real old binary.
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
all-format/redirect/DNS network enforcement, the remaining single-ingest formats,
crash/process containment, bounded shutdown with active/stalled media responses,
real hardware/4K qualification and backend integration.
The existing continuous fMP4 transport is single-consumer and is not advertised
as a shareable gateway output. HTTP/TLS checks above cover their named fixtures,
not every decoder, demuxer, media format or hostile upstream behavior.

Run native opt-in checks with PLAYBACK_TEST_FFMPEG and PLAYBACK_TEST_FFPROBE
pointing to qualified binaries. TLS fixtures additionally require openssl.
Source URLs, headers, API keys, media URLs and test/production databases must never
be committed. Fixture loopback exceptions are compiled only under cfg(test).
