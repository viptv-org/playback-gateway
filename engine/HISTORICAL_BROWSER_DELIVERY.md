
# Historical browser-first delivery notes

Preserved extraction context, not the standalone gateway's deployment contract.

Set `VIPTV_BROWSER_PREPARATION=1` to admit version-1 browser requests with
`inspect_original`. The default is off. The TV bundle separately enables
`VITE_BROWSER_PREPARATION=1` and `VITE_LOCAL_MSE_REMUX=1` (Docker build arguments
`BROWSER_PREPARATION` and `LOCAL_MSE_REMUX`). Legacy requests retain their policy.

The original HTTPS source stays behind an opaque revocable media capability.
Preparation discovers the transport without waiting for FFprobe; inspection is
coalesced and bounded when conversion or canonical track metadata is needed.
Network/authentication failures remain distinct from media incompatibility.
Sequential live sources report byte progress and consumer backpressure; EOF is
an input failure for live supervision.

Qualified MSE clients can receive bounded continuous fragmented MP4 for copied
VOD video, including audio-only conversion. Seeking uses a preceding keyframe
and reports its actual origin. Heartbeats optionally carry consumed position;
production follows media-time lead. Encoded browser HLS can use fMP4 init/segment
files. Versioned copied HLS never uses non-keyframe `split_by_time`; legacy
clients retain that behavior during the independent browser rollout.

`tracks.json` exposes canonical stream indexes. Caption windows extract only
text to VTT; they do not encode audio/video. Browser-local track IDs must never
be interpreted as these indexes. No provider URL or authorization is returned
in browser track metadata.

See [browser qualification](https://github.com/viptv-org/video/blob/af63620913af8c8cbe3f252a6c64e7dc235df87f/docs/BROWSER_PIPELINE.md) in the
organization workspace and [transcoder benchmark](../../docs/TRANSCODER_BENCHMARK.md).

Copied live delivery also uses continuous fragmented MP4 for qualified browsers.
A production source exposed irregular GOPs that raised HLS TARGETDURATION from
2 to 3 after startup; strict playlist refusal then exhausted the client's buffer.
Continuous output preserves reference frames without requiring fixed segment
boundaries. Live sessions remain unthrottled, use output media-time progress for
supervision, and bypass multi-viewer groups because the body has one consumer.
