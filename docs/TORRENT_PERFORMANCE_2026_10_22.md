# Torrent performance, 2026-10-22

**Historical verified-only phase.** The owner subsequently approved early live
playback on 2026-10-22. The measurements and integrity policy below describe the
earlier phase, before that approval. See [the cold-start follow-up](TORRENT_COLD_START_2026_10_22.md)
for the current implementation, separate evidence, and scoped acceptance result.

**The strict Stremio parity gate had not passed in this phase.** The working-tree changes improve
cache access and add the KMP desktop worker host. They do not establish maximum
end-to-end performance or qualify an all-platform rollout. Consumer runtime pins
and production deployments remain unchanged.

## Retained changes

- Disk reads, checksum scans and durable completion run under per-entry ownership
  instead of the global cache mutex. IO pins fence eviction and namespace reuse;
  shutdown waits for admitted IO. The persistent record format and both syncs stay
  unchanged, and only verified complete pieces are readable.
- At most 64 piece descriptors are retained. Completion notifications target the
  pending piece, and metadata replacement rechecks inventory after yielding.
- The current KMP desktop app has a supervised v2 worker host, bounded control
  framing, suspension-aware clocks, checked resource extraction, and package tests.
  macOS capability requires a worker revision with its suspension-aware clock.
- The desktop player retries a local-torrent seek at most three times when the
  documented non-mutating quiescence refusal occurs. Cancellation, unsupported
  sources, failed sessions and ordinary HTTP sources retain their failure behavior.
- Public HTTP/control/JNI APIs, selection rules, cache budgets and verification
  requirements remain unchanged. Measurements use private diagnostics.

## Measured results

[Numeric evidence](evidence/torrent-performance-2026-10-22.json) includes individual
trials, hashes, acceptance decisions and limitations. Baseline runtime source is
`8deb493c5d428d64298e339f7ae02ebc195e1fdc`; the reference is the installed, unmodified
official Stremio server 4.19.0. Both use the same KMP renderer and decoding settings.
Reference source admission waits for the app to be ready, preventing JVM startup
from prewarming Stremio. Trials alternate in seeded random order and retain failures.

Three local microbenchmark repetitions on Btrfs measured:

| Operation | Baseline median | Candidate median | Ratio |
| --- | ---: | ---: | ---: |
| Verified 32 KiB cache read | 3,238 ns | 926 ns | 3.50× faster |
| Mixed reads and durable piece completion | 92,404 ns/op | 60,775 ns/op | 1.52× faster |

These are cache-operation measurements, not download speeds. An earlier tmpfs
profile attributed 82.9% of mutex wait to completion; it measured 3.8×/1.4× gains.
The Btrfs run exercises persistent-disk syncs and has appreciable run-to-run noise.

