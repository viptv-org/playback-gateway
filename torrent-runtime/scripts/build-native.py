#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Build candidate worker/JNI artifacts; never install or activate a consumer."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('platform', choices=['host', 'windows', 'android'])
parser.add_argument('--output', type=Path)
args = parser.parse_args()
out = (args.output or ROOT / 'target' / 'artifacts' / args.platform).resolve()
out.mkdir(parents=True, exist_ok=True)
go = os.environ.get('GO') or shutil.which('go')
if not go:
    raise SystemExit('Set GO to a Go 1.25+ executable')
env = dict(os.environ, GOTOOLCHAIN='local', GOMAXPROCS='2', CGO_ENABLED='1')

def run(command, child_env=env):
    subprocess.run(command, cwd=ROOT, env=child_env, check=True)

common = [go, 'build', '-p', '1', '-trimpath', '-buildvcs=false']
if args.platform == 'host':
    run(common + ['-o', str(out / 'torrent-worker'), './cmd/torrent-worker'])
    run(common + ['-buildmode=c-shared', '-o', str(out / 'libtorrent_runtime.so'), './cmd/torrent-native'])
elif args.platform == 'windows':
    win = dict(env, GOOS='windows', GOARCH='amd64',
               CC=os.environ.get('WINDOWS_CC', 'x86_64-w64-mingw32-gcc'),
               CXX=os.environ.get('WINDOWS_CXX', 'x86_64-w64-mingw32-g++'))
    run(common + ['-ldflags=-s -w', '-o', str(out / 'torrent-worker.exe'), './cmd/torrent-worker'], win)
else:
    ndk = os.environ.get('ANDROID_NDK_HOME')
    if not ndk:
        raise SystemExit('ANDROID_NDK_HOME is required')
    tools = Path(ndk) / 'toolchains/llvm/prebuilt/linux-x86_64/bin'
    for abi, arch, target in [('arm64-v8a', 'arm64', 'aarch64-linux-android'),
                              ('armeabi-v7a', 'arm', 'armv7a-linux-androideabi'),
                              ('x86_64', 'amd64', 'x86_64-linux-android')]:
        dest = out / 'jni' / abi
        dest.mkdir(parents=True, exist_ok=True)
        native = dict(env, GOOS='android', GOARCH=arch, GOARM='7',
                      CC=str(tools / (target + '24-clang')),
                      CXX=str(tools / (target + '24-clang++')))
        # anacrolix's Android interface dependency anet v0.0.3 links net.zoneCache.
        # Upstream requires this Go 1.23+ flag; keep it Android-only. ABI loading
        # and interface enumeration remain mandatory qualification on toolchain bumps.
        run(common + ['-buildmode=c-shared',
                      '-ldflags=-s -w -checklinkname=0 -extldflags=-Wl,-z,max-page-size=16384',
                      '-o', str(dest / 'libtorrent_runtime.so'), './cmd/torrent-native'], native)
        run([native['CC'], '-shared', '-fPIC', '-O2', '-Wl,-z,max-page-size=16384',
             '-Wl,-rpath,$ORIGIN', '-I', str(dest),
             str(ROOT / 'platform/android/bridge.c'), '-L', str(dest),
             '-ltorrent_runtime', '-o', str(dest / 'libtorrent_runtime_jni.so')], native)
        for library in dest.glob('*.so'):
            headers = subprocess.check_output([str(tools / 'llvm-readelf'), '-lW', str(library)], text=True)
            loads = [line.split() for line in headers.splitlines() if line.strip().startswith('LOAD')]
            if not loads or any(int(row[-1], 16) < 16384 for row in loads):
                raise SystemExit('16 KiB ELF alignment check failed: ' + library.name)
    classes = out / 'classes'
    classes.mkdir(exist_ok=True)
    run(['javac', '--release', '8', '-d', str(classes), str(ROOT / 'platform/android/NativeRuntime.java')])
    run(['jar', 'cf', str(out / 'classes.jar'), '-C', str(classes), '.'])

revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
dirty = bool(subprocess.check_output(['git', 'status', '--porcelain'], cwd=ROOT, text=True).strip())
files = {str(p.relative_to(out)): hashlib.sha256(p.read_bytes()).hexdigest()
         for p in sorted(out.rglob('*')) if p.is_file() and p.name != 'build.json'}
(out / 'build.json').write_text(json.dumps({'revision': revision, 'dirty': dirty,
    'platform': args.platform, 'qualification': 'candidate-only', 'files': files}, indent=2) + '\n')
print('Candidate artifacts and checksums:', out)
