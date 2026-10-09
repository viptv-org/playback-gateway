# Torrent runtime research

Research date: 2026-10-09. This note records source inspection, not measured
playback performance or release qualification. The gateway remains a generic
media service; application source-selection policy and native client routing
belong to its consumers.

## Decision and scope

Use **anacrolix/torrent + rardecode** for the first replacement spike. One generic
runtime should serve native Android/API24, Android TV, Linux and Windows locally,
with no gateway retry/fallback, and serve web/Roku ingestion on the gateway.
Require a bounded, reusable cache of verified data, retained after stop, with no
background completion download. Initial archives cover **stored, unencrypted RAR
members, including multipart volumes**. Compressed RAR and passwords are explicitly
unsupported initially; that matches Stremio's documented RAR streaming limit.
Integration rollout depends on the benchmark and correctness gates below.

The engine choice follows API fit for strict storage bounds, not a claim that Go
is faster than libtorrent, rqbit or Stremio. [anacrolix's storage interface](https://github.com/anacrolix/torrent/blob/d913b30f520e10e85a94c7dc4b7b3e828b22c4b2/storage/interface.go)
provides piece completion markers and a shared capacity hook;
[Piece.UpdateCompletion](https://github.com/anacrolix/torrent/blob/d913b30f520e10e85a94c7dc4b7b3e828b22c4b2/piece.go)
refreshes engine state after external data changes. Its
[reader implementation](https://github.com/anacrolix/torrent/blob/d913b30f520e10e85a94c7dc4b7b3e828b22c4b2/reader.go)
handles capped storage whose data can disappear. These are usable upstream seams
for eviction, rather than a guarantee that a custom cache is already correct.
Do not enable `SetResponsive`: it permits reading before piece verification.

## Inspected versions

| Component | Source inspected |
| --- | --- |
| anacrolix/torrent | HEAD `d913b30f520e10e85a94c7dc4b7b3e828b22c4b2`; latest stable tag observed `v1.61.0`, commit `486bcbe758e0cff0f750fcee9c26268a11250d77` |
| rardecode | `v2.4.1`, commit `4e5b2a5cc5359c6a0547de170f8518d1103cb296`, published 2026-08-23 |
| libtorrent-rasterbar | Public `RC_2_1` APIs; latest release observed `v2.1.2`, 2026-09-25 |
| Nuvio engine | `v0.1.4`, HEAD `ddb383ba46bafb9970baf0bc23f51de786f58a4c`, released 2026-10-04; pins libtorrent `2.0.12`, commit `740a0b9aeabe00e762cc0efe4a0f27593db2550b` |
| Existing gateway | Locally patched librqbit `8.1.1`; `unrar-rs =0.10.8` |

These are research snapshots, not permission to follow moving branches in a
build. Select and lock implementation dependencies explicitly.
[anacrolix tags](https://github.com/anacrolix/torrent/tags),
[rardecode version](https://pkg.go.dev/github.com/nwaples/rardecode/v2@v2.4.1),
[libtorrent releases](https://github.com/arvidn/libtorrent/releases),
[Nuvio release](https://github.com/NuvioMedia/nuvio-engine/releases/tag/v0.1.4),
[gateway provenance](../PROVENANCE.md).

## Stremio behavior to preserve at the consumer boundary

Stremio's [current Stream Object contract](https://github.com/Stremio/stremio-addon-sdk/blob/master/docs/api/responses/stream.md)
specifies a torrent `fileIdx`; when absent it selects the largest file in the
torrent. Archive sources can select by `fileIdx` or `fileMustInclude`. Keep that
selection in the application's resolver; the generic runtime receives an exact
selected member and must not silently substitute another file.

The same official contract documents these archive limits:

| Source | Stremio support |
| --- | --- |
| RAR | Multipart and seeking; no decompression |
| ZIP | Multipart and decompression; no video seeking |
| 7z | Multipart and LZMA decompression; seeking only without compression |
| TAR | Seeking; no multipart or compression |
| TGZ | Multipart and decompression; no video seeking |

The [November 2025 announcement](https://blog.stremio.com/stremio-new-stream-sources-usenet-rar-zip-ftp-and-more/)
calls those sources experimental. Public MIT
[rar-http](https://github.com/Stremio/rar-http) and
[rar-stream](https://github.com/Stremio/rar-stream) implement stored-member range
mapping across volumes rather than full extraction. Public
[enginefs](https://github.com/Stremio/enginefs) manages torrent-stream engines, but
the open [stremio-service launcher](https://github.com/Stremio/stremio-service/blob/master/build.rs)
downloads the shipped `server.js`; its public repository is not the complete
streaming-server implementation. Do not claim that stock Stremio is simply
unmodified public torrent-stream.

## Why libtorrent and Nuvio are references, not the chosen cache foundation

Libtorrent has a mature protocol implementation and explicit
[streaming deadlines](https://www.libtorrent.org/streaming.html), plus a supported
[native custom disk interface](https://www.libtorrent.org/reference-Custom_Storage.html).
However, the inspected [public torrent handle API](https://github.com/arvidn/libtorrent/blob/RC_2_1/include/libtorrent/torrent_handle.hpp)
has no documented per-piece eviction/invalidation operation. `force_recheck`
disconnects peers and rechecks the torrent; internal `async_clear_piece` is a disk
synchronization callback, not an external eviction notification. A correct rolling
backend therefore needs additional proof. File priorities select eligible data;
piece deadlines prioritize urgent data. Deadlines alone do not bound retained
storage, and changing file priorities resets piece priorities.

FrostWire jlibtorrent supports API24 and four Android ABIs, but its
[JNI session constructor exposure](https://github.com/frostwire/frostwire-jlibtorrent/blob/master/swig/includes/libtorrent_session_params.i)
only selects default/POSIX disk implementations. A custom disk backend needs
native binding work. [libtorrent4j v2.1.0-39](https://github.com/aldenml/libtorrent4j/releases/tag/v2.1.0-39)
raises its Android minimum to API28.

Nuvio is a useful C++20/C-ABI/JNI reference with Android API24/four-ABI builds and
Linux/Windows x86_64 artifacts, but its
[README](https://github.com/NuvioMedia/nuvio-engine/blob/v0.1.4/README.md) explicitly
states that it is not production-ready. Its
[disk cache](https://github.com/NuvioMedia/nuvio-engine/blob/v0.1.4/src/cache/disk_cache_manager.cpp)
evicts whole inactive torrent directories and protects live torrent data, allowing
the budget to be exceeded. Its bounded RAM piece cache leaves backing files on
disk. The [cache integration test](https://github.com/NuvioMedia/nuvio-engine/blob/v0.1.4/tests/disk_cache_policy_integration_test.cpp)
expressly treats active over-budget pressure as nonfatal.

More critically, Nuvio's
[streaming bridge](https://github.com/NuvioMedia/nuvio-engine/blob/v0.1.4/src/torrent/libtorrent_stream_bridge.cpp)
serves finished but **unverified** blocks from payload files before the whole piece
hash passes. A later hash failure cannot retract already delivered bytes. Its
[dependency build](https://github.com/NuvioMedia/nuvio-engine/blob/v0.1.4/cmake/Dependencies.cmake)
applies eight libtorrent patches, including a new piece-first-block API. Its
[backend](https://github.com/NuvioMedia/nuvio-engine/blob/v0.1.4/src/torrent/libtorrent_backend.cpp)
also disables outgoing uTP. It supplies no archive decoder/API.

Nuvio's [selection policy](https://github.com/NuvioMedia/nuvio-engine/blob/v0.1.4/src/torrent/file_selection.cpp)
prefers exact path/basename hints over explicit indices, then largest playable
file, then largest file; this differs from Stremio's documented torrent contract.
Also distinguish released [NuvioTV 1.0.0](https://github.com/NuvioMedia/NuvioTV/blob/1.0.0/app/src/main/java/com/nuvio/tv/core/torrent/TorrentService.kt),
which uses a local TorrServer binary, from current TV development and
[NuvioMobile 0.5.8-beta](https://github.com/NuvioMedia/NuvioMobile/blob/0.5.8-beta/composeApp/src/androidMain/kotlin/com/nuvio/app/features/p2p/P2pStreamingEngine.android.kt),
which integrate nuvio-engine 0.1.4. Playback reports about the former do not
validate the latter.

## Archives and license declarations

[rardecode v2.4.1](https://github.com/nwaples/rardecode/tree/v2.4.1) is BSD-2-Clause
and exposes `FileSystem(fs.FS)` for torrent-backed multipart volume readers.
Stored, unencrypted, nonsolid entries can expose seeking over seekable volumes;
the archive checksum wrapper affects that capability, so preserve torrent hash
verification and distinguish it from full archive-member CRC validation.
Compressed/encrypted readers are sequential, and solid members depend on earlier
dictionary state. Those broader capabilities do not make instant arbitrary
seeking possible. Initial runtime support should explicitly reject compressed
RAR and encryption rather than downloading/extracting entire files implicitly.
[Reader API/source](https://github.com/nwaples/rardecode/blob/v2.4.1/reader.go),
[volume options](https://github.com/nwaples/rardecode/blob/v2.4.1/volume.go),
[RARLAB solid/dictionary specification](https://www.rarlab.com/technote.htm).

Libarchive's BSD-style readers are not a full substitute: old RAR rejects solid
and encrypted archives; RAR5 handles solid/multipart extraction but rejects
encryption, lacks entry seeking, and limits dictionaries to 64 MiB.
[RAR implementation](https://github.com/libarchive/libarchive/blob/master/libarchive/archive_read_support_format_rar.c),
[RAR5 implementation](https://github.com/libarchive/libarchive/blob/master/libarchive/archive_read_support_format_rar5.c).

Declared licenses: [anacrolix MPL-2.0](https://github.com/anacrolix/torrent/blob/d913b30f520e10e85a94c7dc4b7b3e828b22c4b2/LICENSE),
[rardecode BSD-2-Clause](https://github.com/nwaples/rardecode/blob/v2.4.1/LICENSE),
[Nuvio GPL-3.0-or-later and dependency notices](https://github.com/NuvioMedia/nuvio-engine/blob/v0.1.4/THIRD_PARTY_NOTICES.md).
The existing gateway's exact pinned unrar-rs 0.10.8 is GPL-3.0-or-later with the
UnRAR restriction; [local provenance](../PROVENANCE.md) already records its
unresolved combination with inherited GPL-2.0-only code. Preserve inherited
notices and exact dependency obligations. This report records declarations;
it does not establish distribution readiness or resolve licensing by changing
the architecture's name.

## Remaining proof before rollout

No comparative speed tests were run. Measure cold/warm metadata discovery,
first verified playable bytes, first frame, seek recovery, sustained stalls,
CPU/RAM and physical/cache allocation on identical legal test swarms and network
conditions. Compare properly configured upstream rqbit, libtorrent/Nuvio and the
new runtime; separate Nuvio's unverified-block behavior from verified-only results.
VIPTV's existing DHT/TCP-only native configuration is not evidence of an intrinsic
rqbit speed limit. [rqbit v9 notes](https://github.com/ikatson/rqbit/releases/tag/v9.0.0)
add experimental uTP while keeping it disabled by default.

Correctness gates include crash/restart reuse, verification before reads,
eviction during concurrent reading/hashing, missing-piece redownload, aggregate
reservations and temporary-write accounting, multipart/member boundaries,
exact-selection authority, explicit stop with joined cleanup, and unchanged
gateway credential/namespace enforcement. Native Android packaging/DNS/network
behavior needs real qualification; a portable engine API does not prove it.
