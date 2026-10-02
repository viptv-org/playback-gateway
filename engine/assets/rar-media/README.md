# Real media archive fixtures

These archives contain the original generated `clip.mp4` beside them. The
source consists of FFmpeg's testsrc2 image pattern and sine generator; it
contains no provider media or private information. The eight-second clip is
160×90 H.264/AAC at ten frames per second, with one-second keyframes.

Generated locally with FFmpeg and the temporary RAR 7.23 writer downloaded
from the upstream RAR distribution. The writer executable and its license
are not redistributed in this repository; only generated fixture bytes are.

```sh
ffmpeg -v error -nostdin -f lavfi -i testsrc2=size=160x90:rate=10 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 -t 8 \
  -c:v libx264 -threads 2 -preset ultrafast -g 10 -crf 27 \
  -c:a aac -b:a 32k -movflags +faststart clip.mp4
rar a -idq -ma5 -m0 -v48k -ep stored/clip.rar clip.mp4
rar a -idq -ma5 -m5 -md4m -ep compressed.rar clip.mp4
unrar t -idq stored/clip.part1.rar
unrar t -idq compressed.rar
```

The stored archive spans volumes and supports range reads across boundaries.
The compressed archive requires a complete, bounded, checksum-verified spool
before playback starts. Qualifying tests consume these committed fixtures;
RAR writer installation is not required to run host or container tests.
