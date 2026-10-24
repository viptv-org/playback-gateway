# HTTPS media laboratory

This laboratory owns generated media, ephemeral accounts, gateway keys, processes
and Docker network namespaces. It exercises the current gateway API and current
backend routes without mocking responses or changing production consumers.

The main matrix contains 1,056 distinct recipes across 44 valid container/codec
profiles. `corpus.py` generates and fully decodes each asset before recording it
as valid. Invalid generation, missing assets, incomplete runs and unsupported
outputs remain explicit failures or unexecuted cases. They never count as passes.
Large payloads, credentials, screenshots and raw process arguments belong in a
private external workspace, not this repository.

## Prepare

```sh
python3 qualification/media/corpus.py manifest --root /absolute/private/corpus
python3 qualification/media/corpus.py generate --root /absolute/private/corpus
python3 qualification/media/public_sources.py --output /absolute/private/public-sources.json
```

Generation uses at most two workers, a 250-GiB corpus budget and a free-space
reserve. Every fixture records the exact encoder argv, inspection metadata,
full-decode result and payload checksums. Source patterns include moving texture,
different scene generators, one-second visual/audio markers and distinct noise
seeds. Resolutions range from SD through 4K, with 8/10-bit pictures, HDR transfer
tags, interlacing, variable frame rate, multitrack audio and text captions.

For licensed real-motion and speech/music material, use a **separate** corpus
root with `--master /absolute/master.mp4`. Its adjacent `master.json` must contain
`url`, `license`, and `attribution`. Master-derived files carry that provenance
and the master's SHA256; they are not mislabeled CC0 or timing-marker fixtures.
Public URLs are low-rate compatibility probes, never load-test origins. A
successful probe requires actual video inspection and decoding, not HTTP 200.
Redistribution rights remain asset-specific.

`live.py --source PATH --root CORPUS/live/continuous --seconds 30000` prepackages
continuous HLS, DASH and TS timestamps for over eight hours. `/live/` manifests
advance against an origin clock; `/paced/` TS delivery advances at its recorded
average bitrate. No encoder or asset download belongs in a timed run. HLS/DASH
source completion is finite and explicit; the origin never rewinds a timeline.
The origin also gates segment bytes by their publication time. Serving future
prepackaged DASH segments early lets a demuxer outrun the manifest and can
produce timestamp regressions. Live HLS begins with at least three target
durations; campaign identity follows manifest and segment requests.

Build an unchanged gateway binary first and copy it, its source revision, and
build receipt into the external `baseline/` directory. Preserve that binary
through every optimization. Limit Rust compilation to two jobs.

## Owned runtime

```sh
sudo docker build -f qualification/media/Dockerfile.lab -t playback-media-lab:20261023 .
python3 qualification/media/lab.py start --run /absolute/private/run \
  --corpus /absolute/private/corpus --binary /absolute/private/baseline/playback-gateway
```

The supervisor requires `--network none`, adds a public-classified fixture IP
**only inside that namespace**, and runs the real gateway as UID 10001. The
private CA bundle remains readable by that UID; verification is never disabled.
Input files, certificates and binaries are mounted read-only. State/output uses
a bounded tmpfs. A private Unix socket carries gateway requests to the local
browser ingress; there is no public control listener.

For Coffee Lake, build the existing `Dockerfile.intel-legacy` over the lab image,
then supply `--hardware qsv --device /dev/dri/renderD128 --image IMAGE`.
Software and AMD runs use distinct run directories and receipts. Requested QSV
does **not** establish acceleration: inspect observed producer commands and
runtime diagnostics. Do not modify device permissions or stop live services.

`remote.py` stages owned paths beneath a dedicated host cache directory and reads
SSH credentials from an explicit private environment file. Its `start`, `fetch`,
`status`, and `stop` operations act only on that run's recorded container ID.
Keep main assets on the workstation and stage only validated batches when the
Intel host has less free storage. An SSH Unix-socket forward connects the remote
gateway to the workstation's browser driver.

