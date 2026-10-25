# HDR tone-map path research: keep the current CPU Mobius filter

Recorded 2026-10-24 during the HTTP media qualification campaign. Question: is
there a faster HDR-to-SDR tone-map path that preserves output quality and could
make heavy 4K HDR real-time on the Intel i5-9500 (UHD 630) production host?

## Verdict

Keep the existing path: QSV decode where proven, then the unchanged CPU
`zscale + tonemap=mobius` filter, then QSV encode. No alternative qualified.

## Evidence (private QA directory, not committed)

`/mnt/ALPH/.viptv-qa/gateway-hdr-research-20261024/DECISION.json` with frames
and PSNR runs. Summary:

- **jellyfin `tonemapx` (SIMD CPU)**: about 33% faster than the current chain
  on the workstation and vivid on the synthetic testsrc ground truth, but
  hue-shifts real BT.2020 PQ content (gold -> chartreuse) in both chain
  orderings. Rejected on the preserve-quality gate.
- **jellyfin FFmpeg 8.1.3-1 build** (patched source, TLS verification on):
  startup comparison vs upstream FFmpeg 9.0.2 showed no qualifying benefit
  (median deltas -32ms to +31ms, mixed signs) and 6/6 failures on a 4K60
  H.264 MP4 fixture. All 7 real-media/TLS regression tests pass once the test
  image includes bubblewrap. Rejected as the shipping binary.
- **Intel VPP tone map**: 27.3 dB PSNR versus the matched reference in the
  earlier paired measurement. Rejected.
- **libplacebo over Vulkan**: visually matches the current path on real
  content and is vivid on the synthetic pattern, but adds a Vulkan/shaderc
  dependency class and was not needed by the acceptance gates. Deferred.
- **jellyfin `tonemap_opencl` (OpenCL GPU tone map)**: measured on the Intel
  UHD 630. It runs (unlike VPP) and is ~46% faster than the CPU path on 4K PQ
  (41s vs 76s for 30s), and renders the saturated synthetic pattern vividly
  where the current path washes out. But it is not shippable here: the
  QSV<->OpenCL zero-copy surface mapping is unusable on Gen 9.5, so frames must
  take a download->upload->download detour, and the kernel hard-fails on
  non-mod-16 chroma heights (3840x1606 letterboxed content errors out). Kept
  out of C4; a dimension guard plus a real-content quality pass could revisit
  it later.
- **Current CPU Mobius path**: correct on real content; washes out only the
  extreme synthetic saturated pattern. Heavy full-frame 4K HDR stays below
  real time on the i5-9500; direct FFmpeg reproduces the limit, so it is an
  attributed FFmpeg/CPU bottleneck, which the campaign plan allows.

The candidate retains the earlier measured win: hardware QSV decode feeding
the unchanged CPU tone mapper (-34% wall, -42% CPU versus software decode on
paired real 4K PQ controls, 48.1 dB PSNR matched output).
