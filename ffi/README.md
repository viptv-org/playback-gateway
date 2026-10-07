# Native torrent transport

This crate exposes the gateway's torrent byte transport through UniFFI 0.29.4,
following the shared core's native binding pattern. The native build enables
`playback-engine/torrent` with default features disabled: it includes no archive
extractor/unrar-rs or axum control server and launches no FFmpeg/ffprobe process.
The engine's separate `archive` feature is reserved for archive-enabled consumers;
the default gateway build retains it. Build this facade as the selected package,
not a workspace-wide artifact with unified archive features. It uses a process runtime
with two workers and one client/manager per exclusively locked cache directory.

Methods doing IO block. Call them from Kotlin's `Dispatchers.IO`; progress
callbacks run on native blocking workers and must dispatch UI changes to the
main thread. Keep the client and handle alive while Media3 reads the local URL.
The token is generated internally; never log URLs, magnet values or peer lists.
The consuming Android app needs INTERNET permission and a cleartext network
security exception scoped to loopback `127.0.0.1` for the native player.

## Legacy shared-lease facade

This example uses automatic file selection and shared lease semantics, not an
exact-file authorization adapter.

```kotlin
import uniffi.playback_gateway_ffi.*

val options = defaultTorrentOptions()
val client = TorrentClient(cacheDir.absolutePath + "/torrent-cache", options)
val handle = client.start(magnet, null)
val file = handle.files().filter {
    it.name.substringAfterLast('.').lowercase() in setOf("mkv", "mp4", "webm", "avi")
}.maxBy { it.size }
val media = MediaItem.fromUri(handle.streamUrl(file.index))
player.setMediaItem(media)
player.prepare()
// The loopback endpoint implements byte Range requests for native VOD seeking.
// Release native resources after the player stops reading:
player.stop()
handle.stop()
handle.close()
client.reap()
client.close()
```

File metadata has integer `index`, display `name`, and `size`. `readRange`
returns at most 8 MiB per call and waits at most 30 seconds for missing pieces.
`status` and `setProgressListener` expose byte progress, completion, and stop
state. Calling `stop` invalidates every clone of that shared lease; final
active lease release or explicit stop closes its listener; in-flight readers retain ownership until they settle, then the torrent pauses. `reap` evicts fully idle cache entries after the
idle deadline or when a new reservation needs their space.

## Opt-in independent selected-file leases

`startSelected(magnet, fileIndex)` and
`startSelectedMetainfo(metainfo, fileIndex)` require an explicit metadata index.
Every call allocates a separate stop state, reader accounting and random loopback
capability, even for the same hash/index in one client. Cached payload and its
full-torrent reservation remain shared. `files()` returns only the selected
file, retaining its original metadata index (it need not be zero). Invalid or
unsupported indices fail; no largest-file fallback occurs. `streamUrl`,
`readRange`, HTTP GET/HEAD/ranges and file-size lookup refuse other indices.
`metadataFileCount()` returns the vetted metadata cardinality for exact-index
validation, without exposing another file's name or length.

Stopping or disposing one independent handle does not retire another handle or
its listener. Explicit aliases of a handle still refer to the same lease and
observe its stop. Legacy `start`/`startMetainfo` continue to share same-file lease
state and allow all torrent file indices; gateway/archive callers retain those
semantics. Do not infer independent authority from a legacy handle's clone.

These compatibility methods are not native authorization qualification. Starts
block before a handle exists and plain stop only signals cleanup. Use the owned
strict acquisition APIs below for one startup deadline, cancellable acquisition,
public-only network policy and joined retirement receipts. No capability
advertisement or activation is provided by this facade. Status remains
whole-torrent byte progress, not playback completion or history. Android ABI,
device and full license qualification are separate gates.

