# Playback gateway

This repository is independent of VIPTV. It owns generic media execution,
scoped integration credentials, shared input/output jobs and viewer leases.
Never introduce accounts, profiles, catalog matching or client-brand policy.

Read README.md and PROVENANCE.md. Run cargo test --workspace before committing.
Preserve inherited media tests and GPL-2.0-only notices. Never log raw source
URLs, headers, administrative/integration/media credentials or response bodies.
No production deployment or credential import is authorized by local testing.