`network.py` can apply unrestricted, 50-ms RTT or twice-bitrate transport to the
owned upstream. It asserts network isolation before configuring `tc`; it does
not change host networking or introduce delay into unrelated control traffic.

## Baseline and final campaigns

The backend has an ignored `gateway::media_lab_acceptance::media_lab_backend`
fixture. Compile the backend library tests, preserve the exact executable, and
pass it to the browser runner. It seeds a disposable account, encrypted addon
configuration and a real gateway grant, then serves the actual application
router. No product API response is intercepted.

```sh
python3 qualification/media/lab.py run --run /absolute/private/run \
  --corpus /absolute/private/corpus --backend-binary /absolute/backend-test-binary \
  --tv-root /absolute/tv-web --certs /absolute/trusted-browser-certs \
  --phase baseline
```

The `kmp` driver serves an isolated build of the current Compose/Wasm app. It
uses real cookie login and profile selection during setup, then actual canvas
pointer actions located through the app's accessibility bounds. Catalog,
source selection, playback, renewals and release all use the unchanged backend
handlers. Native video uses compositor frame callbacks and a test-only PCM
observer. Pin `--web-dist` and `--browser-executable`; copy Playwright and its
actual headless executable so an unrelated package update cannot change a run.
The normal TV-served distribution is never a build target. The app's live source
caller now uses POST, matching the existing source-grant mutation route; a
focused regression test and the isolated Wasm build cover that correction.

The default `current-api` driver visits real bootstrap, catalog metadata,
incremental source discovery, source ranking, playback admission, state polling,
heartbeat and release routes. It plays the resulting HTTPS media using the repository’s unmodified
`Html5FallbackAdapter`, including its default MediaBunny/WebCodecs path and
existing fallbacks. `--player-profile mse` exercises the shared HTML adapter
with an explicit non-native-HLS capability profile; `tv-html` uses the browser’s
actual native-HLS capability. These profiles are recorded separately. `--driver product` additionally exercises the shipped
web UI, provided its API revision matches the backend. These are different
qualification levels: a test-driver pass must not be reported as a shipped UI
or physical Roku pass. The legacy UI currently calls retired backend routes;
its failures are retained, not hidden through a response adapter.

`browser.mjs` observes rendered frames, timeline progression, first media,
scheduled audio, renewals, releases and timing-marker A/V alignment. Audio uses
the browser output clock, not physical speakers. Raw evidence retains unknown
measurements as null. Fixture source code and browser/backend/gateway identities
are part of campaign provenance. `--limit` is a subset screen, never full acceptance.

Inside the owned container:

```sh
python3 /lab/measure.py --output /run/lab/startup --selection diverse --count 40 --trials 20
python3 /lab/measure.py --output /run/lab/throughput --selection diverse --count 40 --trials 1 --seconds 30 --reference --consumer-mode copy
python3 /lab/load.py --output /run/lab/concurrency
python3 /lab/load.py --output /run/lab/live-concurrency --live
python3 /lab/load.py --output /run/lab/soak --soak-hours 8 --live
```

The `formats` screen selects one low-resolution fixture per format. The
`diverse` selection deterministically covers resolutions, HDR, interlacing,
audio layouts and track arrangements; use it for full startup qualification. Cold trials use distinct
source identities, then warm trials reuse the same identity after output cleanup.
OS caches are not flushed, and observed probe processes are recorded separately.
Session cleanup observes the existing idle grace rather than changing it.
Gateway CPU/RSS, reaped-child CPU, active FFmpeg CPU/RSS and GPU fdinfo
counters are captured separately. The supervisor records one-Hz resource and
media-storage samples; the browser also records its own process CPU/RSS. Raw commands stay
private because media processes can contain capability URLs.

