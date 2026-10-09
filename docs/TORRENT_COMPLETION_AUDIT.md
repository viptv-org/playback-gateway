# Shared torrent refactor: remaining qualification

Checkpoint 2026-10-09. The owner's latency-parity gate is deferred; correctness,
cache limits, authority and platform integration remain required. Production
rollout and physical-device testing are separately coordinated.

| Requirement | Current authoritative evidence | Remaining work |
|---|---|---|
| Shared unmodified upstream engine | Go module/lock and library-boundary audit | Keep pinned upgrade qualification |
| Verified bounded reusable cache | Go race fixtures for storage, eviction, crash reuse and corrupt data; large native/gateway files | Sustained public multi-viewer trials |
| Discovery hints, trackers/DHT/PEX/TCP/uTP | Go discovery/policy fixtures and duplicate-hint test | Public source variability recorded |
| Explicit/largest selection, stored multipart RAR4/5 seek mapping | Go selection/archive fixtures, independent inner selection and refusal cases | Physical decoder archive trial |
| Native v2, strict v1 compatibility | Core native/WASM vectors, backend protocol tests | Full authenticated source-screen playback qualification |
| Android/TV local acquisition, no gateway fallback | Go/JNI all three ABIs, API24/16KiB APK checks; Media3 emulator frames and seek | Phone presentation and physical ARM tests |
| Linux desktop local acquisition | Bundled application Go→Rust→MPV/GStreamer rendered frames; seek trials | Full account/source-screen path and sustained playback |
| Windows desktop packaging | Windows installer build and release suite; extracted worker/notices match hashes | Direct worker test on Windows currently running; TextureStream playback unverified |
| Web gateway playback | Real HTTPS backend profile/vault/add-on→production API/controller→Mediabunny frames and 401-stop proof; separate seek replacement proof | Ordinary account/catalog screens and physical TV trials |
| Roku gateway delivery | Roku static/runtime/BrightScript suite and package; real gateway HLS decode/seek | Actual physical Roku presentation (separate coordination) |
| 120-second startup, 60-second no progress | Go deadlines; native/core/cancellation tests; gateway/web/Roku renewal and propagated budgets | Measured gateway stages now wired through scoped routes to web/Roku; real backend/browser flow passes |
| Independent lifecycle/authorization | Core/backend authority tests, Go independent handles, gateway shared-viewer/seek/cancel fixtures | Cross-account process-death/recovery in full clients |
| Remove obsolete transport after replacement qualification | Production clients use Go; gateway acquisition uses Go | Legacy v1/rollback transport retained until the remaining gates pass |

This audit does not mark the full goal complete. No build, pushed branch or
single successful source proves all public playback fixed. Performance follow-up
remains in `playback-gateway/docs/TORRENT_PERFORMANCE_FOLLOWUP.md`.
