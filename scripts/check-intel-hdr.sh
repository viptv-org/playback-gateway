#!/usr/bin/env bash
set -euo pipefail
image=${1:?Usage: check-intel-hdr.sh IMAGE [RENDER_DEVICE]}
device=${2:-/dev/dri/renderD128}
root=$(cd "$(dirname "$0")/.." && pwd)
test -c "$device"
group=$(stat -c %g "$device")
docker run --rm --network none --read-only --cap-drop ALL \
  --security-opt no-new-privileges --pids-limit 256 \
  --device "$device" --group-add "$group" \
  --mount "type=bind,src=$root/engine/assets/hdr10-probe.hevc,dst=/sample.hevc,readonly" \
  --tmpfs /tmp:rw,noexec,nosuid,nodev,size=64m,uid=10001,gid=10001,mode=0700 \
  --env "HDR_TEST_DEVICE=$device" --entrypoint sh "$image" -ceu '
ffmpeg=/opt/ffmpeg/bin/ffmpeg
ffprobe=/opt/ffmpeg/bin/ffprobe
"$ffmpeg" -v error -init_hw_device "vaapi=qualification:$HDR_TEST_DEVICE" \
  -filter_hw_device qualification -hwaccel vaapi -hwaccel_output_format vaapi \
  -i /sample.hevc -vf "procamp_vaapi=b=16,tonemap_vaapi=format=nv12:t=bt709:m=bt709:p=bt709,scale_vaapi=w=128:h=128:format=nv12:out_range=tv,setparams=range=limited:color_primaries=bt709:color_trc=bt709:colorspace=bt709,sidedata=mode=delete" \
  -c:v h264_vaapi -profile:v main -level:v 4 -rc_mode VBR -b:v 4000k \
  -maxrate 5000k -bufsize 10000k -bf 0 -frames:v 2 -y /tmp/sdr.mp4
"$ffmpeg" -v error -i /tmp/sdr.mp4 -frames:v 2 -f null -
actual=$("$ffprobe" -v error -select_streams v:0 \
  -show_entries stream=codec_name,pix_fmt,color_range,color_space,color_transfer,color_primaries \
  -of csv=p=0 /tmp/sdr.mp4)
test "$actual" = "h264,yuv420p,tv,bt709,bt709,bt709"
metadata=$("$ffprobe" -v error -show_entries stream_side_data=side_data_type -of csv=p=0 /tmp/sdr.mp4)
case "$metadata" in *Mastering*|*Content*light*|*DOVI*) exit 1 ;; esac
printf "PASS GPU HDR decode/tone-map/encode, SDR color metadata and decoded output\n"
'
