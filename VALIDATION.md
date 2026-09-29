# Foundation validation

The extracted engine has 65 passing unit/process-fixture tests and 17
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

The real `real_egress_proxy_covers_root_redirects_nested_segments_and_keys` test
also passes for both FFmpeg and ffprobe. It overrides child-process proxy
environment variables, then verifies root URLs, redirects, HLS segments and AES
keys reach an explicit denying proxy without contacting a protected local
listener. Raw TCP/TLS segment URLs fail without bypassing that proxy. This does
not yet certify all network paths or HTTPS certificate/hostname validation.
An unconditional `-tls_verify 1` was tested and rejected because FFmpeg fails
ordinary HTTP inputs with that unused option; TLS acceptance remains required.

The gateway engine no longer contains native direct-URL/credential handoff.
That product decision belongs in the integrating backend; gateway passthrough
still serves media through a managed resource. Original-file delivery follows
declared dimensions without hard-coded 1080p/4K clamps, with tests for 4K/8K and
rejection above the client-declared envelope. Codec/decoder checks remain.

These are not yet end-to-end media-service tests. The session ledger is not wired
to engine workers or HTTP media delivery. Input/output sharing, Docker/runtime
startup, FFmpeg network enforcement and backend integration remain incomplete.
The egress proxy alone does not prove engine-wide SSRF protection until all engine
fetches are verified to traverse it. Do not deploy this checkpoint publicly.
