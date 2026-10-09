# Shared torrent delivery status — 2026-10-09

Implementation is pushed on one `refactor/torrent-runtime` workstream per owning
repository. Header workstreams remain separate. The shared production Go code
and all consumer artifact pins use
`c7bb3f1f9a332c20aa8f81aa165d053c35fa7277`. Performance parity is deferred by the
owner; production rollout and physical-device qualification remain separate.

| Requirement | Evidence and limits |
| --- | --- |
| Shared engine and reusable verified storage | Full Go race suite and vet pass. Owned tracker/DHT/uTP, slow/corrupt/handshake, eviction, independent authority and killed-process recovery fixtures pass. |
| Stored RAR4/RAR5 and multipart seeking | Stored ranges, seven-volume multi-member selection, largest-member ties, missing/malformed/compressed/encrypted refusals pass with original byte fixtures. |
| Backend/core v2, strict v1 compatibility | Separate v2 negotiation, exact selection, authority renewal/revocation, scoped progress and pending cancellation are implemented and tested. Native failures cannot fall back to the gateway. |
| Android phone and TV | Three-ABI/API24/16-KiB artifacts and normal APK checks pass. Owned x86_64 phone/TV emulators rendered real media; the phone backend/core/coordinator path rendered in 25.9 s and joined retirement. ARM/API24/16-KiB-page execution remains unverified. |
| Linux desktop | Real worker/native decoder presentation and seeks were exercised in an owned app window. Cold and seek results remain variable; full account/source-screen navigation was outside this harness. |
| Windows desktop | Hosted builds and actual pinned worker startup/refusal/shutdown checks exist. Native TextureStream video presentation remains unverified. |
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

## Remaining lifecycle audit

Native clients acknowledge the runtime from actual presented-frame events.
The gateway currently sends the same private `first_frame` operation when
FFmpeg output preparation succeeds (`RuntimeSource::output_ready`). Public
clients preserve their original 120-second budget through presentation, but
that internal acknowledgement retires the Go startup/progress watchdog before
the web/Roku first frame. [Gateway startup v2](TORRENT_STARTUP_V2.md) adds a
separate opt-in route that waits for a scoped decoder acknowledgement, preserves
lease expiry and isolates legacy/v2 source handles. Backend brokering and
web/Roku actual-frame callbacks still need adoption and end-to-end qualification.
This lifecycle work is distinct from the owner-deferred acquisition/seek performance.

This ledger does not establish universal public-swarm reliability or completion
of the remaining Windows/Roku/device qualification. Detailed measurements and
excluded observations remain in [qualification](../torrent-runtime/QUALIFICATION.md),
[gateway evidence](GATEWAY_TORRENT_RUNTIME.md) and
[performance follow-up](TORRENT_PERFORMANCE_FOLLOWUP.md).
