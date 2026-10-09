# Qualification ledger

Status on 2026-10-09: **client integration authorized; performance parity deferred**.
The approved replacement is specified in design's
`specs/behavior/torrent-runtime-v2.md` at `16277ca`. The owner accepted current
performance for integration; remaining work is recorded in
[performance follow-up](../docs/TORRENT_PERFORMANCE_FOLLOWUP.md). The existing rqbit transport
remains in production paths and available for comparison/rollback. Header
animation branches are unrelated and preserved.

## Foundation revision after controlled failure

The first prototype used v1.61.0. Repeated controlled reads exposed a lost
request wake-up: the peer writer captured its notification channel after
checking for work, and could miss a request arriving in between. A deterministic
upstream-level test reproduced the failure in 200 ms. Upstream revision
`d913b30f520e10e85a94c7dc4b7b3e828b22c4b2` already fixes that ordering
([writer source](https://github.com/anacrolix/torrent/blob/d913b30f520e10e85a94c7dc4b7b3e828b22c4b2/peer-conn-msg-writer.go)).
The module now pins that exact revision through its Go pseudo-version and
requires Go 1.25+. One hundred repetitions of the previously failing owned-peer
test, each issuing fifty successive ranges, passed under the race detector.
The custom cancellable reader remains necessary; the stock capped-reader
recursive-cancellation problem is separate and persists at this revision.

The public-swarm timings below and initial platform build observations used
v1.61.0. Repeat them against the new pin before claiming a performance result
or platform qualification.

## Measured public-swarm startup

Two privately supplied sources are called A and B here. All numbers below are
seconds from source admission through a completed one-frame FFmpeg command,
excluding engine launch. These are not app-rendered first-frame measurements.
Cold trials used new private caches, largest-file selection, no preliminary
range reads, no bandwidth cap and serialized rootless network namespaces with
inbound forwarding disabled. Live swarm conditions cannot be made identical
across trials. Three repetitions per cell are small samples, not percentiles.

| Engine/profile | A cold median | B cold median | Repetitions per source |
| --- | ---: | ---: | ---: |
| Current official Stremio, normal defaults | 2.040 | 2.217 | 3 |
| Candidate, upstream connection defaults | 11.347 | 14.108 | 3 |
| Candidate, dial burst 50 + head/tail priming | 22.347 | 4.141 | 3 |
| Candidate, above + 4 s nominal / 1 s minimum dial timeout | 6.441 | 3.911 | 3 |
| Candidate, above + IPv4 only | 6.618 | 6.363 | 3 |

All listed candidate trials decoded successfully. Faster isolated results were
not accepted as a win when repetitions regressed. IPv4-only tuning does not
justify disabling IPv6. Current defaults have not been replaced by these
experimental profiles.

After pinning the upstream wake-up fix, six further cold runs (three per source)
with the same fast-dial/priming profile all decoded. A had median **6.374 s**
(range 4.765–6.954 s); B had median **4.161 s** (3.602–5.092 s). These do not
establish latency parity with Stremio. The dependency fix has controlled
correctness evidence, not a demonstrated public-swarm speed improvement.

Single subsequent cold, paced-decoder trials consumed sixty media seconds on
each source. A/B reached first decoded progress at 5.325/4.258 seconds; both
completed 1,439 decoded frames, with maximum progress-report gaps of 250.2 ms
and no gap exceeding one second. Final cache charges were 174.2/169.7 MB under
the 2 GiB limit. This probe uses FFmpeg `-readrate 1`, video decoding and no
audio; it is not an application player-buffer or displayed-frame measurement,
and does not replace repeated stalls/seek/device qualification.

Stremio same-process warm medians were 0.201/0.199 seconds. Separate restart
trials with retained cache took 1.594/3.011 seconds (one each). Empty cold cache
and its actual configured path were checked independently: the approximately
two-second cold result is **not explained by reuse of the user's Stremio cache**.
The official download was `v4.21.1`, reporting runtime version `4.21.0`, SHA-256
`405eb494d6708406a30e716c3cfb5abae7a5e9c7a8b79446d64c3f821385930f`.

This is not an equal-integrity comparison: Stremio's virtual 512 KiB chunks can
be served before complete original-piece verification; the candidate verifies
entire pieces (2 MiB for A, 8 MiB for B). The approved verified-read requirement
is retained. Startup observations alone say nothing about first-minute stalls.

Released NuvioTV's TorrServer MatriX.141 sibling on Linux produced A/B totals
3.767/7.800 seconds with its responsive default and 3.611/9.940 seconds with
responsive mode disabled. Those are **single observations**, not repeated
medians or Android results. Source inspection shows the disabled mode uses the
whole-piece reader, but corrupt-piece tests remain necessary to qualify it.

On an earlier privately recorded failing source, maintained rqbit reached its
30-second timeout; candidate acquisition took 40.419 seconds before range and
decoder probes succeeded. Installed older Stremio and Nuvio engine probes both
timed out under the earlier comparison configuration. Those runs do not model
current Stremio's normal defaults and do not support an engine ranking. An
interrupted upstream rqbit trial is not recorded as a failure.

## Integrity-matched official reference

The shipped official server exposes `{"virtual":false}` on first hash-based
creation. An independent owned 2 MiB corrupt-piece fixture proved that mode
withholds all bytes until original-piece verification and recovers with a valid
piece; the unchanged default exposed the corrupt prefix. No official source
patch was used. See [source and fixture proof](../docs/STREMIO_GO_REFERENCE.md#follow-up-supported-integrity-comparison).

Six serialized cold runs (three per source), fresh private caches, normal
tracker/DHT inventory and the same one-frame command yielded:

| Reference | A median/range | B median/range |
| --- | --- | --- |
| Official whole-piece mode | 2.789 / 2.711–2.978 s | 7.083 / 5.868–10.180 s |

Default-mode and whole-piece-mode results describe different integrity
behavior; neither may be substituted for the other. These small live samples
show no uniform engine winner: the candidate is slower on A and faster on B
under the whole-piece mode. The comparative gate remains unpassed.

Additional candidate experiments retained the verified-byte rule. Plaintext-first
plus fast dial/priming produced A/B medians 6.196/6.509 s (three each), so it is
not adopted. A separate TCP-only diagnostic produced 3.739/5.973 s; disabling
uTP is not a production proposal. A same-socket 150 ms TCP-head-start experiment
kept uTP and DHT enabled. Three cold runs per source produced A/B medians
3.417/6.347 s (ranges 3.113–7.775 / 2.620–6.804 s). The uneven results do not
justify changing production defaults; the private hook remains qualification-only.

## Controlled correctness evidence

After the dependency and review fixes, `go test -race -p 1 -timeout 90s ./...`
passed locally (9.107 seconds for the runtime package). It covers:

- Owned TCP and uTP peers, bounded rolling reads and seek/re-download.
- Tracker-only and DHT-only magnets resolve metadata and verified bytes, with
  competing discovery disabled; ten repetitions passed under the race detector.
- A raw owned wire peer cannot expose a partial prefix, a complete corrupt piece
  is refused, and a distinct healthy peer recovers the waiting reader. Dropped
  handshakes and unreachable metadata cancellation settle.
- Encrypted RAR4 headers and malformed archives receive specific refusals.
- Eviction revokes live engine completion before unlink, retains its charge,
  rechecks concurrent allocations, and fences retirement and unlink failures.
  The completion callback waits through upstream publication before invalidating.
- A brief TCP head start still permits outgoing uTP-only verified playback.
- Resolved archive-member selection is reported and cannot be mutated by callers.
- Shared-boundary pieces, exact selected-file ranges, suffix ranges and largest
  selection ties.
- Offline reuse of verified bytes and metadata after restart.
- Partial-slot reclamation after seeks and protection during verification.
- A deterministic allocation-pressure test through the gap between hash-reader
  close and completion publication; it fails before the retained verification
  pin and passes afterward.
- Replaying a cached range does not refresh startup progress; new verified
  ranges do. These are distinct from progress made by another handle.
- Process death with an incomplete write: exclusive ownership is recovered,
  committed data remains readable and incomplete bytes are discarded.
- Stored RAR/RAR5 and multipart fixture reads, missing-volume and compressed
  format refusals.
- Independent authority, cancellation, renewal refusal after revocation,
  suspension-aware expiry and the startup deadline through first frame.
- A blocked worker close reaches its two-second termination response and its
  cache is recoverable after process exit.
- A real connected peer that never answers its handshake exposed premature
  successful close: upstream Drop alone did not join that attempt. Final-handle
  cleanup now waits for outgoing attempts to retire. Owned child-worker
  fixtures cover both explicit close and cancellation followed by shutdown.
  Each reaches the controller deadline, terminates with status 3, closes the
  actual peer connection, and releases cache ownership for safe restart.
- Shutdown revokes every live handle before joining blocked storage, and also
  joins handles already being retired by cancellation or expiry. Retiring
  handles continue to count against admission until cleanup finishes.
  Earlier public stop timings predate this stronger join check and must be rerun.

These tests do not yet cover the full hostile-wire, large-media, malformed
archive or cross-platform matrix. Process-kill recovery is not a power-loss
durability proof. The cache currently scans persisted records on restart;
large-cache startup cost remains to be measured.

## Packaging observations

The initial Linux worker and shared library built and passed process lifecycle
and C ABI load/call/free smoke tests. The Windows x86_64 worker cross-compiled.
Android API 24 libraries built for all three ABIs with 16 KiB alignment checks.
These were dirty-tree v1.61.0 candidate artifacts, not an immutable release set.
The initial Android x86_64 load probe caught an omitted `libc++_shared.so`
dependency. The corrected build statically links the only C++ owner inside its
private worker; all three ABI libraries then passed dependency/alignment checks.
A separate API 36 x86_64 emulator loaded JNI and passed runtime open, invalid
source refusal and shutdown in an independently verified worker process. The
standalone probe was a dirty-tree qualification build, not a consumer build.
ARM loading, API 24 execution, 16 KiB-page execution and Media3 playback remain
unverified; no physical device was used.

The existing Rust gateway workspace tests and design validation passed.

## Remaining gates and delivery

1. Finish matched current/upstream rqbit, candidate, libtorrent/Nuvio and
   Stremio comparisons, including randomized cold/warm repeats on the failure
   corpus. Record steady playback, seeks, memory, disk and cancellation; revise
   the candidate's reliability and latency. These performance comparisons are
   follow-up work and no longer block owner-authorized client integration.
2. Extend discovery/hostile-wire fixtures to the full network matrix, and stress
   eviction with files larger than device storage, simultaneous active
   viewers and repeated crash/restart.
3. Complete the archive matrix, including encrypted RAR5, every multipart
   boundary and explicit/default inner-member selection across multiple entries.
4. Build and verify Android ABI loading/Media3, Linux and Windows native player
   behavior. Cross-compilation alone does not qualify playback.
5. Integrate backend/core versioned negotiation and shared deadlines, Android
   private worker service, desktop supervision, and gateway worker ingestion.
   Preserve native-only acquisition and existing players/authorization.
6. Verify gateway direct/copy/remux/transcode and real HTTPS web/Roku delivery.
   Update contracts, consumers and artifact revisions together; remove obsolete
   wrappers only after replacement qualification.

Physical-device tests and production rollout remain separately coordinated.

## DHT fixture dependency observation

The pinned anacrolix/dht in-memory peer store builds its key from IP alone,
then decodes the key as IP plus port when serving `get_peers`. An owned fixture
using that store could not return the correct peer. The fixture root now uses
an independent fixed response; the candidate's real DHT lookup resolves it.
This does not indicate that third-party DHT roots use that broken store.
The production client leaves that optional peer store unset; the defect belongs
to the test-root setup and does not require a runtime peer-store replacement.
See [library ownership audit](../docs/TORRENT_LIBRARY_BOUNDARY.md).
[Pinned store source](https://github.com/anacrolix/dht/blob/ec3a9bd99456/peer-store/in-memory.go).

## Exact outer-file descriptor during client integration — 2026-10-09

The private source may carry `expected_file_size` with an explicit file index.
The runtime validates this against hashed metadata before serving bytes or
opening a RAR. A mismatch retires the handle with the existing exact-selection
failure. Invalid/unsafe sizes are refused at admission; asynchronous preparation
copies the size rather than retaining the caller's mutable pointer.

The complete Go race suite passes (9.879 s), including exact and mismatched
source-size fixtures. The Rust gateway workspace suite passes. This change
binds file identity and does not establish client/device latency or rollout.
