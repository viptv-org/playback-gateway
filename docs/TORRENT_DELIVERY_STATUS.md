# Shared torrent delivery status — 2026-10-09

Implementation is pushed on one `refactor/torrent-runtime` workstream per owning
repository. Header workstreams remain separate. The shared production Go code
and all consumer artifact pins use
`c7bb3f1f9a332c20aa8f81aa165d053c35fa7277`. Performance parity is deferred by the
owner; production rollout and physical-device qualification remain separate.

| Requirement | Evidence and limits |
| --- | --- |
| Shared engine and reusable verified storage | Full Go race suite and vet pass. Owned tracker/DHT/uTP, slow/corrupt/handshake, eviction, independent authority and killed-process recovery fixtures pass. A 1 GiB file streams through a 600 MiB filesystem with an 88 MiB cache and the 512 MiB free-space reserve intact. |
| Stored RAR4/RAR5 and multipart seeking | Stored ranges, seven-volume multi-member selection, largest-member ties, missing/malformed/compressed/encrypted refusals pass with original byte fixtures. |
| Backend/core v2, strict v1 compatibility | Separate v2 negotiation, exact selection, authority renewal/revocation, scoped progress and pending cancellation are implemented and tested. Native failures cannot fall back to the gateway. |
| Android phone and TV | Three-ABI/API24/16-KiB artifacts and normal APK checks pass. Owned x86_64 phone/TV emulators rendered real media; the phone backend/core/coordinator path rendered in 25.9 s and joined retirement. ARM/API24/16-KiB-page execution remains unverified. |
| Linux desktop | Actual backend/TvApi/core/native-worker/GStreamer playback, automatic renewal, seeks, Stop and a saved-position restart passed in an owned window. Ordinary login/catalog navigation remains outside the harness. Earlier cold/seek failures remain recorded. |
| Windows desktop | Both installers and 45 race-enabled runtime fixtures pass on Windows 2022, including verified rolling reads, discovery, eviction and killed-process reuse. Native TextureStream video presentation remains unverified. |
| Gateway and web | Real Go/FFmpeg fixtures and authenticated HTTPS browser presentation pass. A complete backend/API/controller probe reached frames in 18.37 s and revoked media on Stop. |
| Roku | Scoped startup stages, renewal, cancellation, remaining startup budget and packaging checks pass. Actual physical Roku delivery remains unverified. |
| Production dependency cleanup | Normal Android APKs and the Rust gateway dependency graph exclude rqbit. Explicit comparison artifacts and inherited tests remain available. Inactive legacy gateway cache is reclaimed under its ownership lock; Go records are preserved. |

Fresh installer jobs are
[desktop 38003799432](https://github.com/viptv-org/desktop/actions/runs/38003799432)
and [Android 38004862330](https://github.com/viptv-org/android/actions/runs/38004862330).
The desktop run passed on both platforms; Windows passed seven release tests
with one LAN fixture ignored, including actual pinned worker control/settlement.
Android also passed its replacement hosted run at `54323de`: unit tests,
three-ABI compilation, optimized APK/lint and real APK integrity checks passed.
The first Android hosted attempt exposed a loopback fixture shutdown race. Its
released latch wait could still be interrupted; a smaller regression reproduced
it. Graceful fixture joining fixes it without changing native cancellation or
stale-result assertions, and the full local unit suite passes.

The final decoder-acknowledgement consumer build at Android `339403b` also passed
[run 38013968770](https://github.com/viptv-org/android/actions/runs/38013968770),
including unit tests, all three ABIs, optimized APK/lint and APK integrity checks.
Roku `c331db4` passed compilation and ZIP packaging in
[run 38013970325](https://github.com/viptv-org/roku/actions/runs/38013970325).
Desktop `b729368` is being checked by
[run 38013967354](https://github.com/viptv-org/desktop/actions/runs/38013967354);
Both platforms passed. The subsequent native heartbeat fix
is adopted by desktop `372c585` and passed both platforms in
[run 38015306679](https://github.com/viptv-org/desktop/actions/runs/38015306679).
The pinned race-test capsule also passed 45 top-level Windows runtime tests in
[run 38016897210](https://github.com/viptv-org/desktop/actions/runs/38016897210).
Earlier capsule setup failures occurred before fixtures ran (cross-repository
token scope, then PowerShell argument quoting); they are not engine failures.

## Desktop heartbeat regression

Manual desktop testing caught native renewals sending `{}` to the backend's
bodyless control route. The resulting HTTP 400 occurred about twenty seconds
after a zero-position admission, including without a seek. TV-web `11329bd`
corrects private native HTTP serialization; ordinary gateway renewal retains its
existing JSON request. Regression cases at zero and saved positions reproduce
the exact error before the fix and pass afterward through the real TvApi and
actual WASM holder. All 304 frontend tests, five trusted-HTTPS UI cases, the native
backend-platform renewal test and twelve desktop Rust tests pass (one unrelated
LAN fixture remains ignored). The rebuilt local desktop app is running for owner
testing. This addresses control renewal; variable decoder seek performance
remains recorded separately.

The subsequent actual backend/core/Tauri HTTP/Go/GStreamer run passed three
automatic renewals with 200, measured forward/backward seek frames, Stop and a
new authorized saved-position start. Cold frames took 15.90 s; forward/backward
seeks took 3.95/0.77 s in this one source observation. The owned Xvfb app and
ephemeral account/cache stack were retired; the owner's manual app remained
running. This closes the desktop backend-heartbeat qualification gap without
claiming ordinary login/catalog coverage or general seek reliability.

## Decoder acknowledgement qualification

Native clients acknowledge the local runtime from presented-frame events.
[Gateway startup v2](TORRENT_STARTUP_V2.md) waits for a scoped decoder
acknowledgement, preserves lease expiry and isolates legacy/v2 source handles.
Backend brokering and web/Roku callbacks are integrated. V1 keeps its original
output-ready compatibility behavior.

An actual backend/Go/Mediabunny HTTPS run presented frames before acknowledgement
and revoked media with 401 on Stop. Admission/player opening took 17.85 s in that
single observation. Five HTTPS UI tests include metadata readiness with zero
frames followed by one acknowledgement after the frame count advances. Backend
tests cover source revocation, route-mode idempotency and unchanged authority
expiry. Roku controlled checks cover actual production event handlers, successful
seek replacement, coalescing and stale responses; physical Video-node evidence
remains separately coordinated. Core `0f3d9f7` is adopted by backend, Android,
TV-web and desktop; Go artifact pins remain unchanged.

This ledger does not establish universal public-swarm reliability or completion
of the remaining Windows/Roku/device qualification. Detailed measurements and
excluded observations remain in [qualification](../torrent-runtime/QUALIFICATION.md),
[gateway evidence](GATEWAY_TORRENT_RUNTIME.md) and
[performance follow-up](TORRENT_PERFORMANCE_FOLLOWUP.md).
