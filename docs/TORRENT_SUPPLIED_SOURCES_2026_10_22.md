# Supplied-source Linux cold-start baseline — 2026-10-22

This fixed baseline measured all 21 user-supplied magnets three times per engine: 126 cold attempts. Candidate rendered 59/63; unmodified Stremio server 4.19.0 rendered 53/63. Among the 52 pairs where both rendered, each engine was faster in 26 pairs. These counts do not establish a universal speed advantage or cold-start parity qualification.

The table reports successful-start medians and success/attempt counts together. Failed attempts remain in the numeric evidence and reliability totals. Three pairs per source are diagnostic; they cannot qualify p90, other platforms or future live swarm behavior. User-provided “POPULAR” and “NOT POPULAR” categories are retained in the source receipt and were not independently verified.

All values are seconds to the first rendered image. Medians include successful starts only; success counts and every failure are shown. Three paired attempts per source are diagnostic, insufficient for p90 or parity qualification.

| User label | Candidate median | Starts | Stremio median | Starts | Both-rendered paired wins C/S |
|---|---:|---:|---:|---:|---:|
| Dune Part Two | 4.854 | 3/3 | 13.297 | 2/3 | 2/0 |
| Oppenheimer | 6.017 | 3/3 | 3.052 | 3/3 | 1/2 |
| Interstellar | 10.303 | 3/3 | 6.121 | 2/3 | 0/2 |
| The Matrix | 7.981 | 3/3 | 6.073 | 2/3 | 0/2 |
| The Dark Knight | 3.972 | 3/3 | 4.465 | 3/3 | 1/2 |
| Inception | 6.920 | 3/3 | 10.356 | 3/3 | 2/1 |
| Fight Club | 5.526 | 3/3 | 9.513 | 3/3 | 3/0 |
| Shawshank Redemption | 7.453 | 3/3 | 8.956 | 3/3 | 2/1 |
| Pulp Fiction | 8.444 | 3/3 | 11.322 | 3/3 | 2/1 |
| Forrest Gump | 6.723 | 3/3 | 3.769 | 3/3 | 1/2 |
| Blue Ruin | 4.672 | 3/3 | 6.826 | 3/3 | 1/2 |
| Timecrimes | 3.420 | 3/3 | 7.060 | 3/3 | 3/0 |
| The Vast of Night | 6.612 | 3/3 | 3.513 | 2/3 | 0/2 |
| The One I Love | 7.770 | 3/3 | 7.164 | 3/3 | 1/2 |
| Safety Not Guaranteed | 12.101 | 3/3 | 9.042 | 3/3 | 0/3 |
| The Guest | 2.772 | 3/3 | 2.635 | 3/3 | 2/1 |
| Upstream Color | 5.132 | 3/3 | 3.671 | 3/3 | 1/2 |
| Turbo Kid | 1.355 | 3/3 | 2.701 | 3/3 | 3/0 |
| Certified Copy | 18.192 | 2/3 | 7.066 | 2/3 | 0/1 |
| Martha Marcy May Marlene | 19.650 | 3/3 | 1.935 | 1/3 | 1/0 |
| Sound of My Voice | — | 0/3 | — | 0/3 | 0/0 |


Both engines used the same isolated KMP application, decoder, software rendering and owned Xvfb display. Timing starts after the application is ready, before worker/source admission or reference launch, and ends at the first `DesktopDrawCompleted` image. Each attempt starts new processes and an empty cache, including fresh metadata/DHT state. The six tracker strings are preserved exactly. Metadata is resolved after timed admission; each engine selects its largest file independently. The serial case/engine order follows the recorded seed and fixed schedule. Trial 0 was screened first; fixed trials 1 and 2 continued across the entire corpus without selecting winners or changing settings. Separate metadata/admission and first-image waits each have a 30-second deadline; total attempt duration can exceed 30 seconds.

All resolved selections match the requested infohash, file index, path and byte size. Candidate selection is observed from 63 durable resolved-selection records and checked against cached info bytes. Stremio resolved per-trial metadata in 61/63 attempts; the two missing selections are not imputed from other trials. The corpus receipt also records piece sizes and containers. Filenames establish the resolved metadata identity, not independent verification of the media's creative content.

Candidate failures comprise four attempts after a media endpoint was returned: Certified Copy once and Sound of My Voice three times. Stremio had eight failures after its endpoint returned (Vast of Night, Matrix, Interstellar, Certified Copy and Dune once each, Sound of My Voice three times), plus two Martha Marcy May Marlene metadata/admission timeouts before an endpoint returned. The after-endpoint failures rendered zero frames. Logs usually report media-read “no answer ... within 10s”; Sound of My Voice candidate trial 2 instead records no rendered frame before the render deadline without that log diagnostic. Phase details are retained per attempt.

Safety Not Guaranteed was slower in all three pairs where both rendered. Interstellar, Matrix and Vast of Night lost both comparable successful pairs; their reference also failed another attempt. Certified Copy had two successes per engine with failures in different pairs. These cases motivate further experiments; the frozen baseline does not contain optimized candidate results.

The measured executable is `final-worker`, SHA-256 `bcb96f9fdf47cb236aec7d4824ed9c89a7016e710a2c771f53c7fe9871d78e47`, built by Go 1.25.11. Its measured source was subsequently adopted as checkpoint `674b71523dff5a74fb339eae97ddcb6217ea9f8b`; the earlier [adoption receipt](evidence/torrent-cold-start-adoption-2026-10-22.json) documents code equivalence. Embedded VCS metadata predates that adoption and is recorded separately. The current isolated normal-pin main JAR has SHA-256 `4029588a193925ef72dc1d379ad3e645acbe520e19a0ba293d122ce29d8c2635`. All timed worker, main-JAR, KitePlayer/Skiko and reference identities are retained in the [126-row numeric evidence](evidence/torrent-supplied-sources-2026-10-22.json); artifact hashes were unchanged after the run. Operational reference settings omit private network interface addresses and per-trial local paths.

The managed jobs `6b45c0a9-d766-4fdb-933f-ccbcfccd384a` and `97dc3a0f-094d-4db3-b08e-fd22a9e73546` both exited zero. A completed runner does not mean every playback succeeded: all 14 failed starts remain above and in the receipts. Raw private inputs, logs, metadata and caches remain under `/mnt/ALPH/.viptv-qa/supplied-magnets-20261022/`.

This phase does not qualify sustained playback, seeks, warm cache, archives, Android, other desktop platforms, physical display/GPU behavior or deployment. Resource samples are lower bounds on sampled peaks. Ordinary live contiguous unverified block playback remains owner-approved; reusable cache data remains verified-only, as in the prior cold-start report.
