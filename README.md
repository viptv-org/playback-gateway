# Playback gateway

Independent HTTP(S) media delivery service under development. The engine supports
passthrough, remuxing and transcoding; product identity and catalogs stay outside
this repository. See PROVENANCE.md for the original tested engine source.

## Implementation status

The engine and control contract are being extracted before backend cutover.
Do not deploy this checkout as a public service until scoped authentication,
network isolation, media routes and lifecycle acceptance are complete. Existing
VIPTV deployments still use their old engine until explicit cutover.

The control API uses an environment bootstrap API_KEY to issue revocable scoped
keys. Integration keys authorize namespaces and quotas, not application profiles.
Viewer credentials are separately scoped to playback sessions. See the control
crate's types/tests for the executable v1 boundary.

Run `cargo test --workspace`. Real FFmpeg/hardware tests are opt-in and are not
certified by mocked process tests. Generated media belongs in a dedicated volume.
No source credentials, databases or screenshots belong in this repository.
