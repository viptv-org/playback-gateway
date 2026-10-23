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

## Concurrent diagnostics snapshot

`KnownSwarm` now holds the client read lock across pending, half-open and active
peer snapshots. Upstream locked only the active map; the private profiler exposed
a concurrent-map panic during discovery. A race regression runs diagnostics while
peers are admitted. This changes neither discovery policy nor the public protocol.

## Stored chunk presence on return seeks

`Piece.UpdateStorageAt` retains ordinary completion refresh and optionally asks
storage for metadata-only `HasData(offset, length)` evidence. Cache-discarded
late writes and evicted partial pieces can otherwise leave received-chunk bits
set while whole-piece completion remains false. The standard completion refresh
does not clear those bits. An affected reader then waits for unrelated chunks
and a hash failure before the needed block is requested again.

The runtime reconciles an admitted head before publishing demand, and reconciles
the current missing chunk while waiting. Verified readers scan their admitted
head once; normal block waits stay constant in the number of chunks. Only dirty
chunks absent from actual storage are forgotten, and request scheduling is
notified. Complete pieces and present chunks stay intact. The optional evidence
does not authorize playback or mark reusable data verified.

A controlled two-peer wire regression delivers a valid obsolete write which the
cache drops, then gates unrelated blocks. The unpatched implementation requests
offset 49152 instead of the required 16384. The corrected implementation returns
the exact requested HTTP slice while those unrelated blocks remain gated.
