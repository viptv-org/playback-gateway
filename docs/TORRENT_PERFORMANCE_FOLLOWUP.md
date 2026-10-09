# Torrent performance follow-up

Owner decision, 2026-10-09: proceed with client integration using the current
shared runtime. Performance parity is deferred; cache integrity, authorization,
cancellation and local validation remain required. This does not approve a
production rollout or assert that every failing public source is fixed.

The implementation uses unmodified upstream anacrolix/torrent. See
[library ownership](TORRENT_LIBRARY_BOUNDARY.md) and
[qualification ledger](../torrent-runtime/QUALIFICATION.md).

## Measured limits

Private sources A and B correspond to the two owner-supplied examples. Do not
commit their identities. These are serialized local FFmpeg one-frame probes,
excluding worker launch; they are not application-rendered frame measurements.

| Observation | A | B | Scope |
| --- | ---: | ---: | --- |
| Official Stremio default | 2.040 s | 2.217 s | Three cold trials each; early virtual-chunk reads |
| Official Stremio whole-piece mode | 2.789 s | 7.083 s | Three cold trials each; owned corruption fixture validates that mode |
| Shared Go fast-dial/priming experiment | 6.374 s | 4.161 s | Three cold trials each, verified pieces |
| Actual untuned worker | 6.277 s | 14.185 s | One cold trial each, immutable `fd0fa3c` |
| Actual streaming-policy worker | 7.375 s | 6.627 s | One cold trial each, local candidate iteration |

The latest worker experiment uses the library's rate/connection settings, header
and index priming, a 4 MiB startup read window and a larger bounded window after
first-frame acknowledgement. Results remain variable. Its single observations
do not prove improvement or regression across all swarms.

Actual worker probes exercised JSON control, short-lived authority, first-frame
acknowledgement, close, two-second forced termination and safe cache reopening.
Both tested workers needed termination while unfinished connections drained;
their caches reopened successfully. Measured worker RSS peaks stayed about
40–57 MiB in these short runs. Reserved piece/index charges stayed below 44 MiB
under the requested 2 GiB budget, while selected files exceeded 20 GiB. Those
short samples do not establish sustained memory/storage behavior.

## Remaining investigations

1. Repeat matched cold/warm and seek trials on the owner examples and original
   failing corpus. Measure actual player first frame separately from acquisition.
2. Distinguish metadata/discovery delay from blocking piece transfer. Preserve
   tracker inputs, network conditions, integrity mode and read demand in reports.
3. Evaluate the upstream sequential transport-handshake behavior using owned
   fixtures. TCP-only and plaintext-first experiments did not improve uniformly;
   keep TCP/uTP and encrypted fallback available.
4. Measure readahead and request-stealing configuration against first-minute
   stalls and forward/backward seek recovery; avoid optimizing the first frame
   at the expense of steady playback.
5. Reduce forced worker retirement during normal stops through supported engine
   ownership/cancellation seams. Keep independent viewers and cache recovery
   in the regression scope.
6. Measure retained-cache startup and large-cache integrity scanning, and qualify
   Android, Linux, Windows and gateway output separately after integration.

Upstream issues and public swarm variability remain follow-up evidence, not
permission to remove verification, relax disk bounds or add native gateway
fallback. Physical-device testing and rollout remain separately coordinated.

## Android integration observations, 2026-10-09

An owned API 36 x86_64 Android TV emulator decoded a privately recorded failing
H.264 source through the shared Go/JNI worker and Media3. The selected 2.05 GB
file used a 128 MiB rolling cache. A single cold run reached the local endpoint
in 11.0 s and its first decoded frame in 14.0 s. A process restart with retained
verified content reached a decoded frame in 1.74 s. A 120-second forward seek
completed in 15.3 s, followed by a successful backward seek and worker retirement.
The first seek timer observed position alone; it is excluded from seek evidence.

Priorities: reduce cold discovery/startup and seek recovery, then repeat matched
cold/warm/resume trials across sources and devices. This source and emulator
configuration differ from the Stremio comparison trials above. These observations
establish a working Android byte/player path and do not establish latency parity,
physical-device qualification, general swarm reliability or production rollout.

The Android integration and qualification details are recorded in
`android/docs/TORRENT_RUNTIME_V2.md` in the organization workspace.

## Linux desktop integration observations, 2026-10-09

The bundled Linux app's real Rust host and native plugin decoded the original
private failing H.264 source. One cold MPV trial reached its local endpoint in
12.9 s and rendered frames in 18.8 s. Retained cache gave GStreamer an endpoint
in 0.232 s and rendered frames in 3.37 s; a 120-second forward seek required
58.0 s for new rendered frames after position landing. MPV opened retained cache
in 0.411 s, but two forward-seek checks failed the subsequent-frame gate within
60 s. A later MPV trial produced rendered destination frames after 20.4 s.
The earlier failures remain an open reliability investigation; a later success
does not establish consistent seeking.

These app-window checks exercise the worker/native decoder path directly. They
exclude backend account/source-screen admission, use a software-rendered owned
Xvfb display and are single observations. Engine/cache differences prevent a
cold/warm or Stremio speed comparison. Prioritize MPV seek behavior, then repeat
cold/warm/seek trials and actual authenticated web/Roku delivery qualification.

The Linux check also caught and fixed a Rust host startup-protocol defect:
the production worker's readiness banner must be consumed before the first
control reply. Shared-host regression and gateway fixtures now include it.

## Gateway/browser integration observations, 2026-10-09

A production-worker gateway run reached HLS output in 21.9 seconds cold. Retained
verified cache gave later starts about 0.4-second output preparation. The output
copied H.264 video and converted audio to AAC. Chrome decoded it over trusted
HTTPS, both through native HLS/MSE and the production Mediabunny adapter. A
successful 120-second seek replacement took 27.1 seconds including gateway
preparation and destination frames. An earlier seek creation failed; consistent
reliability remains unproven. No matched speed comparison follows from these
single observations. See `GATEWAY_TORRENT_RUNTIME.md` for scope and revocation
proof. Full backend account/source-screen and physical Roku playback remain
separate acceptance work.

## Android phone and full-authority observations, 2026-10-09

A fresh API 36 phone emulator used the same real source as the TV qualification.
Its cold endpoint took 65.5 s and actual first frame 68.2 s. This slow observation
remains unresolved performance evidence. Retained cache reduced first frame to
1.61 s; a forward seek with fresh texture-frame confirmation took 9.83 s.
The immediate 1 ms position/discontinuity timer is excluded.

A separate full backend/core/app-cache-manager/Go/Media3 run rendered in 25.9 s
and joined retirement. It caught a production integration defect first: app
ownership markers must sit outside Go's strict piece/index directory. The fixed
boundary has actual-worker coverage. Network/cache paths differ, so these values
cannot be treated as matched cold/warm comparisons or Stremio speed claims.