The reference control replays observed FFmpeg plans using the same encoder/filter
options and a minimal owned HTTP file origin. Shared-live controls require an
observed two-producer graph, its local replay inspection, and an explicit launch
relationship before accepting stdin-pipe replay. They align source age, include
both producers' wall time and reject ambiguous graphs. Sustained viewers are paced, renew leases and release
independently. Admission reclamation and compatible-job sharing are hard checks.

`campaign.py --root PRIVATE_ROOT` combines the validated main corpus, real-motion
variants, imported samples, long VOD and periodic live sessions into an ordered
campaign. `lab.py run --manifest CORPUS/full-campaign.json --cleanup-stride 20`
keeps one gateway alive with periodic full-capacity cleanup checks. The backend
fixture restarts between cases, so this establishes gateway soak evidence only;
it cannot establish continuous backend memory stability. `supplemental.py`
assembles a separate fixed catalog; `--persistent-backend --soak-hours 8` keeps
the actual backend process alive across those sessions. `resources.py` requires
backend/gateway process continuity, idle memory observations, sampling coverage,
bounded media storage and full admission reclamation. Interrupted soaks cannot
resume as continuous evidence.

`startup.py` assembles 40 × 20 cold/warm starts through the real backend and
browser. Fresh source identities define cold trials; each warm partner reuses
the same source/probe identity after normal output cleanup. Five seconds of
rendering follows every start. Browser process startup and service boot are
separate from the play-action clock. `load_clients.py` moves output decoding
outside the gateway's CPU quota and uses `remote_sample.py` to observe the Intel
server while clients run elsewhere. It records client CPU and binary identity
separately; use its formats, 1080p, 4K and clocked-live workloads for capacity.

`run_suite.py --config PRIVATE_JSON --credentials PRIVATE_ENV --output PRIVATE_DIR`
runs prepared Intel phases serially: full browser campaign, 40 × 20 cold/warm
starts, matched throughput, Roku HEVC, VOD/live concurrency and controlled
network profiles. Its config specifies `corpus`, `manifest`, `backend_binary`,
`tv_root`, `certs`, `origin`, and `phases` containing `name`, `local_run` and
`remote_run`. It maintains an owned SSH socket, freezes configuration and writes
stage receipts. Completed failures remain failures; incomplete stages stop the
suite. Interrupting it stops only the active owned lab container. Preserve the
interrupted evidence and use a fresh runtime for a continuous campaign.

`network_campaign.py` uses the inspected fixture's stored media bytes/duration
to set twice-bitrate delivery and resets shaping in a `finally` block. `inventory.py`
records matching FFmpeg/ffprobe hashes, build configurations, kernel, CPU, GPU
device/driver and media-runtime library hashes without recording environment
secrets. Run it in each owned container before interpreting hardware results.

Run extensive timing campaigns only for the frozen baseline and completed
candidate. Focused harness/unit correctness checks are separate. Resume receipts
are immutable: use a fresh run directory after a harness or binary change;
retain earlier failed setup and playback attempts.

## FFmpeg candidates and evidence

The default Dockerfile enables libxml2 for DASH input and dav1d for software AV1.
The original build's missing DASH demuxer and failing AV1 outputs are preserved
in baseline evidence. Both additions are internal media-build changes.

`Dockerfile.jellyfin` pins the official 8.1.3-1 package and its SHA256. That package
defaults TLS verification off and fails the existing gateway requirement.
`Dockerfile.jellyfin-source` pins its source commit/archive, changes the TLS
default to verified, and retains GnuTLS, range support, QSV/VAAPI and software
fallback. It now applies the complete pinned Debian patch series before building
and enables OpenCL/libplacebo for HDR research. The previous unpatched source
comparison is invalid as a Jellyfin comparison; its trial results remain visible.
See [HDR research](HDR-RESEARCH.md) for current findings and requalification.
`jellyfin.py` checks both ffmpeg and ffprobe. Actual TLS and media tests
are still required; listing an encoder or option is not runtime qualification.
Neither candidate changes the production image selection automatically.

