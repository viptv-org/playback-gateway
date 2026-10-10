# Gateway shared torrent integration

The opt-in gateway VOD torrent path uses the immutable Go worker named by
`TORRENT_RUNTIME_REF`. Android and desktop currently use that same source revision.
The Rust `torrent-runtime-host` crate owns bounded JSONL pipes, timeouts, child
termination/reaping and redacted errors. Gateway source resolution preserves
tracker/file/archive hints and exposes only a registered, exact private Range URI
to the existing FFmpeg inspection/planner. Source URLs never enter diagnostics.

The production Rust gateway excludes rqbit from its dependency graph. Its HTTP
archive ranges use Tokio task tracking and a separate bounded archive spool
owner; stored HTTP archives reserve no payload spool. Torrent archives use the
Go runtime's verified piece cache and seekable stored-RAR reader. The explicit
engine `torrent` feature retains v1 comparison/rollback code and inherited tests.
Startup holds the parent cache's legacy ownership lock and removes inactive
legacy payload/spool directories before opening Go. An active old owner prevents
migration; retained `shared-runtime-v2` records are preserved.

Sources share by namespace and full input identity. One identity's preparation
lock does not cover unrelated metadata waits. Output jobs retain their source
through real media-process cleanup; weak source caches cannot confer authority.
Cancellation during prepare retains admission until the late control reply's
handle is closed. Cache records survive stop/restart; the final handle stops
its torrent. Worker death can interrupt other inputs in that same worker; each
loses its private media authority and must start a fresh authorized session.

The worker's `first_frame` acknowledgement is sent when gateway output preparation
has succeeded. This is an internal source-consumption acknowledgement, not a
measurement of a web/Roku decoded frame. Client integration carries the original
startup budget through player opening and presentation; qualification must
report actual client frames separately. The gateway's direct/copy/remux/transcode
planner and public viewer authorization remain unchanged.

The separate [torrent startup v2 routes](TORRENT_STARTUP_V2.md) retain the Go
watchdog through decoder acknowledgement. V1 keeps its compatibility behavior.
Backend/web/Roku adoption is integrated; the v2 Go/FFmpeg fixture verifies that
output readiness and a legacy acknowledgement leave its handle awaiting a frame.

The production backend/API/controller and Go worker also passed an owned HTTPS
Mediabunny run: frame presentation preceded the scoped acknowledgement, and Stop
changed media to 401. Five controlled HTTPS UI cases passed, including withholding
frame evidence after metadata readiness. See [delivery status](TORRENT_DELIVERY_STATUS.md)
for exact scope and the separately coordinated physical-device qualification.

## Local qualification

Run `cargo test --workspace`, then
`GO=/path/to/go python3 scripts/qualify-gateway-torrents.py` with real FFmpeg and
ffprobe available (override `FFMPEG_PATH`/`FFPROBE_PATH` if needed).
The script builds test-only worker/seeder entrypoints, creates private temporary
wrappers and removes them after the Rust fixtures finish. Production platform
artifacts exclude these entrypoints and cannot allow private peers.

The owned fixtures exercise real Go tracker discovery and verified Range bytes,
FFmpeg HLS video conversion, compatible viewers sharing an output, independent
seek output, decoded-frame seek comparison, final teardown and a 256 KiB cache
with a substantially larger selected file. A separate pending metadata torrent
runs concurrently. Cache allocation is checked; no whole-file spool is created.
These are local controlled results, not public-swarm latency or physical Roku proof.

While adapting the inherited rqbit seeder fixture, valid fragmented TCP
handshakes were intermittently refused: the receiver parsed a 20-byte initial
read as a complete 68-byte handshake. Its debug log reported `NotEnoughData`.
The gateway correctness fixture now uses upstream anacrolix seeding and does
not patch the retired transport. Keep this rqbit interoperability observation
separate from performance measurements and from the cause of user-reported failures.

Performance is accepted for integration, with follow-up in
[TORRENT_PERFORMANCE_FOLLOWUP.md](TORRENT_PERFORMANCE_FOLLOWUP.md).
Linux/Windows native playback, actual web/Roku delivery and physical-device
qualification require their own evidence. No production rollout is implied.

