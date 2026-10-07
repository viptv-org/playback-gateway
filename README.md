# Playback gateway

Independent HTTP(S) media delivery service under development. The engine supports
passthrough, remuxing and transcoding; product identity and catalogs stay outside
this repository. See PROVENANCE.md for the original tested engine source.

## Implementation status

The standalone service, scoped control/media API and Docker image are implemented
and locally tested. Do not deploy it publicly until the remaining acceptance
work in VALIDATION.md is complete. Existing VIPTV deployments still use their
old engine until explicit backend/client cutover.

The control API uses an environment bootstrap API_KEY to issue revocable scoped
keys. Integration keys authorize namespaces and quotas, not application profiles.
Viewer credentials are separately scoped to playback sessions. See the control
crate's types/tests for the executable v1 boundary.

The engine now separates inspected output preparation from worker startup. The
control worker bridge reserves the engine's normalized plan, shares compatible
jobs, authorizes media resources per viewer, and retains admission through
cancelled-process cleanup. Real HTTP live fixtures verify five compatible
viewers share one active upstream connection; distinct namespaces stay isolated.

Eligible live HLS outputs now share an independent FFmpeg ingest even when their
normalized output plans differ. The ingest copies inspected H.264/HEVC/MPEG-2 video and
AAC/MP2/MP3/AC3/EAC3 audio to a private rolling HLS replay; output readers have
only local-file protocols enabled. HTTP(S) MPEG-TS and HLS sources use the same
ingest seam, retaining upstream egress checks for every child resource. Late
outputs start from retained keyframe segments, with verified source-to-replay
track indices. Releasing or failing one output preserves the other outputs.

Verified FFV1 video or FLAC audio selects private Matroska segments with a rolling
M3U8 index instead of TS. This is a tested internal FFmpeg adapter, not public
Matroska HLS: client HLS output remains TS/fMP4. Gateway-owned eviction retains
12 listed segments, three old segments for active readers and an open new file.
See [live format qualification](docs/LIVE_SHARING_FORMATS.md).
See [progressive live delivery qualification](docs/PROGRESSIVE_QUALIFICATION.md).

Linux SubRip subtitle inputs now use the private Matroska adapter and bounded
decoded cue carryover. Long overlapping cues survive original-segment eviction,
including when captions are enabled only by a late output. Source track identity
stays absolute; selected WebVTT stream index is zero within its separate rendition.
See [shared subtitle qualification](docs/SHARED_SUBTITLES.md).

