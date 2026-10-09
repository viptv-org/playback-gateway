# Playback API v1

The backend is an integration, not a privileged in-process consumer. Neither
product account/profile IDs nor provider-specific catalog logic appear here.
All public traffic to this API must use HTTPS. Inputs independently support HTTP
and HTTPS. URLs and required upstream headers are confidential.

## Torrent and archive inputs

When `/v1/capabilities` reports `torrent: true`, `input.url` additionally accepts
a BitTorrent v1 `magnet:?xt=urn:btih:...` URI, an HTTP(S) `.torrent` URL, or a
direct HTTP(S) stored `.rar` URL. The runtime opt-in is `TORRENT_ENABLED=true`;
disabled requests return `422 unsupported_output` without source I/O.
These inputs require `live: false`; `live: true` returns `422 unsupported_output`.
Optional `input.file_index` (0–65535) selects a torrent payload entry. It is
invalid on ordinary HTTP media/archive inputs. Without it, selection chooses
the largest recognized media/archive payload. Missing/unsupported selections
return `422 unsupported_output`.

Magnets allow one v1 hash and optional display name/tracker parameters. They
refuse source-provided peers, alternate sources and embedded credentials.
Trackers are not contacted; private tracker torrents and webseed metainfo are
unsupported. DHT/peer traffic is the documented opt-in transport-policy exception.
Metainfo and direct archive fetches retain vetted DNS/redirect/egress rules.
Headers apply to those HTTP fetches; magnets require empty headers.

Ready responses additionally include `input_file: {"index":0,"name":"clip.mkv",
"size":123456}` for the selected playable file. Archive metadata describes the
selected member. Only its basename/size/index are returned, never an infohash,
peer list, cache path or private loopback capability. Namespaces, key quotas,
idempotency, viewer leases and media authorization retain their existing rules.
Pending source preparations count toward admission before network activity;
cancellation retains their charge until owned work settles.

Stored RAR members support byte ranges across ordered volumes with no spool
copy. Compressed torrent members must finish bounded, verified extraction before
playback, then support seeking within the completed spool. Partial-progress seek,
encrypted archives and direct compressed HTTP RAR are unsupported. Capacity
refusal is `429 torrent_cache_capacity`; preparation timeout is `504
startup_timeout`; preparation/processing failures are redacted `502` errors.
Outputs continue through the existing FFmpeg planner; public raw torrent/archive
bytes are not served. Native torrent byte delivery is a separate local
[Kotlin facade](ffi/README.md).

## Credentials

`Authorization: Bearer <API_KEY>` authorizes only bootstrap key management:

- `POST /v1/keys`: label, namespaces, scopes, quotas and optional Unix expiry;
  returns a random integration secret once (`201`).
- `GET /v1/keys`: redacted key records; no secrets.
- `DELETE /v1/keys/:id`: idempotent revocation (`204`) and viewer release.

Integration scopes are `capabilities`, `create`, `read`, `renew`, `release`.
Namespaces are exact-match strings. Quotas have `inputs`, `outputs`, `viewers`.
Integration secrets authenticate control requests only. Viewer media credentials
authenticate one viewer's resources only; they cannot access control routes.

## Session lifecycle

`POST /v1/sessions` requires a valid `Idempotency-Key` (1–128 letters, digits,
hyphens or underscores) and a JSON body shaped as follows:

```json
{
  "namespace": "example",
  "input": {
    "url": "http://media.example.test/live.ts",
    "headers": {},
    "live": true
  },
  "output": {
    "protocol": "hls",
    "conversion": "auto",
    "video_codecs": ["h264"],
    "audio_codecs": ["aac"],
    "max_width": 3840,
    "max_height": 2160,
    "audio_track": null,
    "subtitle_track": null,
    "audio_language": null,
    "preferred_audio_language": null,
    "preferred_subtitle_language": null,
    "subtitles_off": false
  },
  "position_seconds": 0
}
```

`conversion` is `auto` (default), `audio`, `video`, or `audio_video`. It explicitly
requests which media components must be encoded; automatic mode retains the
copy/remux-first planner. These are closed choices, never arbitrary FFmpeg
arguments. Track indices are absolute input indices bounded to 65535. Optional
language tags are at most 35 ASCII alphanumeric/hyphen characters. Explicit track
selection and language preferences are passed to the inspected engine planner.
`subtitles_off` cannot be combined with a subtitle index or preferred subtitle
language. Normalized output plans, including chosen tracks and conversion modes,
still determine worker sharing; request preferences cannot merge incompatible
outputs. Use matching control clients/gateway builds for these new fields.

