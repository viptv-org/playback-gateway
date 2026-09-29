# Media execution engine

Generic Rust library extracted with its process/media regression tests. Callers
supply input URL/headers, measured decoder capabilities, timeline and track
selection; the engine owns probing, passthrough/remux/transcode and cleanup.

The library has no product database or account model. It is not a complete public
API/security boundary: callers must enforce authorization and network policy for
all media destinations. The control crate is developing that boundary separately.

`cargo test -p playback-engine` runs the fixture suite. Opt-in media tests require
`PLAYBACK_TEST_FFMPEG` and `PLAYBACK_TEST_FFPROBE`; GPU cases additionally require
their explicit hardware configuration. A fixture pass is not hardware acceptance.

Use a dedicated media directory and never share it between engine instances.
Shutdown/cleanup waits for input processes before releasing reservations.
Decoder dimensions remain validated/even, but are not silently capped at 1080p.
