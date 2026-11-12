from pathlib import Path
import argparse
import array
import json
import math
import random
import subprocess
import wave

parser = argparse.ArgumentParser()
parser.add_argument('--directory', type=Path, required=True)
parser.add_argument('--ffmpeg', type=Path, required=True)
args = parser.parse_args()
args.directory.mkdir(mode=0o700, exist_ok=False)
rate = 16000
randomizer = random.Random(54)
intervals = []
position = 0.5
while position < 118:
    end = min(119, position + randomizer.uniform(0.5, 2.9))
    intervals.append((position, end))
    position = end + randomizer.uniform(0.4, 2.1)
audio = array.array('h')
index = 0
phase = 0.0
for sample in range(120 * rate):
    time = sample / rate
    while index < len(intervals) and time >= intervals[index][1]:
        index += 1
    voiced = index < len(intervals) and intervals[index][0] <= time < intervals[index][1]
    phase += 2 * math.pi * (130 + 35 * math.sin(time * 5.3) + 15 * math.sin(time * 2.1)) / rate
    signal = (math.sin(phase) + 0.5 * math.sin(phase * 2) + 0.3 * math.sin(phase * 5)) * (0.5 + 0.3 * math.sin(time * 18))
    audio.append(round(11000 * signal) if voiced else 0)
with wave.open(str(args.directory / 'synthetic.wav'), 'wb') as output:
    output.setnchannels(1)
    output.setsampwidth(2)
    output.setframerate(rate)
    output.writeframes(audio.tobytes())
subprocess.run([str(args.ffmpeg), '-v', 'error', '-nostdin', '-threads', '1', '-filter_threads', '1', '-f', 'lavfi', '-i', 'color=black:size=160x90:rate=10', '-i', str(args.directory / 'synthetic.wav'), '-t', '120', '-map', '0:v:0', '-map', '1:a:0', '-c:v', 'libx264', '-preset', 'ultrafast', '-g', '20', '-bf', '0', '-c:a', 'aac', '-b:a', '64k', '-hls_time', '2', '-hls_list_size', '0', '-hls_flags', 'temp_file', '-hls_segment_filename', str(args.directory / 'segment-%09d.ts'), str(args.directory / 'index.m3u8')], check=True, timeout=30)
(args.directory / 'fixture.json').write_text(json.dumps({'kind': 'owned synthetic voiced harmonic signal; not dialogue or alignment accuracy evidence', 'seconds': 120, 'rate': rate, 'voiced_intervals': intervals}) + '\n')
