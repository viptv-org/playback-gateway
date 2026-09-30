# Shared subtitle replay contract

Gateway issue 2 owns generic media behavior only. The intended public seams are
the actual control/media HTTP(S) API and decoded fetched media. No VIPTV UI or
live seek origin changes are included.

One source ingest captures all eligible subtitle tracks, preserving original
sparse track identities independently of deterministic private replay indices.
FFmpeg decodes caption packets to local WebVTT pipes on the absolute media clock.
A bounded gateway-owned cue cache retains long active intervals after the AV
segment containing the original packet is evicted. Authorized output renditions
select overlapping cues by actual AV segment timestamps. Stable cue identifiers
let players recognize a cue repeated across consecutive HLS subtitle segments;
there is one copy of each cue per segment. Public caption stream index is zero
within the actual separate WebVTT rendition.

Qualified shared captions are SubRip on Linux with already
eligible AV codecs. ASS/SSA/WebVTT, bitmap/DVB/PGS/teletext, mov_text/text, unlisted
AV codecs and other platforms retain independent execution. Plain WebVTT
conversion has the existing styling limitations. At most 32 subtitle tracks,
32 KiB per decoded block and 1 MiB/8192 cached cues are accepted. Completed cues
have 180 seconds of reader grace; active intervals survive regardless of age
within those byte/count limits. Overflow stops the ingest with the existing safe
processing failure. A separate kernel media bound is still supplied by Compose;
caption cache limits do not replace the filesystem quota or replay watchdog.

The implementation must prove actual decoded timing, long overlaps, late joining
after original cue-segment eviction, seeking among retained output segments,
single compatible upstream, independent authorization/release/expiry and actual
process/storage teardown against its built image. HTTP and trusted HTTPS sources
must be exercised. Qualification evidence will be recorded after these checks;
this contract document alone does not claim completion or deployment.