Sources containing unqualified subtitle formats or other video/audio codecs
retain their existing independent execution paths. They are not yet
qualified for cross-output single-ingest. Progressive live delivery is a
same-container lossless remux of the shared replay for a narrow supported subset
only (live MPEG-TS, and live Matroska with a Matroska replay); see [progressive live delivery](API_V1.md#progressive-live-delivery). Different fallback output plans reserve
separate physical input slots; identical plans still share their existing job.
VOD seek origins remain distinct. The
replay has 12 retained segments plus deletion grace, a 20-second first-segment deadline
and an operator replay watchdog (LIVE_REPLAY_MAX_BYTES, default 256 MiB). The
supplied Compose deployment independently bounds media allocation with a nested
256 MiB tmpfs and /tmp with 64 MiB, preserving credentials on the parent volume.
See [bounded media](docs/BOUNDED_MEDIA.md) for scope, tuning and upgrade handling.
See [bounded sustained 4K copy qualification](docs/CBR4K_QUALIFICATION.md) for
the configured CBR workload, exact byte/progress evidence and its limitations.
The sampled watchdog can overshoot; it is not the filesystem bound. See VALIDATION.md for exact evidence
and remaining format, TLS, hardware, storage and crash-containment acceptance.

## Transport policy

### Opt-in torrent and archive inputs

`TORRENT_ENABLED=true` enables BitTorrent v1 magnets and HTTP(S) `.torrent`
metainfo. It defaults to false; capabilities report `torrent: false` until
enabled. Inputs must set `live: false`. Optional `input.file_index` selects a
payload file; otherwise the largest supported media/archive entry is selected.
Safe filename/size metadata accompanies the viewer response. FFmpeg consumes a
private, token-protected loopback Range endpoint; public delivery still uses
the existing authenticated HLS/progressive viewer paths.

`TORRENT_CACHE_MAX_BYTES` defaults to 2 GiB, minimum 256 MiB, shared across
active input reservations and compressed archive spools. The service defaults
`TORRENT_PIECE_CACHE_BYTES` to 256 MiB per ordinary-video torrent input. Larger
non-archive torrents use fixed disk slots instead of requiring the full payload
to fit: active readers share the input, pieces near their positions are fetched,
and old pieces are evicted and downloaded again when seeking back. Current
reads, in-flight writes and checksum validation protect their slots. Rolling
inputs do not advertise or upload pieces, since availability can be evicted.
The cache must fit at least two complete torrent pieces. Set
`TORRENT_PIECE_CACHE_BYTES=0` to use full-payload storage for every torrent.
The engine library opts into rolling storage through `TorrentConfig`.

Torrents containing archive files, small torrents that fit the per-input budget,
and strict native exact acquisition retain full-payload reservation/storage.
Compressed archive extraction also reserves its complete verified spool.
Rolling input does not bound FFmpeg's output spool; media delivery has separate
limits. See [rolling torrent cache](docs/ROLLING_TORRENT_CACHE.md).

The cache directory is `DATA_DIR/torrent-cache`, private and exclusively locked.
This is a logical slot/payload/spool budget; filesystem block rounding and
metadata need a separate operator filesystem limit. Compose supplies a separate
cache tmpfs, configurable with `TORRENT_SPOOL_SIZE`, in addition to the media
mount. For disk-backed deployment replace that cache tmpfs with a private,
quota-limited disk mount; raising a tmpfs limit consumes memory.
Optional `TORRENT_LISTEN_PORT` opens an inbound TCP peer listener for the first
active namespace; later namespaces use outbound connections. No peer port is
published by default.

Peer/DHT connections are an explicit opt-in exception to HTTP private-address
denial. HTTP metainfo, redirects and direct stored-RAR ranges still pass the
existing egress policy. HTTP trackers are stripped before librqbit admission:
its `disable_trackers` option is ineffective in the pinned version. Private
tracker torrents and webseed metainfo are refused; librqbit has no webseed
implementation. Source-provided peers and alternate source parameters are
refused. One process manager uses per-namespace sessions to prevent librqbit's
global infohash deduplication from crossing namespace boundaries.

Stored RAR payloads, including ordered multipart volumes, expose member ranges
without a second payload copy. Compressed torrent archives are fully extracted
and checksum-verified into a bounded spool before playback; partial-progress
seeking is unavailable. Direct HTTP RAR supports stored members only. Encrypted
archives are refused. Native clients can use the torrent-only
[UniFFI/Kotlin facade](ffi/README.md); its selected-package dependency graph
excludes archive extraction/unrar-rs. The engine's separate `archive` feature
remains enabled for the default gateway. Native archive unpacking is outside
that facade. [Torrent qualification](docs/TORRENT_QUALIFICATION.md) records
exact test scope and remaining acceptance.

In-process consumers can explicitly request independent exact-file leases with
`TorrentManager::start_selected_magnet` / `start_selected_metainfo`; each requires
a metadata file index and owns separate stop state and loopback byte authority.
Only that file's metadata and bytes are exposed by the returned handle. Repeated
selected starts share payload/reservation, not tokens. Existing `start_magnet` /
`start_metainfo` preserve shared-handle and all-file gateway/archive behavior;
explicit clones always remain aliases. These opt-in APIs do not provide
cancellable acquisition, bounded joined shutdown or public-only peer/DHT policy,
and do not qualify a native authorization integration. See the
[facade boundaries](ffi/README.md#opt-in-independent-selected-file-leases).

The separate strict native path uses `new_native_public` and
`begin_native_selected_magnet` / `begin_native_selected_metainfo` with an exact
hash/index/verified size and the caller's remaining monotonic budget. It owns
cancellation before IO, shares pending exact-file payloads with independent byte
grants, and exposes joined retirement or sticky failed settlement. Strict foreign
handles use status polling and refuse callbacks. See
[owned selected acquisitions](ffi/README.md#owned-strict-selected-acquisitions)
for the ABI and remaining qualification limits; this does not activate a client
or establish a bound on arbitrary stalled OS IO.

Native integrations can construct `TorrentManager::new_native_public` and use
`start_native_selected_magnet` / `start_native_selected_metainfo` with an exact
canonical hash, file index and verified selected size. These sessions enforce
public destination policy and bounded metadata at the dependency boundaries,
suppress tracker/PEX workers and use a versioned bootstrap inventory. They
require the generic expected selection before payload file access. The separate
`test-network-policy` build feature permits only exact owned private TCP seeders
with DHT disabled; production native artifacts exclude it. Backend metadata
consumers can adopt the lightweight `torrent-policy` crate without the engine.
See [native network policy](docs/NATIVE_NETWORK_POLICY.md). Acquisition lifetime,
FFI adoption and physical/public-peer qualification remain independent gates.

The dependency additions are pinned in Cargo.lock. See
[provenance](PROVENANCE.md) for licensing restrictions. Never enable librqbit
tracing targets in production: they emit peer addresses and torrent identifiers.
The service installs no tracing subscriber; embedders must suppress the target
prefix `librqbit=off` before installing one.

IPTV inputs can use **HTTP or HTTPS**. Public gateway control and media endpoints
must sit behind HTTPS ingress. The gateway can fetch plain HTTP upstream while
serving HTTPS to clients; the provider hop remains unencrypted. Private/reserved
upstream addresses are denied, including redirects and child resource fetches.
The loopback exceptions used in fixtures are not compiled into production.

FFmpeg/ffprobe must use GnuTLS and enable TLS verification by default. This
protects HTTPS inputs and HTTPS children of HTTP playlists without breaking
ordinary HTTP inputs. Startup rejects unqualified binaries. The Docker image
builds signed FFmpeg 9.0.2 sources with GnuTLS, x264, zimg, VAAPI and oneVPL.

Network inputs use FFmpeg's finite HTTP range requests: 1 MiB during initial
probing and 8 MiB during subsequent reading. This supports file origins that
reject open-ended ranges, while retaining FFmpeg's seek and continuation logic.
Engine warnings record bounded failure categories and recognized HTTP status
codes; dependency transport logs and arbitrary environment filters are disabled
to keep source credentials out of service logs.

## Local service

Set a unique `API_KEY` of at least 32 characters in an ignored `.env`, then run
`docker compose up --build`. The compose file binds only `127.0.0.1:8090`; adding
public HTTPS ingress is an operator action, not automatic deployment. The image
runs as UID/GID 10001 with a read-only root and dedicated writable data volume.
Do not share that data directory between instances. An OS-held lock prevents a
second instance from cleaning or changing another instance's media.

For a native development run, use `cargo run --locked -p playback-gateway` with
`API_KEY` set. Defaults: `DATA_DIR=data`, `BIND_ADDRESS=127.0.0.1:8090`,
`MAX_OUTPUTS=8`, `FFMPEG_PATH=/usr/bin/ffmpeg`, `FFPROBE_PATH=/usr/bin/ffprobe`.
An existing data directory must be private to its owner. Optional `QSV_DEVICE`
and `VAAPI_DEVICE` select render nodes; a container operator must also map the
specific device and grant the container user the appropriate device-group access.

Pre-Xe Intel Quick Sync requires a compatible hardware runtime in addition to
oneVPL. The optional [Intel legacy profile](docs/INTEL_QUICK_SYNC.md) packages and
checks that runtime without changing the default generic image.

`/health` is liveness; `/ready` reflects startup checks and the reaper. SIGTERM
and Ctrl-C stop accepting requests, drain in-flight requests and stop workers.
HTTP draining has a five-second grace bound so an unread response cannot defer
worker shutdown indefinitely. Linux media children also terminate with their
creating parent thread after abrupt owner death; other host platforms are not
crash-qualified. See [container qualification](docs/CONTAINER_QUALIFICATION.md).
Reverse-proxy access logs must redact `/media` capability paths. Never log
Authorization headers, request bodies or session response bodies.

See [API_V1.md](API_V1.md) for the independent API contract.

Run `cargo test --workspace`. Real FFmpeg/hardware tests are opt-in and are not
certified by mocked process tests. Generated media belongs in a dedicated volume.
No source credentials, databases or screenshots belong in this repository.
