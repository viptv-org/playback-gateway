# Shared runtime and upstream ownership

The replacement uses the unmodified anacrolix/torrent dependency pinned in
`torrent-runtime/go.mod`. No local `replace`, vendored engine patch or Stremio
fork is used. Upgrade the pin and run qualification; maintain integration code
only where it supplies behavior the engine does not own.

| Behavior | Owner and integration |
| --- | --- |
| Magnet/metainfo decoding and identity | Upstream `TorrentSpec` and metainfo APIs. Runtime bounds input, selects supported formats and enforces the private descriptor contract. |
| DHT, trackers, PEX and peer protocol | Upstream torrent/DHT/tracker packages. Runtime supplies public-destination policy through supported hooks. |
| DNS | Upstream UDP tracker/DHT code and Go's standard networking. The HTTP dial hook uses `net.Dialer`; it checks the actual resolved socket destination before connection. It implements no DNS server, bootstrap traversal or resolver cache. |
| TCP/uTP, handshakes, request scheduling and hashing | Upstream engine. Runtime sets bounded dial budgets and ordinary-handshake preference with encrypted fallback. Same-socket TCP head-start experiments remain qualification-only configuration. |
| File selection | Runtime private interface resolves explicit index or largest file and reports selection, including the archive-member index. |
| Bounded persistent disk cache | Runtime implements upstream's pluggable storage interface, charges allocation, protects active IO and updates upstream completion before eviction. Cached bytes supply no playback authority. |
| RAR parsing and multipart seeking | Upstream rardecode. Runtime supplies a torrent-backed filesystem and rejects compressed/encrypted formats instead of extracting them. |
| Authority, cancellation and worker ownership | Runtime handles short-lived grants, endpoint revocation and joined retirement. Parent owns the worker and enforces termination responses. These are application integration concerns. |
| HTTP media ranges | Go's `http.ServeContent`; runtime supplies token checking and bounded received-block reads for ordinary selected files. Archives remain verified-only. |

The custom demand reader is the main integration exception. The pinned upstream
capped reader recursively retries cancellation errors; the owned cancellation
fixture cannot qualify that behavior. The wrapper uses upstream piece priorities
and completion operations. Ordinary selected-file playback may read contiguous
received blocks before whole-piece verification, following the owner's explicit
2026-10-22 approval. Unwritten holes are never readable; a rejected piece loses
early-read eligibility until a verified replacement completes. Archive parsing,
archive playback, storage readers, and persisted/restarted cache reuse remain
verified-only. The unchanged `public_discovery_verified_v2` compatibility tag
continues to require verification for reusable cached data; it no longer means
every live plain-file read waits for a complete verified piece. The reader implements no
peer protocol or hash algorithm. Keep this workaround isolated and replace it
with the upstream reader when capped cancellation can pass the same tests.
See [qualification](../torrent-runtime/QUALIFICATION.md) and
[upstream reader](https://github.com/anacrolix/torrent/blob/d913b30f520e10e85a94c7dc4b7b3e828b22c4b2/reader.go).

The 2026-10-09 DNS audit removed the manual HTTP lookup/address loop. The standard
dialer now owns resolution, IPv4/IPv6 racing, cancellation and one overall dial
budget; the policy hook checks each resolved candidate before connect.
[Go dialer](https://pkg.go.dev/net#Dialer),
[tracker hooks](https://github.com/anacrolix/torrent/blob/d913b30f520e10e85a94c7dc4b7b3e828b22c4b2/config.go).
The owned hostname test proves a private destination is refused before the
listener receives a connection, while the isolated fixture can resolve and
connect its own hostname.

The DHT fixture's independent peer oracle is test-only. Source inspection confirms
the production client leaves the optional `PeerStore` unset; it does not use
the broken in-memory peer store found while building that fixture. There is no
reason to implement a second DHT peer store to work around an unused API.