Only BitTorrent v1 over TCP is supported. HTTP trackers are stripped before
library admission, private tracker torrents are refused, and webseed metainfo
is explicitly refused because librqbit does not implement webseeds. Discovery
uses DHT and optionally operator-provided bootstrap peers, an explicit transport
exception to the gateway's HTTP private-address policy. Source URI `x.pe`,
`ws`, and `as` parameters are refused. `startMetainfo` accepts bytes fetched by
the application using its own vetted DNS/redirect policy. The native facade
returns torrent payload file bytes; archive unpacking is currently provided by
the gateway control integration, rather than the native facade.

The cache reserves the full torrent payload before downloading. Increase
`maxCacheBytes` for payloads larger than the default 2 GiB; active torrents and in-flight range readers are
never evicted. A stopped handle keeps its payload charged until its pending
range reads complete or are cancelled. Reuse one `TorrentClient` for a cache directory. A second owner
is refused by the exclusive cache lock. HTTP access is limited to loopback,
uses a random capability token, and closes when the lease stops.

Strict native acquisitions measure available filesystem space before allocating
payload storage and check it against the full aggregate reservation, including
unselected files and retiring work. Materialized bytes can be charged again
against current free space; this conservative admission check does not promise
an exact filesystem ceiling or reserve space against unrelated writers. Shared
payload avoids an additional charge only through the manager's existing proven
sharing registry. A refused candidate leaves current grant authority intact.
Native admission fails closed on platforms without a measured disk adapter;
the Unix adapter covers Android and the host fixture platform.

If the embedding process enables Rust tracing, configure the target prefix
`librqbit=off` (including its underscore-prefixed companion crate targets).
The dependency emits peer addresses and torrent identifiers internally; it
must not be allowed to write them to production logs. Gateway and FFI error
surfaces expose static error variants and contain no source diagnostics.

Generate committed Kotlin with `./ffi/scripts/build-bindings.sh` on Linux, with
Python 3 available. The script resolves Cargo's configured target directory,
including `CARGO_TARGET_DIR` and Cargo configuration, rather than assuming a
repository-local target. Its runtime
requires JNA (the generated binding imports `com.sun.jna`),
as with the shared core's UniFFI output. Build Android libraries with
`./ffi/scripts/build-android.sh`; set `ANDROID_NDK_HOME` for a different NDK.
The script builds armeabi-v7a, arm64-v8a and x86_64 at API 24 with one default
build job and verifies ELF LOAD segment alignment of at least 16 KiB. It requires
a clean committed tree and records its source revision, features and library
checksums in `ffi/generated/android/build.json`. Native builds use reviewed
alignment-only Rust flags; unexpected custom flags are refused. Outputs are
ignored under `ffi/generated/android`.

Export a complete checked consumer bundle with
`python3 ffi/scripts/export-android.py ARTIFACT_DIRECTORY FULL_SOURCE_SHA`.
The exporter matches the source stamp, inventories normal/build dependencies for
all three ABIs, checks dynamic dependencies against Android system libraries and
preserves package licenses/notices. Missing workspace-root notices are retrieved
from the dependency's recorded immutable upstream Git revision and checked
against its Git blob hash. The bundle includes a checksum manifest, native ELF
reports, package inventory, locked source dependencies and notices. It also
preserves installed NDK/LLVM and Rust standard-library supplier notices, which
include build-only components; their presence does not claim all listed code is
linked into the facade. It refuses missing/unverified license provenance.
Import the bundle through the consumer's validator rather than
copying unpinned libraries. Inventory and archive exclusion do not establish
license compatibility or distribution clearance. ABI compilation and host
fixtures do not qualify actual device loading or playback.

## Owned strict selected acquisitions

Construct `TorrentClient.newNativePublic(cacheDir, maxCacheBytes)` for the fixed
public DHT/TCP policy. The legacy constructor cannot call the strict begin APIs.
`beginSelected(magnet, infoHash, fileIndex, verifiedSize, remainingBudgetMillis)`
and `beginSelectedMetainfo(metainfo, infoHash, fileIndex, verifiedSize,
remainingBudgetMillis)` return a `TorrentAcquisition` before queuing IO. The
caller supplies its remaining monotonic startup budget; the engine captures one
absolute deadline before validation and queueing and caps it at thirty seconds.
Metadata, registry waits, initialization and endpoint creation share that deadline.

