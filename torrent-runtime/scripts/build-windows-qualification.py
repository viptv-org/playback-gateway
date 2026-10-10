#!/usr/bin/env python3
"""Build a Windows race-test capsule from a committed runtime, without shipping source."""
# SPDX-License-Identifier: GPL-2.0-only
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import zipfile

owner = Path(__file__).resolve().parents[2]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('output', type=Path, help='Fresh artifact directory')
parser.add_argument('--revision', required=True, help='Full immutable gateway commit')
args = parser.parse_args()
if not re.fullmatch('[a-f0-9]{40}', args.revision):
    raise SystemExit('A full immutable revision is required')
go = os.environ.get('GO') or shutil.which('go')
if not go:
    raise SystemExit('Set GO to the qualification toolchain')
reference = owner / 'vendor/torrent-runtime'
notices = json.loads((reference / 'lock.json').read_text())
if notices['revision'] != args.revision:
    raise SystemExit('Dependency notices must match the runtime revision')
for name, digest in notices['files'].items():
    path = reference / name
    if path.is_symlink() or hashlib.sha256(path.read_bytes()).hexdigest() != digest:
        raise SystemExit('Runtime dependency inventory failed integrity')
args.output.mkdir(parents=True, exist_ok=False)
with tempfile.TemporaryDirectory(prefix='torrent-windows-qualification-') as temporary:
    root = Path(temporary)
    archive = root / 'committed-source.tar'
    with archive.open('wb') as file:
        subprocess.run(['git', 'archive', args.revision, 'torrent-runtime',
                        'engine/assets', 'LICENSE', 'PROVENANCE.md'],
                       cwd=owner, stdout=file, check=True)
    source = root / 'source'
    with tarfile.open(archive) as file:
        file.extractall(source, filter='data')
    package = root / 'package'
    (package / 'torrent-runtime').mkdir(parents=True)
    executable = package / 'torrent-runtime/torrent-runtime.test.exe'
    native = dict(os.environ, GOTOOLCHAIN='local', GOMAXPROCS='2', GOOS='windows',
                  GOARCH='amd64', CGO_ENABLED='1',
                  CC=os.environ.get('WINDOWS_CC', 'x86_64-w64-mingw32-gcc'),
                  CXX=os.environ.get('WINDOWS_CXX', 'x86_64-w64-mingw32-g++'))
    subprocess.run([go, 'test', '-c', '-race', '-p', '1', '-trimpath',
                    '-buildvcs=false', '-ldflags=-s -w -linkmode=external -extldflags=-static',
                    '-o', str(executable)], cwd=source / 'torrent-runtime', env=native, check=True)
    imports = subprocess.check_output([
        os.environ.get('WINDOWS_OBJDUMP', 'x86_64-w64-mingw32-objdump'),
        '-p', str(executable)], text=True)
    system = {'kernel32.dll', 'ws2_32.dll', 'advapi32.dll', 'bcrypt.dll',
              'ntdll.dll', 'userenv.dll', 'secur32.dll', 'msvcrt.dll', 'ucrtbase.dll'}
    for name in re.findall(r'DLL Name:\s*(\S+)', imports):
        if name.lower() not in system and not name.lower().startswith(('api-ms-win-', 'ext-ms-win-')):
            raise SystemExit('Unpackaged Windows test dependency: ' + name)
    shutil.copytree(source / 'engine/assets', package / 'engine/assets')
    shutil.copytree(reference / 'licenses', package / 'licenses')
    for name in ('LICENSE', 'PROVENANCE.md'):
        shutil.copyfile(source / name, package / name)
    goroot = Path(subprocess.check_output([go, 'env', 'GOROOT'], text=True).strip())
    shutil.copyfile(goroot / 'LICENSE', package / 'licenses/Go-LICENSE')
    for name in ('go.mod', 'go.sum'):
        shutil.copyfile(source / 'torrent-runtime' / name, package / 'torrent-runtime' / name)
    files = {path.relative_to(package).as_posix(): hashlib.sha256(path.read_bytes()).hexdigest()
             for path in sorted(package.rglob('*')) if path.is_file()}
    bundle = args.output / 'windows-tests.zip'
    with zipfile.ZipFile(bundle, 'w', compression=zipfile.ZIP_DEFLATED, compresslevel=9) as output:
        for name in files:
            output.write(package / name, name)
    manifest = {'repository': 'viptv-org/playback-gateway', 'revision': args.revision,
                'platform': 'windows-amd64', 'race': True, 'dirty': False,
                'go_version': subprocess.check_output([go, 'version'], text=True).strip(),
                'files': {'windows-tests.zip': hashlib.sha256(bundle.read_bytes()).hexdigest()},
                'contents': files}
    (args.output / 'lock.json').write_text(json.dumps(manifest, indent=2) + '\n')
print('Windows qualification capsule built from', args.revision)
