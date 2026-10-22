# VIPTV request-pipeline patch

Upstream: `github.com/anacrolix/torrent` at
`v1.61.1-0.20260927071845-d913b30f520e`. The upstream MPL 2.0 license is retained in `LICENSE`.

`PeerConn.nominalMaxRequests` in `peerconn.go` and the ownership comparison in
`PeerConn.applyRequestState` in `requesting.go` differ from upstream.
The first data request keeps upstream slow-start behavior. Once a peer supplies useful payload,
the minimum pipeline is 50, 32, 16, 8, or 5 requests for at most 2, 5, 10, 20, or more connected peers.
The peer-advertised request limit, local request limit, and storage admission limits still apply.
This avoids starving a responsive peer while avoiding a large initial queue on unproven peers.

After both peers have supplied at least four useful chunks, request reassignment
compares queue lengths divided by measured useful-data rates. The new owner must
have 25% estimated completion headroom. With insufficient rate samples, upstream
queue-depth balancing remains in force. Existing cancellation guards and
`StealRequestGrace` remain authoritative. This prevents a proven slow peer from
retaining urgent blocks solely because a fast peer has more outstanding requests.

The opt-in `TestPipelineQualification` fixture measures exact payload delivery under controlled
latency and bandwidth. `scripts/benchmark-pipeline.py` compares two prebuilt test binaries.
Rendered-frame comparisons remain a separate acceptance gate.
