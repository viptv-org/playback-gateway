# Native torrent transport

This crate exposes the gateway's torrent byte transport through UniFFI 0.29.4,
following the shared core's native binding pattern. The native build enables
`playback-engine/torrent` with default features disabled: it includes no axum
control server and launches no FFmpeg/ffprobe process. It uses a process runtime
with two workers and one client/manager per exclusively locked cache directory.

Methods doing IO block. Call them from Kotlin's `Dispatchers.IO`; progress
callbacks run on native blocking workers and must dispatch UI changes to the
main thread. Keep the client and handle alive while Media3 reads the local URL.
The token is generated internally; never log URLs, magnet values or peer lists.
The consuming Android app needs INTERNET permission and a cleartext network
security exception scoped to loopback `127.0.0.1` for the native player.

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

Generate committed Kotlin with `./ffi/scripts/build-bindings.sh`. Its runtime
requires JNA (the generated binding imports `com.sun.jna`),
as with the shared core's UniFFI output. Build Android libraries with
`./ffi/scripts/build-android.sh`; set `ANDROID_NDK_HOME` for a different NDK.
The script builds armeabi-v7a, arm64-v8a, x86 and x86_64 at API 23 with two
build jobs and verifies ELF LOAD segment alignment of at least 16 KiB.
Outputs are ignored under `ffi/generated/android/jniLibs`; package those
libraries and `ffi/generated/kotlin` into the consuming Android project.
ABI compilation and host fixtures do not qualify actual device playback.

Run `CARGO_BUILD_JOBS=2 cargo test -p playback-gateway-ffi`. The host fixture
uses a loopback BitTorrent seeder and exercises selection, seek/range bytes,
progress callbacks, and explicit stop without contacting the public network.

The alignment flags follow [Android's page-size guidance](https://developer.android.com/guide/practices/page-sizes). ELF alignment does not verify APK ZIP alignment or device behavior.
