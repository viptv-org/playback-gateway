#!/usr/bin/env python3
"""Own and remove private Go fixtures; exercise real FFmpeg gateway torrent output."""
import os
from pathlib import Path
import subprocess
import tempfile
root = Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix='gateway-torrent-qualification-') as directory:
    work = Path(directory)
    executable = work / 'fixture.test'
    env = dict(os.environ, GOMAXPROCS='2')
    subprocess.run([env.get('GO','go'), 'test', '-c', '-p','1','-o',str(executable)], cwd=root/'torrent-runtime', env=env, check=True)
    worker = work / 'worker'
    seeder = work / 'seeder'
    # Paths come from tempfile, not private source inputs or credentials.
    worker.write_text(f"#!/bin/sh\nexec env TORRENT_OWNED_WORKER=1 '{executable}' -test.run '^TestGatewayQualificationWorker$'\n")
    seeder.write_text(f"#!/bin/sh\nexec '{executable}' -test.run '^TestGatewayQualificationSeeder$'\n")
    for path in [worker,seeder]: path.chmod(0o700)
    env.update(TORRENT_WORKER=str(worker), TORRENT_TEST_SEEDER=str(seeder),
               PLAYBACK_TEST_FFMPEG=env.get('FFMPEG_PATH','ffmpeg'), PLAYBACK_TEST_FFPROBE=env.get('FFPROBE_PATH','ffprobe'))
    subprocess.run(['cargo','test','-p','playback-gateway','torrent_tests::','--','--ignored','--test-threads=1'], cwd=root, env=env, check=True)
