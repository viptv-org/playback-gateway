# Cold-start follow-up — 2026-10-22

**The cold-start gate passed on all five measured Linux desktop cases:** 20
paired trials per case, 200 successful starts and no failures in either engine.
Candidate median and p90 were below the stock Stremio reference for every case.
This phase is separate from the earlier
[verified-only performance report](TORRENT_PERFORMANCE_2026_10_22.md).

The owner explicitly approved Stremio-style early playback: ordinary selected
files may deliver contiguous received blocks to the live decoder before an
entire piece passes its hash. Unwritten holes remain unavailable, and a rejected
piece cannot supply further early bytes until its replacement verifies. Corrupt
bytes may reach the live decoder before rejection. Persisted/restarted reusable
data, storage readers, archive parsing and archive playback remain verified-only.
Cache format, budgets, authority checks, public APIs and the existing
`public_discovery_verified_v2` compatibility tag are unchanged.

The candidate also prefers ordinary handshakes while retaining encrypted fallback
and TCP/uTP. It primes the selected-file head; the player supplies tail demand
when needed by its container. Supported upstream configuration hooks are used,
without modifying the torrent engine or implementing another wire protocol.

Both engines use the same isolated Linux KMP application, decoder and owned Xvfb
display. Timing begins with runtime/source admission after the application is
ready and ends at the first `DesktopDrawCompleted` event's `System.nanoTime`
timestamp. Stremio is started only after that timer begins. Every cold trial has
a new cache and new processes; no media range or metadata is prewarmed. Trials
are paired in a seeded randomized order and run serially. Failures remain in
the statistics. Each case must have at least 20 matching unique trial pairs,
candidate median and p90 no higher than Stremio, and no higher failure rate.
Warm-cache, seek and sustained-playback results do not count toward this gate.

Cleanup records `worker_retirement` separately: `joined` means runtime cleanup
joined before parent process cleanup; `forced` means the runtime explicitly
returned the expected two-second `settlement_timeout` termination contract and
the parent confirmed the actual child exited. Unexpected replies, errors or a
surviving child fail the trial. A valid forced retirement does not invalidate
an already rendered cold first frame.

Scope is Linux software-rendered desktop cold startup on the recorded fixtures.
Physical display/GPU, Android, other desktop platforms, archives, sustained
playback, seeks and deployment are not qualified by this phase.

## Results

All values are seconds to the first rendered image. Public cases 0–3 are the
benchmark script's fixed four-film public corpus in its recorded order. The
controlled case uses a generated 140-second H.264/AAC faststart movie, 4 MiB
pieces, and a single 1 MiB/s seed with a 1 MiB initial burst. Public runs use
the production executable; only the controlled worker permits owned loopback
peers. The reference is the installed unmodified Stremio server 4.19.0.

| Case | Candidate median | Stremio median | Candidate p90 | Stremio p90 | Failures, candidate/reference |
| --- | ---: | ---: | ---: | ---: | ---: |
| Public 0 | 2.396 | 2.769 | 2.922 | 4.332 | 0/20, 0/20 |
| Public 1 | 3.323 | 3.392 | 4.314 | 6.093 | 0/20, 0/20 |
| Public 2 | 1.167 | 1.756 | 1.433 | 1.923 | 0/20, 0/20 |
| Public 3 | 1.826 | 2.757 | 2.177 | 3.127 | 0/20, 0/20 |
| Controlled | 0.569 | 0.713 | 0.584 | 2.159 | 0/20, 0/20 |

Public 1's median advantage is small (about 2%); live swarm variability prevents
a universal claim about other sources or future network conditions. The result
is the strict observed per-case acceptance gate, without tolerance margins,
cross-case averaging, warm-cache substitutions or seek improvements.

The final matrices used fixed settings after separate three-pair profile and
all-case screenings. They completed in managed job
`ca9c97ac-1bcc-4011-9323-0bae6cb14dba` with exit code zero. The 94 valid forced
candidate retirements and six joined retirements are explicitly retained;
all child exits were confirmed. These are retirement outcomes after first frame,
not discarded startup failures.

One-Hz full-trial resource sampling includes the JVM and worker/reference,
including teardown. Public candidate median sampled peak RSS was 340–409 MiB
versus 334–338 MiB for Stremio; median CPU usage was 2.44–2.53 versus 1.92–2.04
CPU seconds. Sampled peak values are lower bounds on actual peaks. This is a latency improvement with additional sampled resource
cost, not a claim of lower memory/CPU. Short-lived disk allocations can be missed
at this sampling rate; these measurements do not establish a peak-memory or
peak-disk bound.

## Verification and reproducibility

The full Go race suite passed, including cache reuse/rejection, holes,
corruption, revocation and cancellation. Separate encrypted-only peer fixtures
confirmed the ordinary-handshake preference retains encrypted TCP/uTP fallback.
All four desktop worker lifecycle tests and six Python acceptance tests passed.
The isolated application built successfully; its source was the clean app
baseline plus the task-owned desktop harness, avoiding unrelated active UI edits.

[Numeric evidence](evidence/torrent-cold-start-2026-10-22.json) contains all 200
sanitized trial rows, per-case decisions, resource summaries, worker/reference
hashes and the actual main application, KitePlayer and Skiko JAR hashes. The
main-JAR SHA-256 is
`fb4521a2b8d7c9580489d23ab219d2721d5206f637f6a46fd3cb09264a987cf2`.
Raw private logs, inputs, caches and source snapshots remain under
`/mnt/ALPH/.viptv-qa/torrent-cold-20261022/`; earlier results are preserved.

## Normal-build adoption

Gateway and app source pins now select immutable checkpoint
`674b71523dff5a74fb339eae97ddcb6217ea9f8b`. The gateway's vendored worker and
checksum lock were rebuilt from a clean clone and validated. Read-only ELF
comparison proves its machine-code section and all other allocated sections are
identical to the measured executable except VCS metadata, its string header,
build information and build-ID notes. A post-measurement section-extraction
command initially rewrote the private measured file; the exact original bytes
were reproducibly restored and match the original timed SHA-256. No timing
receipt was changed.

The app's ordinary pinned build, without a candidate override, also produced a
`qualification_only: false` worker. The isolated normal-pin Linux distributable
passed exact worker/renderer/native-resource inventory, launcher/JVM/SQLite and
worker open/shutdown/reopen checks. See the separate
[adoption receipt](evidence/torrent-cold-start-adoption-2026-10-22.json) for exact
artifact hashes. Active unrelated main-app UI edits were not included in the
isolated build. No production deployment or other-platform performance claim is
implied.
