from pathlib import Path
import argparse
import hashlib
import io
import json
import os
import subprocess
import tarfile
import urllib.request
from scratch import MARKER, REVISION, claim_empty_directory

parser = argparse.ArgumentParser(description='Build a Linux-only native WebRTC VAD research probe in an owned scratch directory.')
parser.add_argument('--directory', type=Path, required=True)
args = parser.parse_args()
try:
    root, marker = claim_empty_directory(args.directory, os.getuid())
except (OSError, ValueError) as error:
    parser.error(str(error))
archive = urllib.request.urlopen('https://codeload.github.com/wiseman/py-webrtcvad/tar.gz/' + REVISION, timeout=20).read()
with tarfile.open(fileobj=io.BytesIO(archive), mode='r:gz') as tar:
    for entry in tar.getmembers():
        parts = Path(entry.name).parts
        if len(parts) < 2 or '..' in parts or not entry.isfile():
            continue
        if parts[1] == 'LICENSE' or parts[1:3] == ('cbits', 'webrtc'):
            target = root.joinpath(*parts[1:])
            target.parent.mkdir(parents=True, exist_ok=True)
            with target.open('xb') as output:
                output.write(tar.extractfile(entry).read())
with (root / 'vad.c').open('xb') as output:
    output.write((Path(__file__).parent / 'vad.c').read_bytes())
sources = sorted((root / 'cbits/webrtc/common_audio').rglob('*.c')) + [root / 'vad.c']
objects = []
for index, source in enumerate(sources):
    target = root / ('probe-%d.o' % index)
    subprocess.run(['cc', '-O2', '-DNDEBUG', '-DWEBRTC_POSIX', '-I' + str(root / 'cbits'), '-c', str(source), '-o', str(target)], check=True)
    objects.append(str(target))
target = root / 'checks.o'
subprocess.run(['g++', '-O2', '-DNDEBUG', '-DWEBRTC_POSIX', '-I' + str(root / 'cbits'), '-c', str(root / 'cbits/webrtc/rtc_base/checks.cc'), '-o', str(target)], check=True)
subprocess.run(['g++', '-static', '-o', str(root / 'subtitle-vad'), *objects, str(target), '-lm', '-pthread'], check=True)
(root / 'subtitle-vad').chmod(0o755)
marker.update(build_complete=True, probe_sha256=hashlib.sha256((root / 'subtitle-vad').read_bytes()).hexdigest())
(root / MARKER).write_text(json.dumps(marker) + '\n')
