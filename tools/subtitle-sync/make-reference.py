from pathlib import Path
import argparse
import json

parser = argparse.ArgumentParser(description='Create a circular mechanism reference from owned fixture VAD; not an accuracy corpus.')
parser.add_argument('--observations', type=Path, required=True)
parser.add_argument('--output', type=Path, required=True)
args = parser.parse_args()
if args.observations.stat().st_size > 2 * 1024 * 1024:
    parser.error('observation input exceeds bound')
observations = json.loads(args.observations.read_text())
if len(observations) > 6000:
    parser.error('observation count exceeds bound')
intervals = []
for frame in observations:
    if frame['activity'] != 'speech':
        continue
    start, end = frame['startUs'], frame['endUs']
    if intervals and intervals[-1][1] == start:
        intervals[-1][1] = end
    else:
        intervals.append([start, end])
def clock(value):
    millis = value // 1000
    return '%02d:%02d:%02d.%03d' % (millis // 3600000, millis // 60000 % 60, millis // 1000 % 60, millis % 1000)
blocks = ['WEBVTT']
for start, end in intervals:
    start, end = max(0, start - 2_000_000), end - 2_000_000
    if end > start:
        blocks.append('%s --> %s\nOwned mechanism fixture' % (clock(start), clock(end)))
with args.output.open('x') as output:
    output.write('\n\n'.join(blocks) + '\n')