`verifiedSize` is nullable. A supplied positive size must match the exact
authorized file in vetted metadata. Without a size, canonical v1 metadata must
resolve that explicit index to a positive length before storage allocation;
hash/index remain mandatory and no alternate file is selected. Every payload
reservation includes all metadata file lengths. Matching known/unknown-size
acquisitions may share a proven payload while retaining independent authority.

Keep the acquisition alive while its returned handle is authorized. Poll
`status()` or call `waitReady()` on an IO dispatcher. `cancel()` fences ready
publication and stops byte authority immediately; `cancelAndWait()` uses one
two-second deadline for joined acquisition, listener, connection, body, native
reader and final-consumer dependency retirement. It returns `Settled` or the
static `SettlementFailed`. A late wait result has no usable byte authority.
Explicit clones retain the same grant and cancellation state.

Same hash/index acquisitions share a pending payload and its full-payload cache
charge. Their grants, cancellation, random tokens and listeners are independent.
Different selected indices have separate dependency jobs and full-payload charges.
Pending jobs retain their provisional directory, session owner and reservation
through failed or dropped acquisitions. Reaping disposes only closed, joined
owners without a sticky settlement failure or remaining public handle references.
An unresolved or failed owner fences further strict admission; a failed settlement
keeps its charge and cache lock even after delayed work eventually exits.

Strict handles reject `setProgressListener` before spawning any foreign callback.
Use bounded status polling; strict progress exposes only the selected file's
size and verified bytes. `stopAndWait()` provides joined handle retirement;
legacy handles return `SettlementFailed` because their foreign callbacks do not
have joined invocation receipts. Legacy start methods and explicit clone aliases
retain their shared/all-file compatibility behavior.

The literal `127.0.0.1` endpoint uses an independent unpredictable 244-bit token
per grant. GET, HEAD, native reads and single ranges are selected-file only.
Wrong tokens, other indices, traversal, malformed ranges and multiple Range
headers return no file bytes; other file sizes are not disclosed.

The `test-network-policy` feature adds a separately compiled
`newNativeOwned` constructor for exact owned TCP fixtures. It is absent from the
normal generated Kotlin and Android native artifacts. Run these fixture binaries
in a loopback-only namespace, with DHT and trackers disabled. Rebuild the normal
archive-free library before regenerating committed Kotlin with the existing
`build-bindings.sh` script.

These APIs expose honest completion receipts. Arbitrarily stalled synchronous OS
IO cannot be forced to exit within two seconds in process; it produces retained,
sticky failed settlement rather than successful cleanup. Android ABI, decoded
media, public networking, device resources and distribution remain separate
qualification gates. Native activation is not provided by this facade.

Run `./ffi/scripts/check-native-deps.sh` to verify the default facade and explicit
Android native dependency graphs exclude archive/control crates. Both artifact
build scripts require this check. Dependency exclusion is not a complete license
review or device qualification.

Run `CARGO_BUILD_JOBS=1 cargo test --locked -p playback-gateway-ffi`. The host
fixtures use owned TCP seeders with DHT/trackers disabled and exercise legacy
selection, independent selected-file leases, seek/range bytes, callbacks and
stop. On Linux run synthetic seeder tests in a loopback-only network namespace:
the pinned dependency's peer listener binds wildcard interfaces, unlike the byte
endpoint's literal `127.0.0.1`. Keep fixture peer listeners inaccessible from
public/LAN interfaces. Strict clients use the maintained public-only dependency
policy; these isolated owned fixtures do not qualify public-network operation.

The alignment flags follow [Android's page-size guidance](https://developer.android.com/guide/practices/page-sizes). ELF alignment does not verify APK ZIP alignment or device behavior.