`report.py` preserves paired failures/missing trials and reports startup
distributions and regressions. Acceptance also requires broad browser coverage,
40 fixtures with 20 cold/20 warm trials, readiness latency, matched throughput,
measured gateway overhead, A/V evidence, long playback, HDR quality, concurrency
and eight-hour soak evidence. Missing measurements fail closed. Never infer full
acceptance from a successful launcher, a subset screen or an FFmpeg capability list.

Focused harness checks:

```sh
python3 -m unittest discover -s qualification/media -p test_lab.py -v
CARGO_BUILD_JOBS=2 cargo test --locked -p playback-engine --lib output_changes
CARGO_BUILD_JOBS=2 cargo test --locked --workspace
```

## Important observed boundaries

HLS inspection combines stream metadata with a bounded keyframe sample in the
existing probe process when the URL identifies HLS. HLS can omit stream-level
field order even for interlaced H.264; decoded-frame evidence prevents an
incorrect copy decision. Extensionless HLS is sampled after demuxer identification
only when that evidence is missing. MPEG-2/VC-1 encodes with unspecified field
order use FFmpeg's conditional deinterlacer without another network inspection.

Readiness requires one delivered reload interval for VOD and two for live;
completed short files are accepted. The delivered target is at least two
seconds. A one-interval live experiment stalled in both the current app and the
older player; the extra interval covers independent upstream/replay reloads.
Retain every startup cost in the paired reports.

Copied legacy HLS no longer uses `split_by_time`: that cut valid GOPs into
fragments without decoder configuration or a keyframe. Live starts and late
joins could select one and fail MSE initialization or lose A/V alignment.
FFmpeg now cuts copies at keyframes; audio-only conversion still copies video.
A variable-GOP regression decodes every advertised fragment independently and
checks that target duration reflects the real GOP. The experimental TS stdin
reader was removed; the qualified path retains FFmpeg's private HLS reader.

Managed VOD playlists carry a standard start hint. The older shared WebCodecs
test profile starts correctly; the current KMP app's MSE path also fetches the
first segment. Chromium 153's native-HLS path was observed ignoring that hint
and can stall after starting from a later segment. Native-HTML diagnostics are
retained separately and are not reported as default-browser or Roku acceptance.
Managed-caption rendering is checked with the MSE profile; default-browser
format coverage leaves captions off, matching its supported playback path.

`extras.py` creates long VOD and a multivariant HLS fixture without modifying
the main frozen manifest. `live.py --corpus CORPUS` writes live descriptors after
fully demuxing the generated live packages. Use `lab.py run --manifest PATH` for
these supplemental campaigns. Live browser cases use the real M3U integration,
channel directory, guide status, source grants, ranking and playback routes;
compile the backend fixture with `--features live-runtime`. Live lag is measured
from the player to the available playlist edge, not as glass-to-glass delay.

Public VC-1 test samples can be cached explicitly with `import_samples.py`. Those
payloads remain private to the isolated test namespace; the source catalog does
not grant redistribution rights. Full decoding and immutable checksums are
recorded before browser playback.

## Current evidence and remaining qualification

`startup-screen.json` records the early matched Intel screen: 70/80 baseline
passes versus 80/80 candidate passes; paired median gateway startup 313.6 →
228.5 ms, and publication-to-response p95 144.5 → 6.0 ms. This is not the final
20-repeat campaign or browser first-frame latency. `ffmpeg-comparison.json`
records the separate Jellyfin comparison and decision to retain upstream FFmpeg.

The complete campaign is not yet qualified. Preserve broad-format failures,
capacity limits and incomplete gates. Legacy periodic A/V pulses measure phase
only. `timing.py` supplies six fixtures with matching visual/audio identities;
`clock.py` adds a 16-bit source clock for advancing live streams. The passive
observer matches identities, accounts for cancelled audio buffers and requires
at least 20 unambiguous pairs. Six focused 30-second browser checks passed with
7–23 ms drift. Clocked TS/HLS/DASH passed too; DASH's measured source-to-picture
delay was about 57 seconds, matching FFmpeg's timeline start policy. This is
retained as source/demux delay, not attributed to gateway processing.

