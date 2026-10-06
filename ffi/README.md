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

Stopping or disposing one independent handle does not retire another handle or
its listener. Explicit aliases of a handle still refer to the same lease and
observe its stop. Legacy `start`/`startMetainfo` continue to share same-file lease
state and allow all torrent file indices; gateway/archive callers retain those
semantics. Do not infer independent authority from a legacy handle's clone.

These methods are not authorization-ready native playback qualification. Starts
still block before a handle exists; stop aborts rather than joins the listener,
and missing-piece reads can wait up to 30 seconds. Total acquisition deadlines,
cancellable acquisition, bounded joined reader/session/dependency settlement,
public-only peer/DHT/bootstrap filtering and bounded dependency metadata
allocation remain required work. No capability advertisement or activation is
provided by this facade. Status remains whole-torrent byte progress, not playback
completion or history. Android ABI/device and full license qualification are
separate gates.

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
The script builds armeabi-v7a, arm64-v8a, x86 and x86_64 at API 23 with two
build jobs and verifies ELF LOAD segment alignment of at least 16 KiB.
Outputs are ignored under `ffi/generated/android/jniLibs`; package those
libraries and `ffi/generated/kotlin` into the consuming Android project.
ABI compilation and host fixtures do not qualify actual device playback.

Run `./ffi/scripts/check-native-deps.sh` to verify the default facade and explicit
Android native dependency graphs exclude archive/control crates. Both artifact
build scripts require this check. Dependency exclusion is not a complete license
review or device qualification.

Run `CARGO_BUILD_JOBS=2 cargo test --locked -p playback-gateway-ffi`. The host
fixtures use owned TCP seeders with DHT/trackers disabled and exercise legacy
selection, independent selected-file leases, seek/range bytes, callbacks and
stop. On Linux run synthetic seeder tests in a loopback-only network namespace:
the pinned dependency's peer listener binds wildcard interfaces, unlike the byte
endpoint's literal `127.0.0.1`. Keep fixture peer listeners inaccessible from
public/LAN interfaces; public-only dependency networking remains unimplemented.

The alignment flags follow [Android's page-size guidance](https://developer.android.com/guide/practices/page-sizes). ELF alignment does not verify APK ZIP alignment or device behavior.
