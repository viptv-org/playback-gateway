# Gateway shared torrent integration

The opt-in gateway VOD torrent path uses the immutable Go worker named by
`TORRENT_RUNTIME_REF`. Android and desktop currently use that same source revision.
The Rust `torrent-runtime-host` crate owns bounded JSONL pipes, timeouts, child
termination/reaping and redacted errors. Gateway source resolution preserves
tracker/file/archive hints and exposes only a registered, exact private Range URI
to the existing FFmpeg inspection/planner. Source URLs never enter diagnostics.

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
