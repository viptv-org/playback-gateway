# Actual desktop playback and seek smoke — 2026-10-23 UTC

The optimized `e49dca8024e74ea85a064f7a4eb94702cf9691c7` torrent worker was tested through the actual KMP desktop player on Linux, using an owned virtual display and software rendering. Dune Part Two covers a larger H.264 MP4, Shawshank covers the stream whose short startup screen favored Stremio, and Turbo Kid covers HEVC/MKV. Every run used a fresh worker and cache.

Each run plays about 45 seconds from the start, seeks to 95% of the duration reported by the player, plays another 15 seconds, seeks back to 30 seconds, then plays another 15 seconds. Seek recovery requires three new distinct-image draw reports at the destination; returning from a seek call alone does not count. The same player stays alive throughout. Its presentation hook emits once per distinct drawn image. An owned-display screenshot was also inspected to verify real video pixels.

The smoke checks sampled frame age every 100 ms, counted detected freezes above 500 ms, measured displayed fps, and checked timeline advancement. A strict phase pass requires no detected freeze, at least 20 displayed fps and at least 80% of the requested timeline advancement. Seeking delays are measured separately from steady playback. This is a quick screen, not a microstutter, physical-scanout or long-session qualification.

| Run | Start / near-end / back displayed fps | Seek to 95% | Seek back | Strict smoke |
| --- | --- | ---: | ---: | --- |
| Dune Part Two, ours | 23.97 / 23.98 / 23.91 | 0.727 s | 0.300 s | Pass |
| Shawshank Redemption, ours | 23.96 / 23.95 / 23.95 | 1.917 s | 0.299 s | Pass |
| Turbo Kid, ours, initial | 24.00 / 23.98 / 19.29 | 3.723 s | 0.356 s | Fails fps threshold after backward seek |
| Turbo Kid, ours, repeat | 23.97 / 23.97 / 23.95 | 3.851 s | 0.315 s | Pass |
| Turbo Kid, Stremio control | 24.00 / 23.98 / 23.97 | 3.837 s | 0.311 s | Pass |

All five actual runs completed all three timed video segments and both requested seeks: 15 segments and 390.47 seconds of observed playback. No sampled freeze above 500 ms was detected; maximum sampled frame age was 80 ms. Reported rebuffer and audio-underrun counters remained zero in every run. The audio sink was an owned null sink, so this is not physical-speaker or passthrough qualification.

The initial HEVC backward segment is retained as a failed strict smoke, rather than weakened or discarded. Decoding remained close to 24 fps while displayed frames fell to 19.29 fps. The repeated candidate run and the same-player Stremio control both displayed about 24 fps. That does not establish a reproducible cause or prove a fix. The repeat/control used `JAVA_TOOL_OPTIONS=-Xmx512m`; their launchers acknowledged the option, but there is no evidence that changing Java heap fixed the cadence outlier.

## Memory finding

Aggregate player/worker RSS was unexpectedly large. The repeated HEVC comparison sampled process RSS separately: the desktop player reached about 7.14 GiB with our worker and 7.13 GiB with Stremio, while our Go worker peaked at 39.8 MiB and the Stremio server at 159.3 MiB. A requested 512 MiB Java heap is not an overall process/native-memory limit; it did not prevent the high RSS. The precise heap/native/graphics ownership cause remains unresolved. These measurements identify the desktop player process, rather than the Go torrent worker, as the source of the large observed footprint; they do not establish whether it is a leak, allocation pressure or caching.

The initial Dune RSS trace rose rapidly then stayed around 6.4–6.9 GiB for the rest of its short run. That interval is too short to prove a stable long-session bound. This memory finding and the intermittent HEVC cadence dip need follow-up before claiming reliable hours-long playback. The successful short clips alone do not clear those concerns.

## Reproduction and evidence

The reusable private hook is in `app/composeApp/src/desktopMain/kotlin/org/viptv/app/DesktopTorrentBenchmark.kt`. `torrent-runtime/scripts/benchmark-desktop.py --playback-smoke` sends the opt-in specification; normal startup-only and existing seek measurements retain their separate paths. The parent rejects a reported smoke success unless all three timed phases and the true 95%/backward seek measurements exist, preventing an older startup-only player build from silently passing. Its benchmark unit suite passes 32 tests. The updated desktop benchmark JAR compiled successfully offline in 12 seconds. Player libraries and torrent engine behavior were not changed.

The playback JAR is `8a981d73ecf436a835ff896f9d30fe021ed3e67b30d211baa9932a4a232f637c`; the prior frozen cold-start JAR remains unchanged at `8a6594947ae403fda60e2bc7b2b5a0eaa17d56c0be6a036058545c4aad715634`. The tested worker remains `55ae4c16efc8d006605cbea575393760d181fc4296258dc72eb1afc8202ae0f9`. All observed file selections match their exact infohash and durable selection records. The extra HEVC control uses stock Stremio 4.19.0 through the same renderer and seek procedure.

An initial runner-only deadline-scope error stopped three launches before playback; it was fixed and covered by a deadline regression test. Those setup logs remain in the receipt separately from the five actual video trials. Both measured managed jobs exited zero, all candidate worker exits were verified, and the HEVC strict failure remains in the results. A successful runner exit does not imply every individual smoke passed.

[Numeric evidence](evidence/torrent-playback-smoke-2026-10-23.json) preserves every measured phase, failed smoothness outcome, timing, diagnostic counter, source/build identity, resource sample reference and the setup failure. Private raw logs, resource samples, caches, source snapshots and the screenshot remain under `/mnt/ALPH/.viptv-qa/playback-smoke-20261023-0143/`. This phase does not qualify Android, physical displays, sustained hours-long playback or production delivery.