The public screening ran four freely distributed films from the
[WebTorrent corpus](https://github.com/webtorrent/webtorrent/blob/master/docs/free-torrents.md):
three repetitions × four cases × three engines × cold/retained cache = **72
successful startup/forward-seek/backward-seek trials**. All use an owned virtual
display and silent audio sink. The sources are identified by ordinal in evidence.

Cold-cache medians in seconds (three trials per cell):

| Case | Baseline startup | Candidate startup | Stremio startup | Candidate forward seek | Stremio forward seek |
| --- | ---: | ---: | ---: | ---: | ---: |
| 0 | 2.325 | 2.221 | 2.190 | 0.985 | 3.695 |
| 1 | 3.741 | 4.145 | 3.140 | 0.985 | 0.847 |
| 2 | 1.818 | 1.758 | 1.832 | 1.046 | 2.676 |
| 3 | 1.928 | 2.346 | 2.590 | 1.037 | 0.975 |

Retained-cache candidate startup medians were 0.570–0.661 s; Stremio's were
0.687–0.736 s. Some seek cells still lose. Three public-swarm repetitions cannot
establish stable percentile claims, and the failed cases cannot be averaged away.
The harness therefore leaves qualification false. The expensive 20-pair/ten-minute
qualification matrix and Android stage were not entered after screening failed.

## Controlled findings and rejected experiments

A generated 140-second H.264/AAC movie, 4 MiB pieces and a single 1 MiB/s seeder
produced final cold-start medians of 5.10 seconds for VIPTV versus 0.98 seconds for
Stremio (three trials each). Forward-seek medians were 8.21 versus 5.52 seconds.
The seeder allows a 1 MiB initial burst: a whole 4 MiB piece cannot arrive within
Stremio's observed startup time. Cold-start equality on this case conflicts with
the retained verified-before-read requirement. No responsive/unverified read mode
was enabled to manufacture parity.

The final repeated controlled run completed **18/18** cold/retained startup and
seek trials with the bounded seek retry, using original-runtime and final-runtime
workers plus Stremio through the same final player. Individual timings and 1 Hz
resource samples are retained. Resource RSS is the combined JVM and its worker
(or reference server), not the Go worker alone. Warm-worker and sustained-playback
qualification remain outstanding.

The fixture also reproduced cold seeks that reverted to the old position. The
pinned KitePlayer source documents a two-second demux quiescence deadline and an
`IllegalStateException` refusal before pipeline mutation. A pending verified piece
can exceed that deadline. A bounded retry restored destination-frame completion;
the earlier conjecture that commands were dropped before `open()` completed was
disproved by tracing and that experimental change was removed. The retry does not
interrupt a source whose interrupt would permanently poison later reads.

Rejected: a consumption-based readahead ramp worsened seeks; reduced speculative
priming priority did not improve the controlled result. Neither is in production
code. The libtorrent 2.1.1 reference did not show a consistent speed advantage and
does not implement rolling eviction. Its first run contained a reference-adapter
error-code conversion defect; that run is excluded from engine conclusions and
the corrected run is retained separately. No engine replacement was adopted.

## Verification and reproduction

Passed: full Go race suite and vet; 463 desktop tests (zero failures, two existing
skips, 465 total); four acceptance-evaluator tests; the three real gateway torrent/FFmpeg
tests, including bounded-cache seeks; packaged native engine and torrent-worker
open/shutdown/reopen; Windows cross-build; Darwin arm64 compile-only build with
CGO disabled; both iOS Kotlin compile targets. Windows/macOS/iOS execution and
Android performance remain unqualified. Linux packaging used the cached portable
Temurin JDK 21: the host Arch JDK launcher produced a package-probe child abort,
which remains in the private logs rather than being suppressed in verification.

Runtime entrypoints:

- `torrent-runtime/scripts/benchmark-streaming.py`: fast, explicitly labelled
  FFmpeg decoded-frame screening; never counts as rendered-frame qualification.
- `torrent-runtime/scripts/benchmark-desktop.py`: same-player rendered-frame
  comparison, cold/retained caches, bounded deadlines, per-case acceptance gate,
  optional sustained playback and sampled CPU/RSS/cache allocation.
- `torrent-runtime/scripts/benchmark-owned.py`: generated media, owned tracker,
  configurable piece size and upload rate. It uses **test-only** private-network
  workers; production destination policy is unchanged.
- `torrent-runtime/scripts/libtorrent-reference.py`: isolated Python reference,
  explicitly unsuitable for production and limited to torrents fitting its budget.

Run GUI comparisons through the workspace's managed virtual-display job runner.
Every run requires a fresh private output directory. Use the scripts' `--help` for
exact arguments. Ordinary tests never access public swarms. Reproduce the cache
profile with `GOMAXPROCS=2 go test -run '^$' -bench BenchmarkCache -benchtime=2s
-count=3`; choose a persistent-filesystem `TMPDIR` for the Btrfs comparison.

Private logs, source descriptors, artifacts and profiler data are under
`/mnt/ALPH/.viptv-qa/torrent-performance-20261022/`. Only numeric evidence is tracked.
No physical display/TV, production backend or served Wasm output was changed.
