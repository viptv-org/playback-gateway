# Experimental retained-audio subtitle synchronization

Disabled unless both `VIPTV_SUBTITLE_SYNC_VAD` and
`VIPTV_SUBTITLE_SYNC_ESTIMATOR` name trusted executable files. Playback and
caption attachment start independently. The optional worker never reads a
provider URL, requests torrent pieces or holds an output producer/renderer.

| Delivery | Acquisition |
| --- | --- |
| HTTP VOD converted/remuxed to retained TS HLS | Completed local output segments |
| Torrent/archive VOD converted to retained TS HLS | Same output segments; no torrent/range reader |
| VOD HLS converted to retained TS HLS | Same output segments; decoder cannot open nested HTTP resources |
| Direct/original HTTP, torrent or HLS | Unavailable |
| Progressive/fMP4, live/shared replay | Unavailable |
| Nonzero playback origin, missing initial segments, discontinuities, encrypted output | Unavailable |

One global worker slot covers snapshot acquisition, VAD and estimation; overload
abstains without a pending-work queue. One analysis slot consumes a 60–120 second snapshot, at most 32 MiB input and
6000 mono 16 kHz/20 ms frames. FFmpeg receives TS bytes through stdin with only
the `pipe` protocol permitted. Decoded frame PTS are normalized against the
retained output origin; gaps remain gaps. The native WebRTC VAD emits one byte
per complete frame. The core estimator receives batches of at most 3000 frames.
Each child has a 15-second deadline, bounded output and kill-on-drop ownership.
Decoder/filter threads are capped at one. Cache holds one job's activity spans,
never PCM, provider URLs or subtitle text. Ambiguity ends that snapshot attempt;
it does not repeatedly decode the same bytes.

The fixed-offset path is experimental. Its 120-second horizon cannot qualify
affine drift. Normalized output timestamps, AAC priming and selected-track
mapping need real-release qualification. Resource ceilings on buffers and work
are not a measured 64 MiB/10% CPU guarantee or a paired playback result.

## Authenticated control lifecycle

`POST /v1/sessions/:id/subtitle-sync` accepts `content_vtt` and
`playback_generation`. GET reads its record; PUT accepts `{revision,manual}`;
DELETE requires the same `playback_generation`. Create scope is required for
mutations and read scope/ownership for reads. Every operation revalidates the
viewer lease; background work also checks key revocation/expiry. Manual freeze
cancels analysis, retains the current correction and rejects late writes.
Replacement, release and expiry invalidate the old work.

Records contain version, playback generation, session media identity, selected
session audio identity, canonical subtitle SHA-256, algorithm, monotonic server
revision, manual policy, state, optional correction and evidence.
`confidence_kind` is `uncalibratedHeuristic`. Latest observation evidence and
accepted correction evidence are separate. Microsecond correction fields are
`anchorUs` and `offsetUs`; positive offset displays later.

Identities are scoped to this immutable job/selected track within the viewer
lifecycle. They do not certify a full release's content hash, and are never used
for persistent or cross-job confidence reuse. Expired/replaced viewers cannot
read another viewer's records.

## Local prototype tools

`python3 tools/subtitle-sync/build-vad.py --directory <empty-owned-directory>`
builds the native WebRTC code pinned by commit from py-webrtcvad and retains its
complete upstream LICENSE beside the executable. The executable reads little
endian mono s16 at 16 kHz in 320-sample frames and writes byte 0/1 decisions.
The MIT Python wrapper is not shipped or executed.

Build core's `subtitle_sync` example at immutable commit
`fec3550db0786ac9bf1b91bf31edd7b0865f88a1`. Its JSON stdin contract is
`{epoch,cues,batches}` with canonical cue intervals and timestamped activity;
stdout is the bounded estimate. The algorithm is shared; no Python aligner or
speech transcription is included.

`make-fixture.py` creates a bounded owned harmonic/silence WAV and TS-HLS fixture
using an explicitly supplied FFmpeg. It is a mechanism fixture, not dialogue or
alignment accuracy evidence. The ignored engine test
`real_retained_audio_is_local_bounded_and_reuses_activity` consumes that fixture
when `PLAYBACK_TEST_FFMPEG`, `PLAYBACK_TEST_FFPROBE`, `PLAYBACK_SYNC_VAD` and
`PLAYBACK_SYNC_FIXTURE` are set. Optional `PLAYBACK_SYNC_OBSERVATIONS` writes
fixture activity to an owned path. Never point these tools at provider traffic
or production storage.


The ignored authenticated worker test seeds an isolated producer ledger with
owned completed segments, then runs the real decoder/VAD/estimator through the
control API. `make-reference.py` can build a deliberately circular cue reference
from the fixture VAD observations with a known two-second offset. That checks
mechanism wiring and sign; it is not independent speech, held-out accuracy or
playback-impact evidence. Set `PLAYBACK_SYNC_ESTIMATOR`,
`PLAYBACK_SYNC_CAPTIONS` and optional `PLAYBACK_SYNC_RECORD` to exercise it.


## Executed mechanism evidence

[Validation manifest](SUBTITLE_SYNC_VALIDATION.json) records exact source/binary
hashes. The owned 120-second retained fixture yields 6000 timestamped frames;
the native VAD marks 3516 speech frames. A deliberately circular reference
shifted by two seconds produces an accepted offset of +2000000 us at server
revision 2 through the authenticated control lifecycle. Foreign-viewer reads are
rejected and the same playback job remains Ready. The engine also reuses cached
activity when a second decoder/VAD executable is absent.

The decoder is userspace Debian FFmpeg 5.1.9 with reused shared dependencies;
no host packages were installed. A separately downloaded static FFmpeg 7.0.2
crashed on MPEG-TS input and was excluded from mechanism evidence. Both decoder
versions differ from the prior isolated production-image baseline. Full producer
and paired playback/resource/held-out accuracy qualification remain open.
