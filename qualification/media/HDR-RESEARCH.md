# HDR conversion research and qualification

Research started 2026-10-24. Production readiness remains unproven. The API and
consumer contracts remain unchanged. HDR passthrough is still a separate
[follow-up](HDR-PASSTHROUGH-NOTE.md), not a substitute for fast SDR conversion.

## Implementations being evaluated

| Implementation | Why evaluate it | Evidence needed before selection |
| --- | --- | --- |
| Existing zimg + CPU Mobius + QSV | Preserves current output; measured control | Frozen reference for speed, color, dimensions and quality |
| FFmpeg OpenCL | GPU processing for PQ and HLG; Intel media-surface interop can reduce transfers | Actual UHD 630 execution, shader compilation latency, surface import/export, color and full-flow tests |
| Jellyfin OpenCL | Its patch series adds LUT processing, BT.2390, and media interop changes | Correct patched build, TLS/range/codec regressions, repeated quality/performance comparisons |
| Vulkan/libplacebo | Library handles scaling, color conversion, tone/gamut mapping and HDR metadata | Actual GPU selection, explicit device isolation, linear-light scaling, bounded shader cache, native render checks |
| Jellyfin `tonemapx` | SIMD software conversion; potential fast fallback without a compute runtime | AVX2/FMA execution on the target CPU, PQ/HLG/DV coverage, scaling/color agreement, concurrent processing |
| Intel VPP | Fixed-function conversion with low CPU use | Revisit only with defensible color/quality results; earlier encoded-output comparison differs substantially |

[Jellyfin's Intel documentation](https://jellyfin.org/docs/general/post-install/transcoding/hardware-acceleration/intel/)
describes OpenCL and VPP, including their different tuning and runtime needs.
[Its FFmpeg feature list](https://github.com/jellyfin/jellyfin-ffmpeg/wiki/Features)
identifies optimized OpenCL, `tonemapx`, and interop patches. The actual pinned
patches and filter sources, rather than a feature list, determine what a build
contains.

[libplacebo](https://libplacebo.org/) is a GPU image-processing library already
integrated with FFmpeg. Its [color options](https://libplacebo.org/options/)
include several mapping algorithms and peak detection. A matching algorithm
name does not guarantee matching transfer functions, gamut handling, peaks, or
pixels. Explicitly control these when comparing to the existing pipeline.

The pinned upstream [OpenCL filter](https://github.com/FFmpeg/FFmpeg/blob/n9.0.2/libavfilter/vf_tonemap_opencl.c)
accepts PQ/HLG input. Its output transfer implementation and resize placement
must be checked against the current CPU pipeline. The
[libplacebo filter](https://github.com/FFmpeg/FFmpeg/blob/n9.0.2/libavfilter/vf_libplacebo.c)
exposes shader, scaling and color controls; device and runtime support still
require real execution tests.

## Findings that change the next action

1. The existing 30-second owned 4K PQ control improves by about 34% elapsed time
   and 42% CPU with QSV decode, but CPU tone mapping remains below real time on
   heavy full-height sources. More gateway buffering does not fix that capacity
   limit. Move conversion work to GPU or a proven optimized software path.
2. The earlier source-built Jellyfin candidate **did not apply
   `debian/patches/series`**. That omitted its HDR and interoperability changes.
   The old comparison is invalid as a Jellyfin comparison. Raw trials remain in
   `ffmpeg-comparison.json`; requalify the corrected build before making a new
   binary selection. The official package's default TLS-verification failure is
   separate evidence and remains valid.
3. The quality comparator's rawvideo/NUT reference lost color metadata. The
   corrected comparison retags only the known software reference. Original and
   corrected measurements of the same checksummed gateway responses are retained
   in `hdr-quality-20261023.json`. Actual gateway metadata remains an independent
   gate. The prior VPP comparison used two encoded BT.709 outputs and is not
   explained by this raw-reference issue.
4. The primary server is Coffee Lake/UHD 630. Intel documents a separate
   [legacy OpenCL runtime](https://github.com/intel/compute-runtime/blob/master/documentation/LEGACY_PLATFORMS.md)
   for this generation. Installing a current Gen12-only runtime does not prove
   support. Keep research runtimes inside owned images; do not change host
   drivers during the frozen baseline.
5. FFmpeg 9.0.2 requires libplacebo >=7.351.0. Debian Trixie's packaged 7.349.0
   is insufficient. Research build 7.360.1 also encountered Vulkan header
   incompatibility with the Trixie SDK. The next build pins 7.351.0, with source
   checksums and package inventories. Failed builds are retained.

## Measurement and decision rules

Pre-generate source windows and fix checksums before timed trials. Measure the
same target GPU, CPU/memory budget, source bytes, dimensions, frame rate, bitrate,
encoder, GOP, and duration. Separate shader/cache cold starts from warm starts.
Record actual decoder/filter/encoder commands and GPU engines. Keep all failed
attempts and fallback costs. Serialize trials with the formal Intel campaign.

Compare decoded SDR pixels, transfer/primaries/matrix/range, clipping, shadow and
highlight detail, motion, gradients, saturation, subtitles, and A/V timing. PQ,
HLG, compatible Dolby Vision base layers, metadata variants, 4K, high frame rate,
AV1 software decode, multiple outputs, long playback and cleanup all remain in
scope. No PQ-only success is a general HDR qualification.

PSNR against current output is a regression signal, not a universal perceptual
ranking across different tone curves. A standards-based curve that intentionally
changes the image needs separate references and visual validation. Do not hide
an output change by changing metadata or lowering a threshold.

The fastest passing implementation must also clear the original compatibility,
startup, throughput, concurrency and eight-hour soak gates. A local benchmark
cannot substantiate a world-record claim. Retain measured results and limits;
the requested optimization and production-readiness work remain active until
the complete candidate passes.

## Current execution

The frozen baseline continues. The queue now waits for a reviewed HDR successor
instead of treating C3, with its known heavy-HDR limit, as the final candidate.
Research image builds and corrected Jellyfin packaging are separate from timed
campaigns. `Dockerfile.hdr-research` and `Dockerfile.jellyfin-source` are research
recipes; neither selects a new production default automatically.

Private build/source receipts and retained logs:
`/mnt/ALPH/.viptv-qa/gateway-hdr-research-20261024/`.
