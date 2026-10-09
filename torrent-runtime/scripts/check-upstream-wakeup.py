#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Exercise the real upstream writer without modifying the module cache."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--version', help='Optional exact comparison version, e.g. v1.61.0')
args = parser.parse_args()
go = os.environ.get('GO') or shutil.which('go')
if not go:
    raise SystemExit('Set GO to a Go 1.25+ executable')
env = dict(os.environ, GOTOOLCHAIN='local', GOMAXPROCS='2', GOWORK='off')
module = 'github.com/anacrolix/torrent'
command = [go, 'mod', 'download', '-json', module + '@' + args.version] if args.version else [go, 'list', '-m', '-json', module]
info = json.loads(subprocess.check_output(command, cwd=root, env=env, text=True))
with tempfile.TemporaryDirectory(prefix='torrent-wakeup-') as scratch:
    target = Path(scratch) / 'upstream'
    shutil.copytree(info['Dir'], target)
    target.chmod(0o700)
    shutil.copyfile(root / 'qualification/peer_writer_wakeup_test.go.txt', target / 'peer_writer_wakeup_test.go')
    print('Testing upstream writer:', info['Version'], flush=True)
    subprocess.run([go, 'test', '-race', '-p', '1', '-run',
        '^TestMessageWriterDoesNotLoseRequestWakeupBetweenFillAndWait$',
        '-count', '10', '-timeout', '30s'], cwd=target, env=env, check=True)