The first corpus generation omitted PQ/HLG transfer and BT.2020 primaries from
18 HEVC samples. They are invalid HDR fixtures and cannot count as supported
playback passes. `qualify.py` audits metadata and checksums into a new corpus,
copies receipts and hard-links only immutable payloads. HDR recipe version 2
uses new IDs and explicit encoder VUI tags; corrected replacements require full
decode and an original-binary baseline. The frozen original files and attempts
remain untouched. The Jellyfin screen's two affected profiles count as 10-bit
unspecified-transfer coverage, not HDR qualification. `quality.py` compares
actual public output pixels and metadata to independent baseline output or a
bounded software reference; a partial set cannot satisfy full HDR acceptance.

The earlier adapter-only baseline is retained as incomplete diagnostic evidence.
Formal baseline/final comparisons use the same current KMP bundle, backend,
browser binary, corrected media and frozen driver. Neither development subset
results nor an interrupted diagnostic run satisfy full-campaign gates.

Use `qualify.py --source ORIGINAL_CORPUS --destination QUALIFIED_ROOT/corpus`
for a separate audited generation. Run `corpus.py generate` against that new
root, then `extras.py`, `campaign.py --root QUALIFIED_ROOT` and
`supplemental.py --corpus QUALIFIED_ROOT/corpus`. Cached payloads require matching
checksums and metadata before reuse. `remote.py stage --link-dest REMOTE_CORPUS`
reuses immutable server payloads while staging the corrected corpus outside
measurements. Do not mutate recipes or payloads in an active campaign.

Optional suite config keys are `browser_startup_manifest`, `timing_manifest`,
`clock_manifest`, `quality_manifest`, `caption_manifest`, `soak_manifest` and
`live_manifest`. `api_startup_trials: 1` keeps an 80-trial API control alongside
the required 1,600 actual backend/browser starts. Phase `stages` selects an
explicit supplemental subset. Baseline and candidate must use the same frozen
driver, source bytes and workload parameters for timing comparisons.

`stage_timings.py` correlates gateway trace IDs with browser observations;
`capacity_report.py` exports all viewer-count points to JSON, CSV and Markdown,
including failed or saturated points. `clock_sync.py` bounds cross-host clock
offset without altering either clock. Repeat it after long runs. Published
playlist diagnostics use the same opened inode as the readiness predicate,
avoiding a newer atomic playlist replacement hiding a fast response from the
external sampler. Node's test-only HTTP requests receive the trusted QA CA via
`NODE_EXTRA_CA_CERTS`; certificate and hostname verification stay enabled.

The test image supplies a separate fixture-generation FFmpeg through
`PLAYBACK_FIXTURE_FFMPEG`; the gateway and output decoder still use the exact
candidate binaries. This allows HEVC/PNG fixture generation without adding
unneeded production encoders. `real_tests.py --extended` retains existing media
regressions; `--test EXACT_NAME` supports focused retries with all earlier
receipts preserved. The existing torrent egress regression remains unqualified:
the small tmpfs cannot initialize its default cache, and a separate retry with
3 GiB tmpfs reached metadata resolution but found no peers. Neither attempt is
counted as a playback or egress-policy pass. `real_tests.py --tmpfs-mib` and
`--memory-mib` record explicit fixture limits for focused infrastructure checks.

External user-supplied HTTPS references can be checked without copying whole assets.
Keep a `{"fixtures":[{"id":"external-01","title":"Reference title","url":"https://…"}]}`
manifest in a private directory outside the repository. Run
`python3 qualification/media/http_sources.py --manifest PRIVATE_JSON --output PRIVATE_REPORT`.
This verifies TLS, follows HTTPS redirects, tests HEAD and a bounded 64 KiB ranged
GET, and reports status codes/container signatures without recording URLs or
response bodies. A failed HEAD does not disqualify a successful media GET.
Reachability is not a playback pass.

