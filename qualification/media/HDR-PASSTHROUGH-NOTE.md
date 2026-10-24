# HDR passthrough: follow-up, not an implementation change

Recorded 2026-10-23 at the user's request. Continue the existing qualification
campaign; the HDR questions do not authorize a new passthrough implementation.

An HDR-capable playback chain can preserve HDR and avoid server HDR-to-SDR tone
mapping. Container repackaging or audio conversion may still be necessary. The
player, decoder, display, and connection must support the source's particular
HDR format and codec/profile; HDR10, HLG, and Dolby Vision support are distinct.

An SDR-only client cannot be assumed to convert HDR locally. Some playback
stacks tone-map correctly; others show incorrect brightness/colors or fail.
Local tone mapping must be verified before relying on it.

The current Roku client advertises codecs and maximum resolution, with direct
play disabled, but does not advertise HDR support (`RPlaybackFacts` in
`roku/roku/source/RemoteWire.brs`, in the sibling Roku repository). Its managed
gateway path currently converts HDR to SDR. An HDR-capable TV alone does not
communicate permission to preserve HDR through this path. The existing
WebCodecs original-file path is separate and is not proof of Roku passthrough.

A future proposal should check the actual player/display capabilities and
qualify HDR format/profile, delivered metadata, native rendering, audio-only
conversion/remux paths, and SDR fallback. Any proposal must respect the existing
API/consumer compatibility requirement. No negotiation or playback behavior is
changed by this note.

Roku's official references:

- [Streaming specifications](https://developer.roku.com/dev/docs/media)
- [Device capability reporting](https://developer.roku.com/dev/docs/ifdeviceinfo)

Current measured HDR performance and remaining limitations are recorded in
`hdr-qsv-control-20261023.json`, `hdr-quality-20261023.json`, and
`roku-qualification-20261023.md`. Heavy 4K software tone mapping remains a
capacity limit; preserving HDR could avoid that work on a qualified client, but
this campaign has not qualified that Roku path.
