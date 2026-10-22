# Cold pipeline candidate — 2026-10-22

This candidate targets two measured cold-start delays: a request queue that ramps up slowly on responsive peers, and a useful metadata peer queued behind stalled connection attempts. Public APIs, control protocol v2, C/JNI interfaces, persisted verification, cache budgets and authority semantics retain their existing behavior.

The request scheduler makes its first data request with upstream slow-start behavior. After useful payload arrives, it permits a larger minimum pipeline when few peers are connected, while retaining the remote request limit, local request limit and storage admission limits. The pinned MPL 2.0 library source is included under `torrent-runtime/third_party/torrent`; only `PeerConn.nominalMaxRequests` differs from upstream. The license and exact source-difference receipt are retained.

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

These are three-pair screens, not final qualification. Live swarms vary; a win on these screens does not establish parity for the other supplied sources or platforms. Safety sampled descriptor peaks were 236–239 for the candidate and 199–210 for Stremio; sampled peaks are lower bounds. The clean ordinary-worker build must reproduce the policy prototype before the required longer comparison.

[Numeric evidence](evidence/torrent-pipeline-candidate-2026-10-22.json) retains controlled results, metadata probes, test receipts, source identity and all focused playback attempts. Earlier unsuccessful experiments and the frozen 126-attempt baseline remain preserved. Runtime pins and deployment have not been changed by this candidate checkpoint.
