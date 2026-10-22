# Cold pipeline candidate — 2026-10-22

This candidate targets two measured cold-start delays: a request queue that ramps up slowly on responsive peers, and a useful metadata peer queued behind stalled connection attempts. Public APIs, control protocol v2, C/JNI interfaces, persisted verification, cache budgets and authority semantics retain their existing behavior.

The request scheduler makes its first data request with upstream slow-start behavior. After useful payload arrives, it permits a larger minimum pipeline when few peers are connected, while retaining the remote request limit, local request limit and storage admission limits. The pinned MPL 2.0 library source is included under `torrent-runtime/third_party/torrent`; the adaptive minimum and sampled queue-time ownership comparison differ from upstream. The license and exact source-difference receipt are retained.

Discovery uses a global maximum of 200 half-open peer attempts, a 200-per-torrent initial cap and a matching bounded dial limiter. Once metadata arrives, the established-peer cap returns to 55. Existing in-flight attempts may finish. TCP/uTP and encrypted fallback remain supported. This is a bounded startup resource tradeoff and requires platform qualification.

## Fast, controlled validation

`TestPipelineQualification` transfers an exact generated 4 MiB payload through a real BitTorrent peer and HTTP reader. Requests incur fixed latency; concurrent responses share a 4 MiB/s upload limit. Both 2 MiB pieces must verify and all bytes must match. The fixture excludes discovery, public swarms and GUI startup so scheduler regressions can be screened in seconds. The application-layer queue problem is described in the [libtorrent maintainer's slow-start analysis](https://blog.libtorrent.org/2015/07/slow-start/).

At 120 ms request latency, three trials per implementation measured these medians:

| Scheduler | Exact 4 MiB delivery |
| --- | ---: |
| Pinned upstream | 2.057 s |
| Fixed minimum of 16 requests | 1.588 s |
| Peer-validated adaptive minimum | 1.470 s |

The adaptive version was 28.5% faster than upstream. At zero injected latency, all three measured approximately 0.997 s. All 18 transfers matched and verified; their summed test-process runtime was 24.55 s. The adaptive version sent one request before its first response, while the fixed-16 version sent 16. The controlled adaptive fixture also passed the race detector.

The clean candidate's full Go race suite passed 70 test cases (54 top-level tests), with 11 opt-in cases skipped and no failures. The local source replacement resolves to the included library directory. `scripts/benchmark-pipeline.py` compares two prebuilt test binaries and retains failures and artifact hashes. These checks do not substitute for first-frame qualification.

## Focused rendered-frame screens

The policy prototype was tested against unmodified Stremio server 4.19.0 through the same isolated Linux KMP app and software renderer. Each attempt used a new worker/cache, the exact supplied magnet and trackers, and the existing rendered-image timing boundary. Medians and p90 include failed attempts as infinity.

| Source | Candidate starts | Candidate median / p90 | Stremio starts | Stremio median / p90 |
| --- | ---: | ---: | ---: | ---: |
| Safety Not Guaranteed | 3/3 | 5.337 / 11.016 s | 3/3 | 16.489 / 23.303 s |
| Certified Copy | 3/3 | 4.275 / 4.284 s | 2/3 | 13.735 s / failed |

These initial three-pair screens selected the candidate for the ordinary-worker comparison below. Live swarms vary; they do not establish parity for other supplied sources or platforms. Safety sampled descriptor peaks were 236–239 for the candidate and 199–210 for Stremio; sampled peaks are lower bounds.

## Ordinary-worker cold gates

The clean `a388911a9e4c291d60f7fed7d6b5a93adc06f4d6` build produced worker SHA-256 `75500b252822828ec0d99fa16e503d48775eb21b6f62f683e37b23fccb7c7508`. It ran 20 alternating, randomized pairs per source through the same Linux software renderer. Each attempt had fresh caches and workers, identical magnet/trackers and observed largest-file selection. Metadata and rendering each had a 30-second deadline. Failures remain in the latency distribution as infinity.

| Source | Candidate starts | Candidate median / p90 | Stremio starts | Stremio median / p90 |
| --- | ---: | ---: | ---: | ---: |
| Safety Not Guaranteed | 20/20 | 7.232 / 10.504 s | 18/20 | 8.970 / 23.670 s |
| Certified Copy | 20/20 | 4.722 / 7.590 s | 13/20 | 13.784 s / failure-censored |

Both source-specific cold gates pass. This is 40/40 candidate starts against 31/40 reference starts. Safety's candidate was faster in 14 of the 18 pairs where both rendered; it does not win every individual live-network trial. Certified Copy was faster in all 13 pairs where both rendered. The candidate used more sampled CPU/memory; the numeric receipts retain those costs and forced worker-retirement outcomes. Process death was checked, but these startup gates do not establish graceful worker shutdown, sustained playback or other platforms.

The original evaluator incorrectly treated a reference p90 censored by failures as missing data and rejected Certified Copy. Its original decision and raw attempts are preserved. The corrected evaluator distinguishes failed attempts from genuinely missing successful measurements, rejects missing measurements, and allows a finite candidate latency to beat failure-censored reference latency. Regression tests cover these cases. [Ordinary-worker evidence](evidence/torrent-pipeline-linux-gates-2026-10-22.json) records both decisions and all 80 attempts.

## Narrow development loop

Use controlled transfers before public swarms. Build the baseline and candidate test binaries once with the same `pipeline_qualification_test.go`, Go version and fixture. Run `scripts/benchmark-pipeline.py` with `--baseline BASE.test --candidate CANDIDATE.test --output FRESH_DIRECTORY`. The default checks three pairs at 0 and 120 ms latency. Then run it into a second fresh directory with `--latency-ms 120 --mixed-peer`. The mixed fixture introduces a fast peer after a slow peer has supplied its first block. All bytes must match and both pieces must verify.

The runner now exits unsuccessfully on failures, missing measurements or median regressions exceeding 5%. This tolerance only rejects development candidates quickly; final rendered-frame gates still require median and p90 no worse than Stremio, without a tolerance. Artifact identities and all failed attempts remain saved. `benchmark-desktop.py --trial-offset N --seed SEED` resumes the same randomized trial schedule into a fresh evidence directory; retain and combine earlier attempts rather than restarting them or excluding failures.

The mixed fixture caught a 6.5% regression in the adaptive checkpoint: 2.194 s against upstream's 2.060 s. Delaying the large queue until four useful responses made it worse (2.413 s), so that experiment was rejected. Research of the pinned scheduler showed request stealing balanced queue counts rather than estimated delivery time. The [libtorrent streaming documentation](https://www.libtorrent.org/streaming.html) explains why peers should instead be assigned urgent work by estimated queue completion time.

A separate refinement uses measured rates after both peers have supplied four useful blocks, requiring 25% estimated delivery headroom before reassignment. Existing request grace, cancellation guards and queue bounds remain authoritative. Its mixed median was 1.719 s against 2.060 s upstream; single-peer latency remained 1.471 s against 2.057 s, with 0 ms controls within 0.1%. All 18 transfers matched and verified. The refinement's full runtime race suite, with the mixed fixture enabled, passed. The two ordinary-worker gates above belong to the earlier adaptive checkpoint; the refinement's own final gates appear below. [Narrow-screen evidence](evidence/torrent-pipeline-narrow-screen-2026-10-22.json) preserves the rejected variants and all numeric trials.

The refinement was frozen at `e49dca8024e74ea85a064f7a4eb94702cf9691c7`. Its clean ordinary worker (`55ae4c16efc8d006605cbea575393760d181fc4296258dc72eb1afc8202ae0f9`) is byte-identical to the screened experimental worker. The full runtime race suite and five upstream connection/request regressions passed. In a subsequent three-pair live screen, Safety's candidate median/p90 was 5.756/6.887 s versus Stremio's 19.338/20.553 s; Certified Copy was 3.962/4.587 s versus 6.759/13.903 s. All six starts per engine rendered, and all 12 observed file selections matched. These are screening results, not 20-pair qualification. [Frozen source and screen receipts](evidence/torrent-queue-time-screen-2026-10-22.json) retain every attempt.

Qualification resumed with `--trial-offset 3 --trials 17` under the same seed, source ordering, worker hash and player. `scripts/merge-cold-results.py --inputs SCREEN_DIRECTORY RESUMED_DIRECTORY --output FRESH_DIRECTORY` checks identical artifact/measurement identities, exact randomized order, source identities, consecutive trial ranges and every expected attempt. It rejects duplicates and gaps; failures are copied without alteration. Thus valid screens count toward the required 20 pairs without selectively discarding outcomes or restarting the same work. The benchmark test suite passes 23 cases, including changed-worker/source rejection and preservation of failed attempts.

## Queue-time refinement: final source-specific gates

The resumed run finished all 80 attempts. The strict merge accepted the batches and both cold-start gates passed, without a latency tolerance. All observed file selections matched, and the player JAR and worker hashes remained unchanged.

| Source | Candidate starts | Candidate median / p90 | Stremio starts | Stremio median / p90 |
| --- | ---: | ---: | ---: | ---: |
| Safety Not Guaranteed | 20/20 | 7.114 / 15.468 s | 20/20 | 10.039 / 18.025 s |
| Certified Copy | 18/20 | 12.263 / 19.088 s | 14/20 | 17.384 s / failure-censored |

Median latency improved by 29% on each source. Total rendered starts were 38/40 versus 34/40. The candidate's two Certified Copy failures occurred in trials 18 and 19 after metadata/endpoint admission; both timed out before any rendered image. Logs reported HTTP reads waiting for data, without a worker crash report. These failures remain in the latency distribution and failure-rate gate. Live swarm variation is substantial: the earlier three-pair screen overstated the final gain. A different candidate or network interval must not inherit these numbers.

All 40 candidate worker PIDs exited. Explicit settlement-timeout retirement occurred in 22 attempts; the remaining host retirements also use forced process ownership cleanup. Startup success does not imply graceful process shutdown or ten-minute playback qualification. The full source-specific records retain exit codes, sampled CPU/memory and all timeouts. Certified Copy used median sampled peak RSS of 598 MB versus 506 MB for Stremio; Safety used 461 MB versus 495 MB. These are sampled aggregate JVM/worker measurements, not isolated engine memory limits.

An isolated copy of the app's desktop build helper successfully built the pinned `e49dca8` archive, retained the locally modified library's MPL notice, and verified protocol-v2 readiness, open, shutdown and exit zero. Its stripped package worker hash is `bdbc97026ebefe921e73b64c509daa599283bc167021911f0373d6943f7a16e5`; the unstripped performance worker remains `55ae4c16efc8d006605cbea575393760d181fc4296258dc72eb1afc8202ae0f9`. This checks worker packaging/control, not a new packaged-app playback qualification.

[Final numeric evidence](evidence/torrent-queue-time-linux-gates-2026-10-22.json) includes all 80 attempts, both input-batch hashes, final gates, observed selections, source/build identity and packaging receipts. The wider supplied corpus, original free-film corpus, Android and other platforms remain unqualified for this refinement. Consumer pins remain at the earlier runtime pending those gates; this local source checkpoint is not a production rollout.

[Numeric evidence](evidence/torrent-pipeline-candidate-2026-10-22.json) retains controlled results, metadata probes, test receipts, source identity and all focused playback attempts. Earlier unsuccessful experiments and the frozen 126-attempt baseline remain preserved. Runtime pins and deployment have not been changed by this candidate checkpoint.
