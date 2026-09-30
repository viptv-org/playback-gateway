# Supported Docker media-spool allocation bound

The supplied Compose deployment mounts a dedicated tmpfs at the existing
`/var/lib/playback-gateway/media` path, inside the persistent named data volume.
Its default data-page allocation limit is 256 MiB. `/tmp` has a separate 64 MiB
tmpfs. Both are nodev/nosuid; the media mount is also noexec and owned by
UID/GID 10001 with mode 0700. The service remains non-root with a read-only root,
dropped capabilities and no-new-privileges. Credentials, SQLite journal files and
`instance.lock` remain siblings on the persistent named volume.

This guarantee requires the supplied nested mount and default DATA_DIR. A plain
`docker run` without that mount, native runs, changed DATA_DIR or custom unbounded
volumes do not inherit it. The image alone cannot impose a host mount. Docker
Compose interpolation `MEDIA_SPOOL_SIZE` changes the mount limit; it is an operator
deployment setting, not a public runtime API or client quality preference.
[Docker documents tmpfs size, ownership and lifecycle](https://docs.docker.com/engine/storage/tmpfs/).

## Write-path audit

| Producer/resource | Write path or storage | Covered |
| --- | --- | --- |
| Live ingest manifests, MPEG-TS replay, partial segments | Config.root/UUID, including .tmp | Media tmpfs |
| Remux/transcode HLS, TS/fMP4 segments, initialization files | Config.root/UUID | Media tmpfs |
| Managed WebVTT sidecars/subtitle playlists | Output Config.root/UUID | Media tmpfs |
| Pipeline fallback, startup failure and crash leftovers | Same generated UUID children | Media tmpfs |
| On-demand subtitle extraction | Bounded stdout/read pipes | No disk spool |
| Progressive original-media range/HLS caches | Bounded memory buffers | No disk spool |
| Continuous fMP4 and progress | Bounded pipes/buffers | No disk spool |
| FFmpeg/ffprobe binaries and config | Read-only image | Cannot create another disk spool there |
| Other temporary files | Separately bounded /tmp | Separate allocation bound |
| Credential DB/journals and instance lock | Persistent DATA_DIR parent | Intentionally outside media bound |

All generated media paths are under Config.root; FFmpeg receives absolute output
paths. No disk cache protocol or arbitrary output path is accepted in v1. Cleanup
removes generated UUID children, not the mounted root. The filesystem kernel
enforces its allocation limit across concurrent producers, partial files, retries
and orphaned/unlinked-but-open files. It does not rely on a post-write sampling
watchdog.

The limit is allocated filesystem data pages, not logical sparse-file lengths,
process RSS, all tmpfs metadata/slab memory, swap usage or persistent credential
volume usage. Docker notes that tmpfs can be swapped; this is not a guarantee
that media bytes can never reach host swap. No filesystem or process-memory limit
for arbitrary native/custom deployments is claimed.

## Separate operator replay watchdog

The former fixed 64 MiB replay ceiling is replaced by operator
`LIVE_REPLAY_MAX_BYTES`, default 268435456 bytes. The minimum accepted setting is
1 MiB. The library constructor also accepts this resource budget. It is logical
replay-file accounting, sampled and independent of the hard allocation limit;
it does not cover separate output files. No client quality field was introduced.

A nominal retained 30-second window at the default per-ingest logical budget is
about 71.6 Mbit/s before overhead; actual keyframe durations and deletion grace
can retain more/less time. The global mount is shared by all ingests and outputs,
so usable capacity per job is lower under concurrency. Operators admitting
high-bitrate/long-GOP streams must budget the mount and replay watchdog together.
The defaults are resource limits, not a decoder-resolution clamp. Sustained
high-bitrate 4K/long-GOP qualification remains open; the earlier small four-frame
4K copy test does not certify this workload.

## Filesystem-full qualification

Run after building the committed source image:

```sh
sudo bash scripts/check-bounded-media-container.sh LOCAL_GATEWAY_IMAGE
sudo bash scripts/check-container-envelope.sh LOCAL_GATEWAY_IMAGE
```

The first fixture uses a dedicated 2 MiB mount and real service/media binaries.
Its controller runs in network none and verifies only loopback before assigning
its synthetic public-classified fixture address. Test-only controller privileges
permit that setup and cross-UID inspection/signalling. The actual service drops
to UID/GID10001 with effective capabilities zero and a read-only root.

The fixture measures kernel allocation, distinguishes a 1 GiB sparse logical
length from allocated blocks, then fills the mount until writes fail with ENOSPC
while a real input/output is active. It checks the exact allocation limit,
readiness503, safe not_ready denial with no fake playback response, safe active
viewer processing_failed, actual child/upstream teardown and quota reclamation.
It then removes only its own fill file, plays successfully, restarts on the same
persistent credential/lock storage and uses the existing key for fresh playback.
The separate envelope fixture proves credential-key authentication survives a
Docker stop/start while a media-only marker disappears with the temporary mount.

Capacity/readiness free-block checks are advisory and can race allocation. They
do not replace actual producer failure and teardown accounting. No source URL,
raw provider/subprocess diagnostics or credentials are returned for full storage.
The named test must run exactly once; its disposable container/data are removed.
Stopped envelope data is privately archived before named-volume removal.

## Upgrade and rollback without silent deletion

An existing parent volume may already contain `media/UUID` directories. The new
child mount hides those files; it neither deletes them nor reduces their existing
persistent-volume usage. They are outside the new mount's allocation accounting.
Do not assume a smaller `df` inside the mount removed legacy bytes underneath.

Before changing a production mount, an operator must stop the old owner, take a
consistent credential/data backup, and inspect/archive the parent volume through
a read-only helper **without** the child tmpfs. Override the helper entrypoint so
it cannot start the service or run normal orphan cleanup. Preserve that archive
and deployment definition for rollback. Removing old generated files underneath
is a separate, explicitly reviewed operation; this change does not authorize or
perform it. Removing the child mount on rollback exposes the old files again,
and normal startup orphan cleanup may remove recognized generated UUID children,
so inspection/backup must precede that restart. No production data was touched.
