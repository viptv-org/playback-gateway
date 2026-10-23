# Seek investigation and stored-byte repair — 2026-10-23

The retained change fixes a reproducible return-seek stall. Late network writes
can arrive after their range is retired. The bounded cache correctly discards
those bytes, but the torrent engine can still mark the chunks received. Whole-
piece completion remains false, so refreshing completion alone does not repair
this mismatch. A later range then waits for unrelated chunks and a hash-read
failure before its missing bytes are requested again.

The controlled two-peer regression sends a real late chunk which storage drops,
then gates unrelated blocks. Before the fix, a request for bytes 16384–17407
caused the second peer to receive an unrelated request at 49152. With the fix,
the exact HTTP slice returns while those unrelated blocks remain gated. The
regression passes 20 consecutive race-enabled runs. Existing present bytes stay
marked received; reusable completion still requires hash verification.

The cache now supplies metadata-only stored-span evidence. The runtime reconciles
a newly admitted head before publishing urgent demand, then checks the current
missing block while waiting. Verified readers reconcile their head once, avoiding
a whole-piece scan on every block arrival. Storage without this optional evidence
retains its existing behavior. No HTTP, control v2, C/JNI or seek-accuracy contract
changes. Cache budgets, cancellation and authority deadlines remain in force.

## Why startup can beat a seek

Startup normally reaches the first keyframe near the start. An exact seek can
require an index elsewhere in the file, uncached destination data, and decoding
from a preceding keyframe. [mpv documents this precise-seek cost](https://mpv.io/manual/stable/#options-hr-seek).
In one traced HEVC seek, the server waited 3.301 seconds for index bytes and
another 1.146 seconds for video bytes. The index request started 69,303 bytes
from EOF and 1,683,271 bytes into a 2 MiB piece. Startup did not need that index.

The current benchmark also counts startup at one image and seek recovery at
three destination-image draws: roughly two extra frame intervals, or 83 ms at
24 fps. This small measurement difference does not explain multi-second waits.

## Rendered HEVC screen

The final comparison uses one compiled public-policy worker test binary, with
only stored-chunk reconciliation toggled by a private qualification field. Both
variants use approved live-block playback, identical tracker/file inputs, fresh
caches, the same mpv surface and exact seeks. The first three randomized paired
trials include stock Stremio 4.19.0. Three further paired trials repeat only the
before/repair comparison. Each jumps to 95% of the observed duration and back to
10 seconds, requires three destination images, then checks three seconds of
playback with at least 20 drawn fps and no sampled frame age over 500 ms.

| Worker | Trials | Complete playback screens passed | Startup median | 95% seek median | Return-to-10s median |
| --- | ---: | ---: | ---: | ---: | ---: |
| Before repair | 6 | 5 | 1.129 s | 2.250 s | 1.789 s |
| Stored-byte repair | 6 | 6 | 1.173 s | 3.044 s | 0.554 s |
| Stock Stremio | 3 | 3 | 2.628 s | 1.631 s | 0.387 s |

These are medians of recorded recovery latencies, including the recorded latency
from a baseline run whose subsequent playback failed the fps threshold. That
failure remains in the pass count and raw evidence. Missing measurements never
become zero. Returning to 10 seconds after only one second of initial playback
is not guaranteed to use cached data.

Return-seek recovery improved in this small screen; long forward seeks did not
show a consistent improvement and remain slower than Stremio in this HEVC case.
The correction is retained for the deterministic storage-state defect, not as a
claim of a general performance win. Six pairs and three Stremio runs do not meet
the approved per-case 20-pair parity gate. The wider seek objective remains open.

## Rejected experiments and validation

Earlier experiments rotated urgent chunk order, bounded that boost, preserved
request ownership, and warmed full/tail index regions after the first frame.
Their seek/playback trade-offs were inconsistent, so those scheduling and index-
prefetch changes were removed. Every exploratory row, timeout and recovery-fps
failure is preserved. A profiler-only concurrent-map panic also exposed an
upstream KnownSwarm lock gap; its read lock now covers pending and half-open peers
as well as active peers, with a concurrent-admission race regression.

The full runtime Go race suite passes, including cache eviction, corruption,
crash recovery, cancellation, stalled peers and concurrent viewers. The stored-
byte wire regression passes 20 race-enabled repetitions; all 33 benchmark Python
tests pass. Final source uses binary-search span lookup and a single native
priority update per repair batch; the numeric screen binary predates these
metadata-only refinements, whose final source passed the same regressions.

The reusable quick runner is `torrent-runtime/scripts/benchmark-desktop.py
--seek-screen`; its validator rejects missing/shortened playback checks and
incorrect seek destinations. Private HTTP read timings and repair counters do
not extend production responses. The KMP private benchmark gained timestamped
diagnostics and the quick screen; normal mpv controls and rendering are unchanged.

[Numeric evidence](evidence/torrent-seek-screen-2026-10-23.json) includes all
exploratory runs, failures, same-build controls, selections, hashes and limits.
Private raw data, rejected source snapshots, worker binaries and logs are under
`/mnt/ALPH/.viptv-qa/seek-performance-20261023/`. File selections match the exact
requested infohash and resolved selected-file records.

Qualification is Linux on an owned virtual software display with a null audio
sink. Physical scanout/audio, Android and native Windows/macOS execution are
unqualified. Consumer runtime pins and packaged platform workers are not promoted
by this investigation; this is a shared-runtime source candidate.