## Authenticated HTTPS browser delivery, 2026-10-09

A fresh local gateway process used the production pinned worker, a disposable
integration key and an HTTPS media endpoint. The privately recorded original
failing H.264 source was selected with its explicit file index. The gateway
copied video and converted audio to AAC; it did not extract/download the whole
selected file. The initial playlist's media sequence was zero.

Chrome's native HLS/MSE path decoded 1920×800 frames. The production
`Html5FallbackAdapter` also opened the same delivery through Mediabunny/WebCodecs,
with actual canvas presentation and advancing time from zero. A replacement
output at 120 seconds was then opened by that production adapter; it presented
three destination frames with absolute player time 120.114 seconds. The complete
seek request/player replacement took 27.1 seconds in this successful run. An
earlier seek creation failed before diagnostic capture; it is not counted as a
successful trial and consistent seek reliability is not established.

Releasing the viewer returned 204 and immediately changed its media URL to 401.
All credentials, media URLs, source identities and screenshots remain private.
The owned gateway/HTTPS/browser processes were stopped. Warm preparation was
about 0.4 seconds after retained verified cache; the first cold gateway output
observation was 21.9 seconds. These are individual integration observations,
not matched latency comparisons. Browser startup used a test page around the
production player adapter; full backend account/source-screen admission was
not part of this probe. Physical Roku delivery remains separate.

## Measured progress and complete backend admission

Gateway preparations expose only a closed stage to the owning integration key
through `/v1/preparations/:idempotency_key`. Source progress is shared by actual
input identity; unrelated metadata waits remain concurrent. Observation records
are bounded and are removed on cancellation/completion. Reading stages confers
no playback authority and does not renew a lease.

The backend brokers those facts through the separate
`/api/v2/playback/:id/progress` route after account/profile/source and gateway
configuration validation. Web maps the stage through shared core's canonical
copy; Roku's scoped Task field updates its existing status label once per change.
Old playback session/negotiation envelopes are unchanged. Unknown stage values,
raw diagnostics and stale request owners never reach the status surface.

A subsequent trusted-HTTPS run exercised the real backend account/profile/vault,
registered add-on discovery, gateway admission, the production `TvApi`, shared
Rust/WASM normalization, `PlaybackSessionController` and Mediabunny presentation.
It showed Finding peers → Fetching metadata → Buffering from measured worker
states, then presented six frames with an advancing clock. Admission and player
opening took 18.37 seconds in that one cold observation. Stopping through the
production controller/backend immediately made its media URL return 401. No
production data or physical device was involved. The test page wrapped production
API/controller code; ordinary account login/catalog screens were not exercised.

Pending acquisition cancellation is now explicit. Backend Stop, request-ID
cancellation and its authority reaper cancel the owned gateway preparation,
including before its viewer ID exists. A scoped tombstone covers a cancellation
that races the create request's arrival. An actual HTTPS production
API/controller run was stopped during the measured peer/metadata stage; the
Go preparation settled and restored all two input/output slots. No ready media
was exposed. Older gateways still receive late-viewer release reconciliation.
Controlled scope and cancellation-before-admission tests accompany this path.

The resolved file/archive tuple is retained in the shared cache independently of
live authority. Piece, metainfo, routing and selection records share one bounded
accounting/validation model; selection records do not count as downloaded payload
progress. Native artifacts and this gateway worker adopt the same source revision.

## Production transport dependency qualification

The normal `cargo check --locked -p playback-gateway` and
`cargo tree -p playback-gateway --edges normal` exclude every rqbit crate.
Engine tests with normal features exercise token-protected HEAD/Range bytes,
revocation and joined listener shutdown. HTTP task retirement must also wait for
captured IO destruction; a timed-out receipt stays failed after late completion.
Archive reservations retain their bytes and exclusive directory owner until
blocking readers finish, and reclaim abandoned spools after restart. Workspace
tests retain the inherited archive cancellation and legacy transport checks.
