# Progressive live delivery qualification

Built-image evidence for [progressive live delivery](../API_V1.md#progressive-live-delivery)
(playback-gateway#1). This records named disposable fixtures. It is not a
deployment or a universal playback approval.

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

## Results (2026-09-30)

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

### Defect found: Matroska progressive stalls on the built image

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

A correct fix needs a concatenable Matroska replay: for example a header or
init segment plus cluster-only segments, or a reader that opens each segment as
its own demuxer. That changes the private Matroska replay adapter, which the
shared HLS Matroska outputs also use. It is not a minimal change, so it is
left as follow-up work. Until it is fixed, a live Matroska progressive request
is accepted but does not deliver continuous media.

## Not qualified

- Matroska (FFV1/FLAC) progressive delivery: it fails on the built image (see
  above). There is no Matroska evidence for HTTPS, eviction, isolation or
  reaping.
- Public ingress, Cloudflare or Dockflare HTTPS, and any deployment. HTTPS
  here is a fixture CA and proxy in a network-none container.
- On-demand progressive, which keeps its pre-existing original-file transport.
- Codecs, containers, bitrates and resolutions beyond the tested 320x180 25 fps
  H264/AAC MPEG-TS source. Also not covered: HLS, MP4 or other live inputs
  (they are refused by contract), interlaced input, and subtitle selection.
- The non-root, read-only production envelope for these progressive fixtures.
  The runtime fixtures run as root with `NET_ADMIN`. Only the `bounded` modes
  run the gateway as UID 10001. See
  [container qualification](CONTAINER_QUALIFICATION.md) for the separate
  envelope check.
- Client reconnect: a closed connection ends that viewer, by contract.
- Long-lived readers beyond about 90 s, many concurrent viewers, and hardware
  acceleration.