This is a schema example, not a playable public URL. Live `progressive` is
accepted only for the supported subset described in
[progressive live delivery](#progressive-live-delivery), which also states the
on-demand behavior; every other live `progressive` request fails with
`unsupported_output`, with no silent switch to HLS. Dimensions represent decoder capabilities,
not a product quality preference. Track indices are absolute input indices.
Live sessions start at zero. Source URLs cannot contain embedded userinfo or
fragments. At most 32 upstream headers, with 4096 bytes per value, are accepted;
transport-control headers and ambiguous duplicate names are rejected.

The response contains `id` (viewer ID), `job_id`, `status`, `expires_at`,
`error_code`, `renew_after_seconds`, and `playback` when ready. Playback contains
the viewer-scoped relative media URL and format/track metadata, never an upstream
URL, engine capability or integration key. A ready response is `201`; joining an
already-starting worker can return `202`. HTTP starts retain a 30-second preparation budget. Torrent starts have a
120-second budget and renew their viewer lease while the authenticated create
request remains active. The private runtime separately enforces 60 seconds
without meaningful startup progress.
Repeating an identical idempotency key reuses the viewer and its media URL;
changing the body under that key returns `409`.

- `GET /v1/sessions/:id` reads authorized state and the same media URL.
- `POST /v1/sessions/:id/renew` renews a live viewer lease; no body is required.
- `DELETE /v1/sessions/:id` releases that viewer (`204`), not the shared job.
- `GET /v1/capabilities` reports protocol version, supported delivery transports,
  own namespaces, operation scopes and configured quota limits. Integrators can
  validate key permissions without creating a playback session.
  The available object reports remaining inputs/outputs/viewers for that key;
  output availability also respects the engine's global worker admission. These
  are advisory snapshots, not reservations. Stopping jobs remain charged until
  teardown; a compatible viewer may reuse an existing job despite no room for
  a new input/output.

Leases last 60 seconds; renew every 20. Media reads do not renew leases. Shared
workers have 15 seconds of zero-viewer grace. Startup and cleanup retain quota
until workers stop. Expired/released sessions cannot be revived by renewal.
Eligible live replay outputs share one input across differing normalized plans.
Independent fallback or VOD output pipelines each consume an input reservation;
viewers joining an identical existing job do not reserve another input. This
accounting does not promise universal single-ingest for every source/transport.

Disguised HLS inputs (segments named or typed as images/stylesheets, or with
an image/CSS prefix before the media) need no integration change: the engine
reads them through a private loopback sanitizer that applies the input's
headers and egress route itself. A disguised input whose segments are on
another origin behind an egress proxy is refused at inspection.

### Progressive live delivery

Decision recorded 2026-09-30 (playback-gateway#1), extended to Matroska on
2026-10-02 (playback-gateway#11): original progressive
delivery is a same-container lossless remux, not byte-identical forwarding.

- **Supported subset.** `input.live` is `true`; the input container is
  MPEG-TS whose private replay is MPEG-TS (delivered as MPEG-TS, `video/mp2t`,
  `stream.ts`), or Matroska whose private replay is Matroska (FFV1 video, FLAC
  audio or text subtitles; delivered as Matroska, `video/x-matroska`,
  `stream.mkv`); the video and the selected audio codecs are inside the private
  replay set and in the request's `video_codecs`/`audio_codecs`; the picture is
  progressive and within `max_width`/`max_height`; `conversion` is `auto`; and
  no subtitle track or preferred subtitle language is requested. Live HLS, MP4
  and other inputs, mismatched container families (for example H264/AAC in
  Matroska, whose replay is MPEG-TS, or an MPEG-TS input whose replay would be
  Matroska), and any live request needing an encode fail at request time with
  `unsupported_output`, before any ingest, reader or reservation starts.
  On-demand `progressive` requests are outside this contract: they keep the
  pre-existing original-file transport when the original container and codecs
  are compatible, and otherwise fail with `unsupported_output`.
- **Execution.** Compatible viewers in one namespace with the same source and
  headers share one authenticated upstream ingest, the same one shared HLS
  outputs use. Each viewer gets its own job: a copy-only FFmpeg reader of the
  private replay that can open only gateway-generated local files. An MPEG-TS
  reader reads the private playlist. A Matroska reader opens nothing but its
  own stdin: the gateway follows the private Matroska playlist and writes one
  continuous Matroska stream to it (the EBML header, an unknown-size Segment,
  Info and Tracks once, then only the Clusters of each listed segment in
  order), because the private segments are complete Matroska files that a
  single demuxer cannot read back to back. The output
  carries the first video and the one selected audio track (or none);
  metadata, chapters, data and subtitle streams are dropped. No packet is
  re-encoded.
- **Join and seek.** A reader starts at the second-newest listed replay segment,
  at a segment boundary, which is a keyframe, and writes fresh container headers.
  A Matroska reader that falls so far behind that a segment it has not read was
  evicted fails rather than skip media.
  There are no byte ranges (`Accept-Ranges: none`), no seeking and no content
  length. Each viewer's media URL can be opened once with `GET` (`HEAD` is
  answered without consuming it). A second `GET` on the same viewer fails, so
  reconnecting requires a new viewer session.
- **Bounds.** Each reader has a bounded in-memory channel (32 chunks of 64 KiB).
  A reader that leaves it full for 20 seconds, including a viewer that never
  opens its URL, is evicted: its stream ends with an error and its job fails
  with `processing_failed`. The shared replay keeps its own time and byte
  window and deletes segments regardless of reader position, so a slow or
  stalled reader never retains shared storage or delays another viewer.
- **Termination.** A live progressive body never ends cleanly. When its job
  fails (storage exhaustion, eviction, ingest or reader failure), is released
  or expires, or its reader stops for any reason, the gateway aborts the
  chunked response without a terminating chunk (the connection is reset), so a
  client sees a transport error, never an apparently complete stream. A
  client must treat any end of a live progressive body as a failure and start
  a new viewer session to continue.
- **Lifecycle.** Each viewer has its own media token, lease, expiry and release.
  A closed client connection ends that viewer's reader. Releasing or expiring a
  viewer stops only its reader after the normal idle grace. When the last reader
  of an input is torn down, the ingest is cancelled, its process reaped and its
  storage removed; reservations are released only after that teardown.
- **Quotas.** Every progressive viewer uses one output reservation. All outputs
  of one live input, progressive or HLS, use one input reservation.

### Shared live text subtitles

Subtitle metadata keeps the absolute source `input_index`. A selected HLS
subtitle is a separate WebVTT rendition: `output_index` and
`output_stream_index` are both zero within that rendition, never native player
track IDs or indices in the AV segment. This corrects the earlier combined-map
stream index; clients must resolve native tracks through their player.

The Linux shared replay adapter qualifies SubRip alongside
the eligible AV codecs listed in `docs/LIVE_SHARING_FORMATS.md`. All eligible
caption tracks are retained even when the first output disables subtitles.
Compatible viewers and differing HLS output plans share one input. Caption
selection remains part of the output plan; release, expiry and credentials
remain viewer scoped.

Decoded WebVTT preserves cue intervals and overlapping cues across replay
eviction, with stable cue IDs in dependent segments. Styling follows the
existing conversion to plain WebVTT. Live control position remains zero; seeking
here means a player seeking within currently retained HLS segments, not a new
control API rewind origin. A reader outside retained media can fail.

Caption blocks are bounded at 32 KiB and the shared cue cache at 1 MiB/8192 cues;
completed cues retain 180 seconds of reader grace, while still-active cues remain
until their end. Overflow or invalid caption timing fails the affected ingest
safely, rather than silently dropping captions. The supported Docker media
tmpfs bound also remains required. ASS/SSA/WebVTT, bitmap, DVB/PGS/teletext, mov_text/text,
unqualified codecs and non-Linux subtitle replay keep the independent path.
These are qualification boundaries, not a universal subtitle-format promise.

## Media and errors

Returned `/media/<viewer>/<capability>/<file>` URLs authorize `GET`/`HEAD` for
playlists, dependent segments, initialization data and other engine resources.
Child references remain under that viewer URL. Release, expiry and integration
key revocation deny subsequent requests. Media CORS permits GET/HEAD and Range
without cookies, allowing browsers to consume their private capability URL.
All API/media responses use `Cache-Control: no-store` and `Referrer-Policy:
no-referrer`; do not place capability URLs in logs or analytics.

Errors use `{"error":{"code":"..."}}`, never raw subprocess/provider output.
Invalid requests are `400`, invalid credentials `401`, missing operation scope
`403`, unknown/foreign session IDs `404`, expired renewal `410`, incompatible
output `422`, and key quota exhaustion `429`. Startup timeout is `504`; worker
and source preparation failures currently use safe generic `502` codes. More
specific upstream failure classification remains an acceptance item.

`/health` and `/ready` expose no source, credential or job information. HTTPS
ingress, pending-inspection admission hardening and multi-output input sharing
are deployment acceptance requirements, not implied by a successful health check.

## Shared torrent input fields

Torrent VOD inputs accept `file_index` (otherwise largest file), `archive_index`
(stored-RAR member; otherwise largest member), `expected_file_size` (positive
outer torrent-file length, requiring explicit `file_index`) and up to 32 public
UDP/HTTP(S) `trackers`. Existing input bodies remain valid. File/member indices
are bounded to 65535; exact sizes are bounded to JavaScript's safe integer range.
These fields participate in source identity; they cannot merge incompatible
selection constraints. Native v1 negotiation is a separate backend contract.
Compressed/encrypted/missing archive errors and startup phase failures remain
closed codes; no source URL, private runtime token or decoder stderr is returned.

`GET /v1/preparations/:idempotency_key` reports `{ "stage": null | string }`
while that key's torrent create request is active. Only its integration key
and namespace with read scope can observe it. Stages are `finding_peers`,
`fetching_metadata`, `opening_archive` and `buffering`; no private source,
capability, byte count or authority is returned. Missing/retired requests return
404. Observing progress never renews a viewer or private runtime lease.
Existing session responses and protocol negotiation remain unchanged.
