# Shared torrent runtime candidate

This Go module is the shared transport prototype for Android, Android TV,
Linux/Windows desktop and gateway ingestion. It is **not activated in any
production consumer**. Client integration is owner-authorized; performance
improvements remain [follow-up work](../docs/TORRENT_PERFORMANCE_FOLLOWUP.md).
[Qualification](QUALIFICATION.md) records evidence and remaining checks.

The engine is `anacrolix/torrent v1.61.1-0.20260927071845-d913b30f520e`; stored RAR parsing is
`github.com/nwaples/rardecode/v2 v2.4.1`. `go.sum` locks dependency content.
No engine fork is used. Accounts, catalogs, profiles, authorization decisions,
players and output transcoding remain outside this module.

## Interface and ownership

`Runtime.Prepare` creates a cancellable handle without waiting for metadata.
`Observe` reports finding peers, fetching metadata, opening an archive, buffering
or playing. Once ready, the handle owns a token-protected loopback media URL.
`Renew` accepts at most sixty seconds of authority. `ReportFirstFrame` ends the
120-second startup deadline; endpoint readiness does not. `Close` revokes access
and retires the torrent after its final handle closes. It also waits for
unfinished dial/handshake attempts; an upstream Drop by itself does not join
those. The controller enforces the worker settlement deadline during this wait. Cached data is not authority.

The private version-2 JSON controller exposes `open`, `prepare`, `observe`,
`renew`, `first_frame`, `close` and `shutdown` over worker stdin/stdout or JNI.
It is not an HTTP administrative endpoint or the backend's public negotiation
route. Consumer-side protocol negotiation and worker supervision still need
integration. A `terminate` response requires the parent to kill its owned worker;
the executable exits with status 3 when settlement exceeds two seconds.

`prepare` takes a magnet or metainfo, optional tracker hints and file index,
and `authority_ms`. An optional exact `expected_file_size` binds the selected
outer torrent file before serving or opening an archive. Absent file selection means the largest torrent file, with
the first index winning ties. Resolved stored archive selection is reported in progress as `archive_index`
and can be supplied on a subsequent preparation. Observation returns its own
copy so callers cannot mutate live selection. Compressed, solid or encrypted archives are refused.

Each cache directory has one exclusive process owner. The default budget is
2 GiB, reduced to retain 512 MiB of free disk space. Piece records include their
allocation overhead. Incomplete writes, verified data and metadata/routing
records share the budget; complete archive extraction is never staged. Hashing
and reads pin storage. Eviction first makes the piece unavailable to reads and completion queries,
waits for any engine publication, synchronizes engine completion, and only
then deletes its bytes. The allocation charge remains until deletion. A failed
unlink terminates cache access and wakes waiting writers. A restart checks persisted records; incomplete or corrupt records cannot
be served. Final handle closure retains verified data for an authorized replay.

Public peer and tracker destinations are checked, automatic router mapping is
disabled, and discovery enables trackers, DHT, PEX, TCP and uTP. Bench-only
connection, TCP head-start and header-priming experiments are unexported configuration hooks;
their results must not be mistaken for production defaults.

The integration candidate configures an upstream dial burst/rate of 50, fifty
half-open attempts per torrent, a 4 s nominal/1 s minimum connection budget, and
verified head/index priming. Reader demand starts with at most 4 MiB and expands
to the configured bounded window after first-frame acknowledgement. Reference
probes can explicitly retain upstream defaults. These settings leave protocol,
DNS, handshake, request scheduling and hash verification in their libraries.

## Local checks and packaging

Use Go 1.25 or newer and a C compiler:

```sh
go test -race -p 1 -timeout 90s ./...
python3 scripts/check-upstream-wakeup.py
python3 scripts/build-native.py host
python3 scripts/build-native.py windows
ANDROID_NDK_HOME=/path/to/ndk python3 scripts/build-native.py android
```

Set `GO` when the executable is not on PATH. Windows cross-compilation requires
MinGW. Android uses API 24 for arm64-v8a, armeabi-v7a and x86_64; the build script
checks 16 KiB ELF segment alignment and packages the Java byte-array JNI wrapper.
The Android parent must host this wrapper in an app-private worker process.
Only the Go library owns C++ code in that process; its C++ runtime is statically
linked and the JNI bridge is pure C. Every emitted library is checked for
unpackaged dependencies as well as 16 KiB alignment. NDK notices accompany the
artifacts.

The standalone `scripts/build-android-probe.py` packages a separate ABI test app
from a verified artifact inventory. It exercises JNI open/refusal/shutdown in a
private process and records a local result. This does not exercise Media3 or a
product client; use an explicitly selected emulator for its installation.

The transitive Android network-interface dependency `wlynxg/anet v0.0.3`
uses a private Go symbol. Android builds therefore use its upstream-documented
`-checklinkname=0` flag ([anet build notes](https://github.com/wlynxg/anet#how-to-build-with-go-1230-or-later)).
Toolchain upgrades require rechecking that layout and emulator interface access;
successful linking alone does not validate this compatibility seam.

The wake-up check copies the pinned upstream module into a temporary directory
and tests its real peer writer; it never edits the Go module cache. Running it
with `--version v1.61.0` intentionally demonstrates the earlier lost-wake-up
failure. The updated exact pin fixes that race without a local engine patch.

`build.json` records the owning Git revision, dirty-tree flag and artifact
checksums. Dirty artifacts are qualification-only. Release artifacts must all
come from one clean immutable revision; this module alone does not enforce a
consumer's release inventory.

Public swarm probes are explicit opt-ins using a private JSON corpus. Never
commit corpus identities, media URLs, decoded output or raw peer logs. The
ordinary test suite uses locally owned data and peers. See
[research](../docs/TORRENT_RUNTIME_RESEARCH.md) for source citations and the
comparison methodology.
