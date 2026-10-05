# Intel Quick Sync Container Profile

Quick Sync processing runs in the gateway, not in a browser. FFmpeg's oneVPL
dispatcher also needs a hardware runtime. Pre-Xe platforms such as Coffee Lake
use Intel Media SDK's `libmfxhw64.so.1`; installing `libvpl2` alone is insufficient.
See [Intel's runtime guidance](https://github.com/intel/libvpl/blob/main/INSTALL.md)
and the [Media SDK hardware matrix](https://github.com/Intel-Media-SDK/MediaSDK).

`Dockerfile.intel-legacy` is an optional amd64 overlay on the normal gateway
image. It adds Debian bookworm's pinned Media SDK runtime and Debian trixie's
full-feature Intel media driver. The runtime files and copyright notices are
copied without replacing the gateway image's libc, libva, FFmpeg or dispatcher.
The default generic image and its software fallback are unchanged.

The free-kernel Intel driver can initialize VAAPI while refusing VBR encoding
on older hardware. Do not qualify our bounded encoder with only a CQP smoke test.
The optional full-feature driver comes from Debian's signed non-free component;
its notices remain installed under `/usr/share/doc`. Intel Media SDK is a legacy
dependency, not a promise of support for newer GPU generations.

```sh
docker build -t playback-gateway:local .
docker build -f Dockerfile.intel-legacy \
  --build-arg BASE_IMAGE=playback-gateway:local \
  -t playback-gateway:intel-legacy .
bash scripts/check-intel-qsv.sh playback-gateway:intel-legacy
```

The hardware check is non-root, network-isolated, read-only and uses only
generated media in a bounded tmpfs. It requires a real render device and checks
Quick Sync VBR encoding, H.264 and HEVC Main10 hardware decoding, QSV scaling,
and software decoding of the resulting output. It does not qualify real sources,
HDR tone mapping, playback-control operations, or production deployment.

For the service, map the specific render node and add its numeric host group:
`--device /dev/dri/renderD128 --group-add "$(stat -c %g /dev/dri/renderD128)"`.
Set `QSV_DEVICE=/dev/dri/renderD128`; optionally set `VAAPI_DEVICE` to the same
node for separately qualified fallback/tone-mapping paths. Keep the non-root
user, read-only root, dropped capabilities, private data volume and API scopes.
Do not mount all devices, run privileged, or make the node world-writable.

After enabling the profile, inspect an actual conversion's selected encoder and
hardware decoder and decode its output. Mere `/ready` health or the presence of
`h264_qsv` in FFmpeg's codec list is not proof of hardware execution. Copy/remux
still avoids conversion. HDR presentation can retain CPU tone mapping even when
the encoder is Quick Sync; report that distinction and measured CPU use honestly.

When the preferred VAAPI HDR path fails, the bounded fallback ladder retains a
successfully probed Quick Sync encoder before trying software encoding. This
does not claim GPU HDR tone mapping: that retry can still decode and tone-map on
the CPU. Without a qualified Quick Sync device, the previous software fallback
remains unchanged. Conversion attempts retain the same source and reservation.

## HDR Output Metadata

GPU HDR conversion applies limited-range BT.709 metadata with `setparams` in the
frame filter chain and removes the source HDR side data there. It does not pass
encoder-side color options that can make FFmpeg request a software-format
conversion after VAAPI hardware frames. Software conversion retains its existing
encoder color options. Both paths must produce correctly tagged SDR, not merely
an output that opens.

The VPP scaler explicitly produces limited-range pixels before the metadata is
tagged, so a full-range input is not merely relabeled as limited-range output.

The VPP capability probe uses the same metadata filter as runtime conversion.
This correction does not add OpenCL support or qualify HLG/Dolby Vision paths.
Direct/copy delivery remains unchanged and must not lose original HDR metadata.
