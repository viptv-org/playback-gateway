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

## Transport policy

IPTV inputs can use **HTTP or HTTPS**. Public gateway control and media endpoints
must sit behind HTTPS ingress. The gateway can fetch plain HTTP upstream while
serving HTTPS to clients; the provider hop remains unencrypted. Private/reserved
upstream addresses are denied, including redirects and child resource fetches.
The loopback exceptions used in fixtures are not compiled into production.

FFmpeg/ffprobe must use GnuTLS and enable TLS verification by default. This
protects HTTPS inputs and HTTPS children of HTTP playlists without breaking
ordinary HTTP inputs. Startup rejects unqualified binaries. The Docker image
builds signed FFmpeg 9.0.2 sources with GnuTLS, x264, zimg, VAAPI and oneVPL.

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

`/health` is liveness; `/ready` reflects startup checks and the reaper. SIGTERM
and Ctrl-C stop accepting requests, drain in-flight requests and stop workers.
Reverse-proxy access logs must redact `/media` capability paths. Never log
Authorization headers, request bodies or session response bodies.

See [API_V1.md](API_V1.md) for the independent API contract.

Run `cargo test --workspace`. Real FFmpeg/hardware tests are opt-in and are not
certified by mocked process tests. Generated media belongs in a dedicated volume.
No source credentials, databases or screenshots belong in this repository.
