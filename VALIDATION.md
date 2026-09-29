# Foundation validation

The extracted engine has 64 passing unit/process-fixture tests and 16 existing
opt-in media/hardware tests. Its 1080p clamp is removed; the dimension test now
checks 3840x2160 while retaining even dimensions and no-upscale filters.

Control tests cover source redaction and structural validation; hashed persistent
integration keys; namespace/scope/expiry/revocation; idempotent viewer allocation;
five logical viewers sharing one logical job; cleanup retaining key capacity;
startup deadline/late completion; private/reserved IP denial; and proxy forwarding
to an explicit test-only loopback origin without proxy credentials leaking.

The control crate now has 15 passing tests. Additional coverage verifies
per-viewer hashed media capabilities, rotation, exact lease expiry, revocation
before background cleanup, credential-type separation and safe shared-job failure
codes while retaining admission until teardown. Bootstrap-only HTTP key routes
are tested for issuance, redacted listing, idempotent revocation, authentication
before body parsing, bounded bodies and non-cacheable responses. SQLite work is
offloaded from the asynchronous HTTP executor.

One opt-in real-media test was additionally run with `/usr/bin/ffmpeg` and
`/usr/bin/ffprobe`: `real_ffmpeg_remux_transcode_and_cleanup` passed. This checks
the extracted engine's real remux/transcode and cleanup path, not gateway HTTP
integration or hardware acceleration.

These are not yet end-to-end media-service tests. The session ledger is not wired
to engine workers or HTTP media delivery. Input/output sharing, Docker/runtime
startup, FFmpeg network enforcement and backend integration remain incomplete.
The egress proxy alone does not prove engine-wide SSRF protection until all engine
fetches are verified to traverse it. Do not deploy this checkpoint publicly.
