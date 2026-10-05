#!/usr/bin/env bash
set -euo pipefail
image=${1:?Usage: check-intel-qsv.sh IMAGE [RENDER_DEVICE]}
device=${2:-/dev/dri/renderD128}
test -c "$device"
group=$(stat -c %g "$device")
docker run --rm --network none --read-only --cap-drop ALL \
  --security-opt no-new-privileges --pids-limit 256 \
  --device "$device" --group-add "$group" \
  --tmpfs /tmp:rw,noexec,nosuid,nodev,size=128m,uid=10001,gid=10001,mode=0700 \
  --env "QSV_TEST_DEVICE=$device" --entrypoint sh "$image" -ceu '
ffmpeg=/opt/ffmpeg/bin/ffmpeg
init="qsv=qualification,child_device=$QSV_TEST_DEVICE"
"$ffmpeg" -v error -init_hw_device "$init" -filter_hw_device qualification \
  -f lavfi -i color=size=128x128:rate=30 \
  -vf format=nv12,hwupload=extra_hw_frames=32 -c:v h264_qsv \
  -preset veryfast -profile:v main -level:v 4.0 -b:v 4000k \
  -maxrate 5000k -bufsize 10000k -look_ahead 0 -async_depth 4 -bf 0 \
  -frames:v 2 -f null -
printf "PASS Quick Sync initialization and bounded VBR encode\n"
for input in h264 hevc10; do
  if [ "$input" = h264 ]; then
    codec=libx264; decoder=h264_qsv; pixel=yuv420p
    "$ffmpeg" -v error -f lavfi -i testsrc2=size=640x360:rate=30 \
      -pix_fmt "$pixel" -c:v "$codec" -preset ultrafast -frames:v 30 \
      -threads 2 -y "/tmp/$input.mkv"
  else
    decoder=hevc_qsv
    "$ffmpeg" -v error -init_hw_device "$init" -filter_hw_device qualification \
      -f lavfi -i testsrc2=size=640x360:rate=30 \
      -vf format=p010le,hwupload=extra_hw_frames=32 \
      -c:v hevc_qsv -profile:v main10 -preset veryfast -frames:v 30 \
      -y "/tmp/$input.mkv"
  fi
  "$ffmpeg" -v error -init_hw_device "$init" -filter_hw_device qualification \
    -hwaccel qsv -hwaccel_output_format qsv -c:v "$decoder" \
    -i "/tmp/$input.mkv" -vf vpp_qsv=w=320:h=180:format=nv12 \
    -c:v h264_qsv -preset veryfast -b:v 1000k -look_ahead 0 -bf 0 \
    -frames:v 30 -y "/tmp/$input-qsv.mp4"
  "$ffmpeg" -v error -i "/tmp/$input-qsv.mp4" -frames:v 30 -f null -
  printf "PASS %s hardware decode, QSV scale/encode, software decode\n" "$input"
done
'
