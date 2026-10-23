# All supplied movies: Linux cold-start retest — 2026-10-22

Follow-up [actual playback and near-end/backward seek tests](TORRENT_PLAYBACK_SMOKE_2026_10_23.md)
found no detected half-second freezes in five short runs, but retained an
intermittent HEVC cadence dip and a large shared desktop-player memory footprint.
Cold-start speed alone does not establish long-session reliability.

The optimized `e49dca8024e74ea85a064f7a4eb94702cf9691c7` worker was retested against unmodified Stremio server 4.19.0 across all 21 supplied torrents. Each engine used the same isolated KMP player, software renderer, exact magnet and trackers, largest-file selection, fresh worker and empty cache. Timing covers runtime admission to the first actual `DesktopDrawCompleted` image. Metadata and rendering each retain their separate 30-second deadlines.

The predeclared fast-screen procedure runs one pair per movie, followed by exactly two extra pairs for either-engine failures, candidate losses or a candidate advantage smaller than 20%. First-pass outcomes are retained. Six movies met that repeat rule: Interstellar, Shawshank Redemption, Forrest Gump, Safety Not Guaranteed, Certified Copy and Martha Marcy May Marlene. This creates 33 pairs / 66 attempts; sources with clear initial advantages retain one pair. No unsuccessful attempt was removed or replaced.

These are diagnostic screening results, not twenty-pair qualification of every source. Source labels below compare failure-inclusive median latency and observed failure rates; they are provisional. Live peer availability changes between runs. The earlier 20-pair gates on Safety and Certified Copy remain separate evidence and are not pooled here.

| Movie | Pairs | Ours median | Starts | Stremio median | Starts | Screen |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Dune Part Two | 1 | 3.405 s | 1/1 | 7.766 s | 1/1 | Ours favored |
| Oppenheimer | 1 | 4.336 s | 1/1 | 8.221 s | 1/1 | Ours favored |
| Interstellar | 3 | 5.507 s | 3/3 | 8.756 s | 3/3 | Ours favored |
| The Matrix | 1 | 2.592 s | 1/1 | 10.546 s | 1/1 | Ours favored |
| The Dark Knight | 1 | 3.675 s | 1/1 | 5.570 s | 1/1 | Ours favored |
| Inception | 1 | 4.303 s | 1/1 | 6.303 s | 1/1 | Ours favored |
| Fight Club | 1 | 3.013 s | 1/1 | 29.873 s | 1/1 | Ours favored |
| Shawshank Redemption | 3 | 5.029 s | 3/3 | 4.611 s | 3/3 | Stremio favored |
| Pulp Fiction | 1 | 4.213 s | 1/1 | 5.794 s | 1/1 | Ours favored |
| Forrest Gump | 3 | 2.229 s | 3/3 | 11.769 s | 3/3 | Ours favored |
| Blue Ruin | 1 | 1.069 s | 1/1 | 2.492 s | 1/1 | Ours favored |
| Timecrimes | 1 | 2.601 s | 1/1 | 3.710 s | 1/1 | Ours favored |
| The Vast of Night | 1 | 1.778 s | 1/1 | 2.363 s | 1/1 | Ours favored |
| The One I Love | 1 | 1.383 s | 1/1 | 6.510 s | 1/1 | Ours favored |
| Safety Not Guaranteed | 3 | 3.823 s | 3/3 | 8.999 s | 3/3 | Ours favored |
| The Guest | 1 | 0.864 s | 1/1 | 2.453 s | 1/1 | Ours favored |
| Upstream Color | 1 | 3.494 s | 1/1 | 5.435 s | 1/1 | Ours favored |
| Turbo Kid | 1 | 1.204 s | 1/1 | 2.384 s | 1/1 | Ours favored |
| Certified Copy | 3 | 9.658 s | 3/3 | 14.762 s | 3/3 | Ours favored |
| Martha Marcy May Marlene | 3 | Failure-censored | 0/3 | Failure-censored | 0/3 | Neither started |
| Sound of My Voice | 1 | 2.876 s | 1/1 | 7.725 s | 1/1 | Ours favored |

The screen favored ours on 19 movies and Stremio on one; one reached no frame in either engine. Both engines rendered 30/33 attempts. Every failure contributes infinity to its source's latency distribution; a failure-censored median is not a missing measurement. Both engines failing does not establish a speed advantage.

The first pass completed in 5 minutes 43 seconds; the fixed repeat batch took about 5 minutes. Both managed jobs exited zero. All 33 candidate worker process exits were verified. This is 66 automated cold attempts without manual playback operations.

All observed selections match their exact infohash, file index, path and byte length. Candidate selection is checked against durable resolved-selection records, not just the largest file inferred from metadata. The numeric receipt retains failures, retirement/exit outcomes, resources, first-pass and followup batch identities and all 66 attempts. Source content is identified by torrent metadata; creative movie content is not independently authenticated.

The programmatic reporter validates every scheduled attempt, source identity, artifact hashes and timing settings before combining batches. Repeated subset indices are mapped back to the original movie IDs. Its benchmark test suite passes 29 tests, including failed-fast measurement handling, unavailable swarms, repeat selection and source-subset mapping.

[Numeric evidence](evidence/torrent-all-movies-retest-2026-10-22.json) and [earlier two-source qualification](TORRENT_PIPELINE_CANDIDATE_2026_10_22.md) preserve the respective scopes. The reusable cache remains verified-only; live contiguous early playback is the previously approved behavior. This phase changes no engine behavior or consumer pins. Android, other desktop platforms, sustained playback and rollout are outside this retest.
