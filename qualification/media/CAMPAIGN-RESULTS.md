# HTTP media campaign results — candidate C4

Verdict and evidence for the playback-gateway media campaign. Generated
2026-10-24/25 from the frozen harness and retained private QA evidence under
`/mnt/ALPH/.viptv-qa/gateway-media-20261023/` and
`/mnt/ALPH/.viptv-qa/gateway-hdr-research-20261024/` (both private; not in git).

## Candidate

`candidate-c4` = current `main` HEAD `db705a4` (clean tree). Contents:
event-driven FFmpeg output readiness, DASH + AV1 in the bundled FFmpeg,
keyframe-aligned copied HLS, VOD whole-playlist + start hint, live readiness
buffering, decoded-frame interlace detection, and QSV HDR decode with the
unchanged CPU Mobius tone mapper.

## Measured gains (paired, same harness/host)

- Startup to ready: median **313.6 -> 228.5 ms (-27%)**, p95 1980 -> 1896 ms,
  70 paired trials, **zero regressions**.
- Publication-to-response p95: **145 -> 6 ms** (event-driven readiness).
- Fixed all **10 baseline format failures** (AV1, DASH) — the old FFmpeg could
  not decode them at all; the candidate plays them.

## Coverage

- **359 stratified fixtures** covering every container x codec x transfer x
  layout class played end-to-end through the gateway (API + real output
  decode, cold + warm).
- **30-fixture real-app browser acceptance** (KMP app + real backend + C4):
  27/30 passed. Failures: 2 heavy 4K HDR CPU-bound, 1 caption-UI test artifact
  (media played fine).
- **Session cleanup**: 30/30 isolated sessions reclaimed; no leak.
- Your **10 HTTPS streams** and the **50-source Roku-profile** set played.

## Known limits (attributed, documented)

- **Heavy 4K HDR** (PQ tone mapping) is CPU-bound on the i5-9500 under the
  4-CPU lab cap: below real-time, and it cannot sustain several concurrent 4K
  HDR transcodes on 4 CPUs. In isolation a 4K PQ title plays (9.3s startup).
  This is the FFmpeg/CPU boundary the plan allowed. OpenCL tone mapping
  measured ~46% faster here but is fragile on Gen 9.5 (fails letterboxed
  content; QSV<->OpenCL zero-copy unusable), so it is documented, not shipped.
- The 8-hour continuous soak from the original plan was **not run** (cut at
  the user's direction). Leak coverage comes from the lifecycle tests and the
  isolated reclaim checks; multi-hour stability is not separately proven here.

## Production readiness

C4 is a measured improvement over the current production binary on every axis
tested and is safe to deploy for the supported format matrix. Deploy via the
normal procedure (`backend/DEPLOYMENT.md`) and confirm the deploy result (see
`ops/DEPLOY_NOTES.md` — past deploys reported success while failing).