For actual browser acceptance, start a **separate** lab runtime with
`lab.py start --public-https-egress` and the usual explicit candidate/corpus/image
arguments. This opt-in uses Docker bridge egress without published container ports;
the default remains `--network=none`. Gateway source validation and TLS checks
remain enabled. The fixture addon's optional `url` field passes the original
external HTTPS reference through the real backend and gateway, including resolver
redirects. Its normal `file` field continues to address the owned origin. Supply
an externally validated receipt, stream expectations, and `timing_markers:false`
for ordinary media without generated timing markers, then use the existing
`lab.py run --driver kmp` with the isolated current app and pinned browser.
Keep manifests, selections, backend logs, and gateway state private: upstream
references and redirect destinations can contain credentials. External network
trials are functional checks and must not be treated as controlled-origin
performance comparisons. Release each lease and verify capacity reclamation.

`lab.py run --driver roku-api` exercises the existing Roku request shape through
the real backend: catalog/metadata discovery, source grants/ranking, gateway-only
playback, first-frame acknowledgement, heartbeat, playback events, and release.
A paced FFmpeg decoder runs on the workstation behind the trusted HTTPS ingress.
It decodes the delivered video/audio, checks codecs and the exact negotiated
resolution fit, verifies SDR BT.709/8-bit output for HDR inputs, and verifies
selected audio/subtitle delivery. Per-case `driver_sha256` records the actual
observer. `compatibility_passed` is separate from `performance_failures`: a paced
decoder progress gap remains a failed performance result while independent color,
codec, resolution, and cleanup checks still run. This qualifies the Roku wire
protocol and output; it does not assert physical Roku rendering. Actual browser
rendering continues to use `--driver kmp`.

A fixture's optional `client` object supplies the existing Roku 720p, 1080p H.264,
or 4K H.264/HEVC facts. The default is 1080p H.264/AAC with direct play disabled.
Use a private `.observer-candidate.json` with the existing `remote_sample.py`
configuration to retain actual producer commands and DRM video-engine counters.
Those command files remain private because they can include upstream credentials.

`lab.py start --cpus N` freezes an explicit CPU budget in provenance. Four CPUs
remain the default for matched performance controls. A separate six-CPU run can
qualify the full i5-9500 host budget; never mix those measurements into a claimed
code-only improvement. Serialize campaigns using the same GPU. Campaign-specific
Unix-socket aliases prevent one functional consumer's cleanup from unlinking
another consumer's transport.

Compatible Intel HDR sources now use QSV decode, one native-bit-depth download,
the existing linear-light CPU resize/Mobius tone mapper, and the existing QSV
encode upload. Unsupported GPU decode retries CPU decode with the QSV encoder
before software fallback. A proven QSV path does not probe an unused VAAPI tone
mapper, and tone-mapping status reflects the selected CPU filter. A two-run,
30-second, owned-HTTPS 4K PQ control measured about 34% less elapsed processing
and 42% less CPU, with 48.1 dB decoded-output PSNR against the prior path. This is
a focused result, not the completed compatibility or capacity campaign. The faster
VPP tone-mapping experiment measured 27.3 dB and was not selected.

The [HDR passthrough follow-up note](HDR-PASSTHROUGH-NOTE.md) records the user's
questions about avoiding SDR conversion on HDR-capable clients. It is a future
qualification consideration and does not change current playback behavior.

Rawvideo software references stored in NUT lose color metadata. The quality
comparator restores the reference's known BT.709 limited-range properties before
PSNR so automatic format negotiation does not introduce an unrelated color
conversion. Captured gateway and baseline media are never retagged; incorrect
gateway color metadata still fails independently. Original scores remain in the
report alongside corrected analysis of the same checksummed response bytes.
Run the three real-media comparator regressions with explicit
`PLAYBACK_TEST_FFMPEG` and `PLAYBACK_TEST_FFPROBE` paths using
`python3 -m unittest discover -s qualification/media -p test_quality.py -v`.
