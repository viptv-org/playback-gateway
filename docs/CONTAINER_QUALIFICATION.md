# Local container and runtime qualification

## Exact committed-source evidence — 2026-09-30

Runtime source `f69adb00c09828121d5479243d337472be7aa87f` was built from a
Git archive as local image `playback-gateway:qualification-f69adb0`, content ID
`sha256:3f2a6affeba84ad8e2ec56fb0ecc18c4c53cf93bd73048726d6aa9e1f17f7ada`.
Both committed-source scripts passed against that exact image: all three named
runtime tests executed exactly once, and the separate non-root envelope passed.
The runtime fixture image content ID is recorded by its digest-derived local tag
and Docker build output. Runtime execution used the resolved local image; no
registry push or production deployment occurred.

The exact image's FFmpeg/ffprobe plus GnuTLS, libc and x264 content hashes match
the original image runtime used by the named extracted-binary HTTP/HLS fixtures.
The direct service tests additionally use the exact image's in-container binary
and media executables. Stopped envelope data was archived privately at
`/tmp/pgw-qualified-volume.FVp4r8/fixture-data.tar` before its named volume was
removed. That is fixture data, not production backup material. Runtime containers
and anonymous test volumes were removed; images, logs and private stopped-data
archives remain for reproduction/recovery.

This is acceptance evidence for named disposable fixtures, not deployment or
universal playback approval. No production data, developer env, device or real
provider is used. Build dependencies may be downloaded; runtime networking is
`none`, with no published ports.

Build the reviewed source locally, then run:

```sh
sudo docker build -t playback-gateway:qualification .
sudo bash scripts/check-container-envelope.sh playback-gateway:qualification
sudo bash scripts/check-runtime-container.sh playback-gateway:qualification
```

The envelope check runs the actual production image as UID/GID 10001 with a
read-only root, all capabilities dropped, no-new-privileges and private writable
data. It verifies `/ready`, Docker health, denied root writes, directory mode,
duplicate-volume lock rejection, clean SIGTERM and restart. Only its newly
created container and named volume are removed. A stopped-data tar archive is
retained privately under `/tmp/pgw-qualified-volume.*` for recovery. On failure
the fixture volume is retained, with its name reported. No active database is
copied.

The runtime harness builds test executables from a Git archive of committed
HEAD, using Rust 1.98/trixie to match the image. It resolves the supplied local
image to its content ID, and requires three named tests to run exactly once.
Ignored env/data and uncommitted source are not sent to this build. Each test
uses a disposable container with network none, root plus NET_ADMIN solely to
configure a synthetic public-classified address on isolated loopback. The test
independently requires `/.dockerenv` and only the `lo` interface. This preserves
production SSRF validation and cannot contact a real provider. This root fixture
does not qualify the non-root envelope; the separate envelope check does.

The runtime tests exercise:

- actual service readiness, unauthenticated denial and second-owner rejection;
- a real H264 HTTP source feeding one ingest and two differing outputs;
- normal authenticated HLS/segment reads before a deliberately large synthetic
  generated-file response forces TCP backpressure;
- SIGTERM while that response remains unread, actual media child reaping and
  upstream closure, followed by restart using the same storage;
- persistent integration-key authentication, denial of old in-memory viewer
  IDs after restart, and successful fresh playback;
- SIGKILL of the service, media child exit/reaping, recognizable crash leftovers,
  startup cleanup and successful fresh playback after another restart.

The large generated file is HTTP/backpressure evidence, not decoder evidence.
Every runtime fixture has a 120-second outer bound and checks an exact test name
and `1 passed`; a successful zero-test selection is rejected. Cleanup targets
only the newly returned container ID and its disposable anonymous volumes.
Images and logs remain for reproduction. Fixture tmpfs limits do not establish
a hard bound for the production data volume.

## Observed regression and runtime changes — 2026-09-30

The reviewed `a47c25c` image
`sha256:1fbc1c6ffac58cf09fb185fcbdc9c96853f95afc6256a0c7477116421ff9d6a7`
failed the stalled-response test: SIGTERM did not complete within eight seconds
because Axum graceful draining preceded worker shutdown without a deadline.
The corrected runtime caps graceful HTTP draining at five seconds, then stops
and reaps owned workers before the runtime exits and storage ownership ends.
The same real service fixture passes with the correction, including restart.

Linux media subprocesses additionally arm parent-death SIGKILL before exec,
checking for reparenting before registration completes. Cancellation still uses
the existing kill/wait guards. A real kernel unit test proves a configured child
dies when its creating thread exits. The a47 crash fixture already eventually
passed its five-second check via proxy EOF; this change is additional ownership
hardening, not evidence that a47 permanently orphaned its tested children.
Linux parent-death semantics refer to the creating thread, and credential-changing
or privileged helper executables can clear the setting. The qualification uses
the image's direct FFmpeg/ffprobe executables, not arbitrary helper chains.
[Linux's parent-death documentation](https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html)
describes those limits.

Named HTTP and H264 HLS sharing fixtures also pass using the original image's
exact FFmpeg 9.0.2 binaries and Debian runtime libraries extracted for execution
through their ELF loader. That is image-media-binary qualification on the host,
not another container envelope test; using exec preserves process identity.
HEVC fixture generation needs libx265, which the production image does not ship.
No image HEVC generation or current image TLS-sharing qualification is inferred.

Remaining gates include universal ingest formats, a hard filesystem/writer
storage bound, public HTTPS ingress, slow progressive/direct and encrypted-media
responses, shutdown during stalled inspection/database/filesystem operations,
non-Linux host crash semantics, hardware/4K stress, long-lived readers and native
renderers. The five-second HTTP drain bound is not proof of a universal shutdown
deadline for every possible kernel or storage stall.
