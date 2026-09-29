# Foundation validation

The extracted engine has 64 passing unit/process-fixture tests and 16 existing
opt-in media/hardware tests. Its 1080p clamp is removed; the dimension test now
checks 3840x2160 while retaining even dimensions and no-upscale filters.

Control tests cover source redaction and structural validation; hashed persistent
integration keys; namespace/scope/expiry/revocation; idempotent viewer allocation;
five logical viewers sharing one logical job; cleanup retaining key capacity;
startup deadline/late completion; private/reserved IP denial; and proxy forwarding
to an explicit test-only loopback origin without proxy credentials leaking.

These are not yet end-to-end media-service tests. The session ledger is not wired
to engine workers or HTTP media capabilities. Input/output sharing, Docker/runtime
startup, FFmpeg network enforcement and backend integration remain incomplete.
The egress proxy alone does not prove engine-wide SSRF protection until all engine
fetches are verified to traverse it. Do not deploy this checkpoint publicly.
