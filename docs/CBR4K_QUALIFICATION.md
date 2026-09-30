# Bounded sustained 4K copy qualification

Run the committed-source fixture against a locally built gateway image:

```sh
sudo -n bash scripts/check-cbr4k-container.sh LOCAL_GATEWAY_IMAGE
```

The controller uses only a disposable network-none container and synthetic
public-address loopback source. Its setup privileges are not the service's
production envelope. The actual gateway child runs as UID/GID 10001. The script
removes only its returned container ID and anonymous fixture data volume; local
images remain. No provider, production data, device, deployment or hardware is used.

The fixture generates a static black H264 Constrained Baseline/yuv420p/level 5.1
3840x2160, 5 fps, 24-second/120-frame clip with two libx264 encoder threads,
40 Mbit/s VBV CBR and `nal-hrd=cbr`. Generation has a 20-second timeout.
[Official x264 options](https://code.videolan.org/videolan/x264/-/raw/master/x264.c)
state that this mode implies filler. No MPEG-TS muxrate/null-padding option or
filler-removing bitstream filter is used. Actual demuxed H264 packet byte counts
are asserted and printed separately from transport file lengths.

The HTTP producer paces approximately 40 Mbit/s. Two compatible viewers share
one copy output and one active upstream; creating the second viewer must not
open another source connection. Over 18 seconds of observation the actual
completed private replay files must exceed the old 64 MiB threshold, the session
must remain ready, and allocation must remain within the kernel media mount.
One real output frame is decoded, then both viewers are released. After the
existing idle grace the media processes, upstream and all media allocations
must disappear. The generated 24-second clip avoids timestamp looping during
the retention observation; the server never deliberately sends EOF.

This explicitly configured workload uses a 512 MiB logical replay watchdog,
1 GiB media tmpfs, 256 MiB fixture `/tmp`, 2 GiB memory/no additional swap,
two CPUs and 256 PIDs. It does **not** qualify the default 256 MiB media/replay
configuration, arbitrary bitrate, motion/30 fps, HDR, encoding, native/custom
unbounded volumes or device/hardware playback. The independent output watchdog
still applies (32 MiB per file, 384 MiB total); this workload does not bypass it.
Tmpfs allocation is not logical sparse-file length, process RSS or persistent
credential storage; see [BOUNDED_MEDIA.md](BOUNDED_MEDIA.md).

## Regression discovered

The pre-fix exact `9258fd82bd1dff31b1739e03ade1e7706ec0d699971ea45c9887980b0b8c8b61`
image accepted copy, but its output stopped after 10 frames/two media seconds:
FFmpeg 9 rejected the generated `index.m3u8` on live reload because the private
reader extension list contained only `ts`. The output's clean exit caused safe
processing failure and actual ingest cleanup around 6–7 seconds. A longer
monotonic source reproduced it, without EOF, OOM or cache-limit exhaustion.

The fix permits only `ts,m3u8` for gateway-owned local replay readers; protocol
whitelisting remains `file` only. Upstream HTTP(S), nested-resource egress/SSRF
validation, decoder eligibility, public v1 JSON and output/cache quotas are
unchanged. [Current FFmpeg HLS documentation](https://ffmpeg.org/ffmpeg-all.html#hls)
defines `allowed_extensions` as the comma-separated extensions the demuxer may
access and recommends correct whitelists instead of disabling extension checks.

Working-tree candidate evidence (not the final committed-image qualification):
101,676,604 retained replay bytes, 188,088,320 allocated media bytes,
10,000,060 demuxed H264 bytes in a sampled replay segment; successful actual
copy/output decode and release/reap with final media allocation zero.
The script emits the exact committed source revision and image digest for each
independent final run; retain that output with the handoff.
