# Rolling torrent input cache

## Configuration and scope

The standalone service defaults `TORRENT_PIECE_CACHE_BYTES` to 268435456
(256 MiB) per input. `0` disables rolling storage. A positive budget must be
at least 32 KiB, no larger than `TORRENT_CACHE_MAX_BYTES`, and fit at least two
pieces of the admitted torrent. Engine embedders explicitly set
`TorrentConfig::piece_cache_bytes`; its library default remains disabled.

A non-archive torrent larger than this per-input budget reserves that budget
rather than its full logical payload. Small torrents, torrents containing
archive files/volumes retain their full-payload policy. Compressed archives
require a complete extracted spool. Strict native exact acquisition supports the
same bounded piece slots when configured. The Android production facade enables
a 256 MiB per-input cache under its supplied aggregate budget; authorization,
independent grants, measured disk admission and joined retirement remain strict.

All active input reservations and archive spools share the global byte limit.
Concurrent readers of the same namespace/torrent share one reservation; separate
namespaces retain isolation. An input whose reservation cannot fit fails with
the existing capacity error. HLS/progressive output storage remains separately
bounded. The cache filesystem also needs a limit for block rounding/metadata.
Compose's cache tmpfs remains RAM-backed; use a private quota-limited disk mount
in its place when deployment requires disk-backed caching. No deployment or
mount migration is implied by the source configuration.

## Storage and scheduling

One private `pieces.cache` file contains a fixed number of piece-sized slots;
slot reuse never extends the file. Logical file offsets map through torrent
piece indices, including pieces crossing file boundaries. No full-sized payload
files are created, and ephemeral cache metadata is not restored from persistence.

Rolling downloads are driven by active streams, with an interleaved read-ahead
window shared across readers. Idle rolling inputs do not download the rest of
the torrent. The scheduler holds the torrent write lock while choosing a slot
and invalidating its old piece. It resets have/chunk/per-file accounting, so
backward seeks request and hash-verify evicted pieces again. Slot reads/writes
are serialized; current stream pieces, outstanding requests and checksum/IO
guards protect pieces from reuse. Unprotected abandoned partial pieces are
reclaimable. Peer failure retires that peer's rolling reservations.

The rolling mode sends neither bitfield nor HAVE availability and refuses peer
upload requests. Existing protocol availability cannot be retracted safely when
pieces are evicted. Download performance still depends on peer availability;
a seek outside retained data may stall or hit the existing read deadline.

## Verification

`engine/tests/torrent_rolling.rs` uses a real local TCP seeder, a payload much
larger than a 64 KiB cache, multi-file boundary pieces, repeated forward/backward
seeks, sequential reads through EOF, simultaneous readers, aggregate reservation
pressure and release/reaping. Additional tests cover a missing-piece cancellation
and a peer disconnect halfway through a piece followed by successful retry.
It checks byte identity, cache file length and allocated filesystem blocks.
The 100 GiB virtual-torrent fixture generates requested bytes over TCP with
2 MiB pieces and a 4 MiB cache. It verifies seeks beyond 32-bit offsets, reads
near the end, and re-downloads the evicted first piece. It does not allocate or
decode a complete 100 GiB movie.
Maintained storage unit tests cover pinned-slot refusal, abandoned partial-slot
reuse, shared-file boundaries, invalid ranges, storage handoff and closing shared
file descriptors during retirement.

The `scripts/check-torrent-container.sh LOCAL_RUNTIME_IMAGE rolling-torrent`
mode builds the committed fixture source against the selected runtime image.
The opt-in `workers::torrent_tests::rolling_torrent_viewers_decode_seek_with_bounded_input`
fixture uses actual FFmpeg/ffprobe, shared HLS viewers and a separate seek output.
It compares the first decoded seek frame with the requested source timestamp
and checks retained input allocation against the small cache budget.

```sh
CARGO_BUILD_JOBS=1 cargo test --locked -p playback-engine --test torrent_rolling
CARGO_BUILD_JOBS=1 PLAYBACK_TEST_FFMPEG=/path/to/ffmpeg9 \
  PLAYBACK_TEST_FFPROBE=/path/to/ffprobe9 cargo test --locked -p playback-gateway --lib \
  workers::torrent_tests::rolling_torrent_viewers_decode_seek_with_bounded_input \
  -- --ignored --exact --test-threads=1
CARGO_BUILD_JOBS=1 cargo test --locked --workspace
```

Use the FFmpeg 9.0.2 binaries from the qualified runtime image for the media
fixture. Host FFprobe 6.1 lacks the existing required HTTP `request_size` option.

These controlled fixtures do not qualify hostile/public swarms, sustained 4K
bitrate, sparse-piece availability or production deployment. Android decoded playback and compressed archives require their separate
qualification paths. Native TCP tests additionally cover a 100 GiB exact-file
input with 4 MiB retained storage and cross-file reads with independent grants.
