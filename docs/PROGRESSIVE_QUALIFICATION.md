# Progressive live delivery qualification

Built-image evidence for [progressive live delivery](../API_V1.md#progressive-live-delivery)
(playback-gateway#1). This records named disposable fixtures. It is not a
deployment or a universal playback approval.

**Current supported subset (2026-10-02, playback-gateway#11):** same-container
live MPEG-TS and Matroska remuxes whose private replay matches the input
container. Matroska readers use the joined replay described below. Subtitle
selection, byte ranges, seeking, encoding, and other live containers remain
unsupported. The September runs below preserve the original failure and
interim refusal evidence; they describe older revisions.

## Matroska join qualification (2026-10-02)

Runtime source `0382030` was freshly built with the repository Dockerfile and
`CARGO_BUILD_JOBS=2` in the throwaway Rust build stage. Local image
`viptv-gateway-qual:mkv-stage1-0382030` has content ID
`sha256:48be718a12d4e3641b38f28de76391f040c05a790fa7de45edd04c434ac82d6f`.
The final image matches the earlier `6b8525b` runtime byte for byte: subsequent
changes adjust fixtures and documentation. Fixture/script source is `133fa4a`.
The image supplies FFmpeg/ffprobe 9.0.2.

Each Matroska progressive reader receives one EBML header, an unknown-size
Segment, Info and Tracks once, followed by only Clusters from listed private
segments. The private replay on disk and shared HLS readers keep their existing
path. The reader starts at the second-newest listed segment, which begins on a
keyframe. Missing/evicted segments, truncated layouts and replay teardown fail
the stream rather than skipping media. Live bodies retain error termination on
release, eviction and failure.

The native FFmpeg fixtures initially stalled because their provider repeated a
12-second complete Matroska file, introducing extra EBML headers and resetting
source timestamps. A single 240-second source, held open after its bytes are
sent, exercises a continuous live container. No additional engine fix was
required. Both FFV1/FLAC and H264/FLAC worker fixtures pass with two viewers on
one upstream, late keyframe decode, continued first-viewer delivery, bounded
slow-reader eviction and last-release cleanup. The three existing Matroska HLS
sharing/budget fixtures also pass (five tests together: 78.35 seconds).
The original image fixture also let its 60-second viewer leases expire while
waiting for a low-rate Matroska socket to fill. It now renews viewers intended
to remain active, then stops renewal at explicit release. Media consumption
still never renews a lease; the separate expiry qualification is unchanged.
MPEG-TS progressive passes in 30.28 seconds. The exact-reader join fixture
passes in 0.67 seconds, confirming continuous timestamps and full-segment
coverage for FFV1 and H264.

`cargo fmt --check`, strict all-target Clippy, and `cargo test --workspace`
pass on this isolated pre-torrent revision: 105 engine, 28 control and one main
binary test; 45 real-media tests remain opt-in.

On this host, the kernel cannot create bridge veth pairs, so image builds use
`PLAYBACK_TEST_BUILD_NETWORK=host`. This affects build-time package downloads;
all qualification runtime containers still use `--network none`. For example:

```sh
sudo -n env PLAYBACK_TEST_BUILD_NETWORK=host bash scripts/check-progressive-container.sh IMAGE matroska
```

### Built-image results

| Mode | Assertion | Result |
| --- | --- | --- |
| `matroska` | FFV1/FLAC delivery, late decode, isolation, eviction and reap | pass, 138.18 s |
| `matroska-tls` | same through the verified fixture TLS proxy | pass, 136.85 s |
| `bounded-matroska` | real ENOSPC, body error, reclaim and fresh playback | pass, 20.12 s |
| `ts` | H264/AAC delivery, late decode, isolation, eviction and reap | pass, 88.70 s |
| `bounded` | MPEG-TS real ENOSPC, body error, reclaim and fresh playback | pass, 25.13 s |

Both full Matroska runs decode 20 video and 94 audio frames from the first two
seconds of a late viewer, with a keyframe as its first video packet. Slow
readers fail with `processing_failed` after 82.04 seconds (HTTP) and 81.87
seconds (TLS); the raw response drains 2,243,698 buffered bytes and has no
clean chunked terminator. The renewed late viewer keeps advancing and the
provider is never reopened. Final release reclaims the two media children,
upstream, replay storage and 1/4/5 input/output/viewer reservations in 16.27
seconds (HTTP) and 14.72 seconds (TLS). SIGTERM exits cleanly.

The MPEG-TS run repeats the full viewer assertions with 50 video and 94 audio
frames decoded, slow-reader failure after 36.81 seconds, 1,866,062 buffered
bytes without a terminating chunk, and final teardown in 15.05 seconds.

Both bounded runs verify actual service UID/GID 10001, one ingest
and one reader on a 2 MiB media tmpfs. Real ENOSPC at 2,097,152 allocated
bytes makes readiness fail and refuses fresh admission. The affected viewer
gets `processing_failed`, and its response ends with a transport error.
Children, upstream and reservations are reclaimed; removing the fill file
allows fresh progressive playback. The MPEG-TS and Matroska assertions are
identical. No clean EOF is accepted as failure
handling.

## Image and source

- Runtime source: `eb9978d` (merge of #5), built from a Git archive with the
  repository `Dockerfile`. The only build-time difference was
  `ENV CARGO_BUILD_JOBS=2` in the throwaway `app-build` stage, which does not
  reach the final image.
- Local image `viptv-gateway-qual:progressive`, content ID
  `sha256:37b6b5772b5ddf9b3fb552cef0b47a5e93005c16285b8d1fbc0cd9f04decb9c3`
  (FFmpeg 9.0.2 from the image).
- Fixture source: `72547dd` on `test/progressive-image-qualification`. The
  fixture image is built from a Git archive of that revision; documentation
  commits after it do not change the archived fixture inputs.

## Harness

```sh
sudo -n bash scripts/check-progressive-container.sh IMAGE ts
sudo -n bash scripts/check-progressive-container.sh IMAGE ts-tls
sudo -n bash scripts/check-progressive-container.sh IMAGE bounded
sudo -n bash scripts/check-progressive-container.sh IMAGE matroska
sudo -n bash scripts/check-progressive-container.sh IMAGE bounded-matroska
```

The script reuses the exact-image runtime fixture
(`scripts/runtime-fixture.Dockerfile`) and, for `*-tls`, the fixture-only CA,
leaf and Python TLS proxy from `scripts/subtitle-fixture.Dockerfile`. Each run
uses one disposable `--network none` container (root plus `NET_ADMIN` only to
add the synthetic public-classified loopback address), runs exactly one named
opt-in test from `control/tests/runtime.rs`, requires `1 passed`, and removes
only that container and its anonymous volumes. The gateway inside is the
image's `/usr/local/bin/playback-gateway` with the image's FFmpeg/ffprobe. The
`bounded` modes run the gateway as UID/GID 10001 with a 2 MiB media tmpfs.

`PLAYBACK_TEST_PROGRESSIVE=1` gives the fixture source a real audio track (AAC
in MPEG-TS, FLAC in Matroska), a 240-second clip paced in real time so no test
observes the source loop, `MAX_OUTPUTS=4`, and key quotas of 1 input, 4 outputs
and 5 viewers.

## Results (2026-09-30, first run on `eb9978d`)

| Mode | Test | Result |
| --- | --- | --- |
| `ts` (HTTP) | `real_progressive_image_share_late_join_evict_isolate_and_reap` | pass, 89.2 s |
| `ts-tls` (HTTPS) | same | pass, 86.7 s |
| `bounded` | `real_progressive_kernel_bounded_media_full_fails_and_reclaims` | pass, 25.4 s |
| `matroska` | `real_progressive_image_share_late_join_evict_isolate_and_reap` | **fail** |
| `bounded-matroska` | `real_progressive_kernel_bounded_media_full_fails_and_reclaims` | **fail** |

### MPEG-TS over HTTP and HTTPS (`ts`, `ts-tls`)

The two runs make the same assertions. In `ts-tls`, the provider and the
control/media API are both reached over verified TLS through the fixture CA.

- An `hevc`-only request gets `422 unsupported_output`.
- The first viewer gets `format=mpegts`, `mode=remux`, video and audio `copy`,
  and a `stream.ts` URL. `HEAD` returns `video/mp2t`, `Accept-Ranges: none`
  and no length. `GET` returns `200` with `Accept-Ranges: none` and no
  `Content-Length`, and the body starts with a TS sync byte. A second `GET` on
  the same viewer is refused.
- A late viewer created three seconds later uses the same single upstream
  connection, with no new provider request. The first viewer's media token is
  refused on the late viewer's URL. The late viewer's first 1 MiB demuxes as
  exactly `h264` video and `aac` audio. The first video packet is a keyframe,
  and FFmpeg decodes the first two seconds with `-xerror` (50 video frames and
  94 audio frames). The first viewer keeps receiving media meanwhile.
- **Namespace isolation.** A key scoped to namespace `other`:
  - cannot create in `fixture`;
  - gets its own upstream for the same URL (2 active connections, its own
    preparation and ingest requests);
  - cannot read a `fixture` viewer;
  - can call release on a `fixture` viewer, which is the documented
    idempotent no-op: the viewer stays `ready` and keeps streaming.

  The `fixture` key cannot read the `other` viewer. Neither viewer's token
  opens the other's media.
- **Slow-reader eviction.** A viewer opens its URL over a raw socket with a
  4 KiB receive buffer and stops reading after the headers. It becomes
  `failed`/`processing_failed` about 36.7 s after the stall began (20 s of full
  channel, after the kernel and HTTP buffers filled). Its body then ends after
  about 1.87 MB of already-buffered bytes, without a clean chunked terminator.
  The late viewer keeps advancing during and after the eviction. The provider
  is never re-opened.
- **Release.** Releasing the first viewer during the stall ends its stream
  about 14.6–14.8 s later, which is the idle grace. The other viewers keep
  streaming. Releasing the `other` viewer closes that namespace's upstream.
- **Last release.** Releasing the last viewer ends its stream. The upstream
  closes, both media children (the ingest and the reader) are reaped, the
  media root is empty, and the key's capabilities return to 1/4/5 available,
  all within about 15 s. SIGTERM then exits the service cleanly.

### Hard storage bound (`bounded`, MPEG-TS)

- The gateway runs as UID 10001 with a 2 MiB media tmpfs. One progressive
  viewer streams, and the shared replay sits on that mount.
- Exactly one ingest and one reader run.
- A fixture file fills the mount until a real `ENOSPC` at 2,097,152 allocated
  bytes. `/ready` then returns `503`, and a new progressive create gets
  `not_ready`.
- The viewer becomes `failed`/`processing_failed` and its stream ends with an
  error. Both processes are reaped, the upstream closes, and reservations
  return to 1/4/5.
- After the fill file is removed, a fresh progressive viewer streams again,
  and SIGTERM exits cleanly.

### Defect found: Matroska progressive stalls on the built image (first run)

On the built image, an FFV1/FLAC Matroska progressive viewer gets
`format=matroska` and `remux`/`copy`. It receives about 220 KB, then no more
bytes, while the private replay keeps growing and the reader process stays
alive. Its session stays `ready`. The fixture fails after 30 s without
progress.

A standalone reproduction inside the image uses the gateway's exact segment
muxer and progressive reader arguments. It shows the cause: the HLS demuxer
concatenates the private `segment-*.mkv` files into one Matroska byte stream.
Each segment is a complete Matroska file, so the demuxer meets a second EBML
Segment, logs `Unknown element 18538067 … Seek to desired resync point failed`,
and resyncs to an earlier point. That produces backward timestamp
discontinuities of about 10 s per stream. Encoding readers (`-c:v libx264`)
tolerate this by re-offsetting timestamps, at the cost of re-reading data. The
copy-only Matroska muxer stalls instead.

In `bounded-matroska`, storage exhaustion does fail the viewer
(`processing_failed`) and the tmpfs hits `ENOSPC`. However, the viewer's stream
ended cleanly instead of with an error, so the test fails there.

The clean end was a generic progressive defect, not Matroska-specific: a
stream finished its body cleanly whenever the producer dropped its channel
before the HTTP body observed the close, or when the reader process exited
successfully after its replay was torn down. It is fixed for every progressive
container (see the re-run).

## Historical Matroska progressive refusal (2026-09-30)

Decision (2026-09-30, technical and reversible): the supported progressive
subset is narrowed to live MPEG-TS. A live progressive request whose input is
Matroska, or whose private replay would be Matroska, fails at request time with
`422 unsupported_output`, before any ingest, reader, replay or reservation
starts. The private Matroska replay and its shared HLS outputs are unchanged.

**Root cause.** The private Matroska replay is a segment muxer that writes every
`segment-*.mkv` as a complete Matroska file (EBML header, Segment, Tracks,
Clusters). A progressive reader opens the private M3U8 with FFmpeg's HLS
demuxer, which concatenates the segment files into one byte stream for a single
Matroska demuxer. At each segment boundary that demuxer meets a second EBML
header inside its Segment, fails to resync forward, and jumps back about 10 s.
A copy-only Matroska muxer cannot absorb the backward timestamps and stalls.
Encoding HLS outputs re-offset timestamps and continue, at the cost of
re-reading duplicated data.

**Proposed fix** (follow-up issue "Matroska progressive live delivery via
joinable replay"): make the replay joinable by a copy-only reader, either

- a header file (EBML header, Segment and Tracks written once) plus media-only
  segments that contain only Clusters, so header + any suffix of segments is
  one valid Matroska stream; or
- a reader that opens each segment as its own demuxer input (per-segment
  opening) and re-stitches packets with continuous timestamps.

Either change touches the private Matroska adapter that shared HLS outputs
also use, so the HLS Matroska fixtures must be re-run with it. Then re-enable
Matroska in the progressive subset and run the `matroska`, `matroska-tls` and
`bounded-matroska` modes against a rebuilt image.

## Re-run after narrowing to MPEG-TS

- Runtime source: `0220483` (Matroska progressive refused; failed progressive
  bodies abort), built from a Git archive with the repository `Dockerfile` plus
  the same throwaway `ENV CARGO_BUILD_JOBS=2`. Local image
  `viptv-gateway-qual:ts-only-0220483`, content ID
  `sha256:3445592a8a69edc0e18055f2c242383ab8224793a4f2cf42bb8480f48a54d68a`.
- Fixture source: `0220483` for `ts`, `bounded` and `bounded-matroska`;
  `9d6de06` for `matroska`, which changes only the Matroska refusal assertion
  in `control/tests/runtime.rs` (the gateway runtime source is identical).
- In the Matroska modes both named tests now assert the refusal on the built
  image: `422 unsupported_output`, no upstream connection left open, no ingest
  or reader child, no private replay directory, reservations at 1/4/5, and a
  clean SIGTERM exit.

| Mode | Test | Result |
| --- | --- | --- |
| `ts` (HTTP) | `real_progressive_image_share_late_join_evict_isolate_and_reap` | pass, 88.8 s |
| `bounded` | `real_progressive_kernel_bounded_media_full_fails_and_reclaims` | pass, 25.2 s |
| `matroska` | same image test, refusal branch | pass (refused), 1.1 s |
| `bounded-matroska` | same bounded test, refusal branch | pass (refused), 1.1 s |

`ts` repeats every first-run assertion (HEVC refusal, headers, single GET,
late join with no new provider request, keyframe start and `-xerror` decode of
50 video and 94 audio frames, namespace isolation, eviction about 36.8 s after
the stall with a body that ends without a clean terminator, release and
last-release cleanup). In `bounded`, a real `ENOSPC` at 2,097,152 bytes fails
the viewer with `processing_failed` and the client's body ends with a transport
error, not a clean end; processes, upstream and reservations are reclaimed and
a fresh viewer streams afterwards.

**Error termination.** A progressive body now ends cleanly only after a
successful end of output, and a live progressive body never does. Before the
fix, a body ended cleanly when the producer dropped its channel before the
HTTP body observed the close (release or job failure) or when the reader
exited successfully after its replay was torn down. Unit tests
`failed_or_released_reader_never_ends_its_body_cleanly` (50 rounds of the
close race plus a failed reader) and
`live_reader_exiting_cleanly_still_ends_the_body_with_an_error` fail on the
previous `fmp4::Stream` and pass now;
`on_demand_output_ends_cleanly_only_after_success` keeps the on-demand fMP4
clean end. `live_matroska_progressive_is_refused_before_any_media_job` proves
the refusal at the coordinator without starting a process.

## Not qualified

- Public ingress, Cloudflare or Dockflare HTTPS, and any deployment. HTTPS
  here is a fixture CA and proxy in a network-none container.
- On-demand progressive, which keeps its pre-existing original-file transport.
- Codecs, containers, bitrates and resolutions beyond the tested H264/AAC
  MPEG-TS (320x180 at 25 fps), FFV1/FLAC Matroska (128x72 at 10 fps
  in the image; 320x180 at 25 fps natively), and H264/FLAC Matroska
  (320x180 at 25 fps natively) sources. Also not covered: HLS, MP4 or other live inputs
  (they are refused by contract), interlaced input, and subtitle selection.
- The non-root, read-only production envelope for these progressive fixtures.
  The runtime fixtures run as root with `NET_ADMIN`. Only the `bounded` modes
  run the gateway as UID 10001. See
  [container qualification](CONTAINER_QUALIFICATION.md) for the separate
  envelope check.
- Client reconnect: a closed connection ends that viewer, by contract.
- Long-lived readers beyond the roughly 140-second Matroska fixture, many concurrent viewers, and hardware
  acceleration.
