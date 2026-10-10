# Torrent startup control v2

The v1 control routes and response envelopes retain their compatibility behavior.
V2 torrent preparations keep the shared runtime's startup/progress watchdog
active after FFmpeg output readiness until the authorized decoder reports its
first frame. Native clients already report directly to their local runtime.

All routes require a current integration control credential. A media credential
cannot call them. No route accepts product accounts, profiles or source policy.

- `GET /v2/torrent-runtime-protocol` requires `capabilities` scope and returns
  exactly `{"version":2,"first_frame_ack":true}` when torrents are enabled;
  the boolean is false when disabled. The v1 capabilities envelope is unchanged.
- `POST /v2/torrent-sessions` takes the existing create envelope and
  `Idempotency-Key`, with the existing namespace/create admission checks. Only
  torrent inputs are accepted. Poll, renew, release and preparation cancellation
  use their existing v1 routes and viewer identifiers.
- `POST /v2/torrent-sessions/:id/first-frame` takes exactly `{}` and requires
  read and renew scopes, the original integration owner, the viewer namespace,
  a ready unexpired viewer and its live owned source. It returns 204, is
  idempotent and does not extend the viewer lease. Report actual decoder
  presentation, not endpoint readiness, playlist availability or player Ready.

The original 120-second startup and 60-second meaningful-progress watchdogs
remain active while awaiting acknowledgement. An acknowledgement cannot revive
a failed/expired runtime handle. Closing still revokes access and preserves
admission until owned cleanup settles.

The route chooses the private acknowledgement mode. It is not a field accepted
by the v1 JSON envelope. Sharing, source resolution, quota and idempotency
identities include it, so a legacy output-ready acknowledgement cannot retire a
v2 handle's watchdog. V2 viewers can still share a compatible v2 output/input;
one actual decoder acknowledgement establishes consumption of that input.

Controlled checks cover mode isolation, legacy wire rejection, foreign-key and
namespace refusal, expiry, duplicate acknowledgements and unchanged lease
expiry. The real Go/FFmpeg fixture confirms output readiness leaves v2 awaiting
a frame, independently from a legacy handle, then decodes output before reporting
the frame. Backend/web/Roku adoption is integrated. An actual authenticated
HTTPS browser run presents frames before acknowledgement and revokes media on
Stop. Physical Roku presentation remains separately coordinated; controlled
gateway checks alone do not qualify its hardware decoder.
