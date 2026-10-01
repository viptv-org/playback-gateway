# Playback API v1

The backend is an integration, not a privileged in-process consumer. Neither
product account/profile IDs nor provider-specific catalog logic appear here.
All public traffic to this API must use HTTPS. Inputs independently support HTTP
and HTTPS. URLs and required upstream headers are confidential.

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

This is a schema example, not a playable public URL. `progressive` requests
are currently rejected with `unsupported_output` for every input: no shared
progressive output exists yet, and there is no silent switch to HLS. Dimensions represent decoder capabilities,
not a product quality preference. Track indices are absolute input indices.
Live sessions start at zero. Source URLs cannot contain embedded userinfo or
fragments. At most 32 upstream headers, with 4096 bytes per value, are accepted;
transport-control headers and ambiguous duplicate names are rejected.

The response contains `id` (viewer ID), `job_id`, `status`, `expires_at`,
`error_code`, `renew_after_seconds`, and `playback` when ready. Playback contains
the viewer-scoped relative media URL and format/track metadata, never an upstream
URL, engine capability or integration key. A ready response is `201`; joining an
already-starting worker can return `202`. A normal start waits up to 30 seconds.
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
