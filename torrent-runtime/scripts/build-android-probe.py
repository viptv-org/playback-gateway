#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Package a standalone emulator ABI probe; never install a product client."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import zipfile

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--artifacts', type=Path, required=True)
parser.add_argument('--sdk', type=Path, default=os.environ.get('ANDROID_SDK_ROOT'))
parser.add_argument('--output', type=Path, default=root / 'target/android-probe')
parser.add_argument('--allow-dirty', action='store_true', help='Permit explicitly local prototype iteration')
args = parser.parse_args()
if args.sdk is None:
    raise SystemExit('--sdk or ANDROID_SDK_ROOT is required')
artifacts, out = args.artifacts.resolve(), args.output.resolve()
out.mkdir(parents=True, exist_ok=True)
manifest = json.loads((artifacts / 'build.json').read_text())
if manifest['platform'] != 'android' or (manifest['dirty'] and not args.allow_dirty):
    raise SystemExit('Probe requires clean, immutable Android artifacts')
for name, digest in manifest['files'].items():
    if hashlib.sha256((artifacts / name).read_bytes()).hexdigest() != digest:
        raise SystemExit('Artifact checksum mismatch')
tools = args.sdk / 'build-tools/36.0.0'
android = args.sdk / 'platforms/android-36/android.jar'
sources = root / 'qualification/android'
classes, dex = out / 'classes', out / 'dex'
classes.mkdir(exist_ok=True)
dex.mkdir(exist_ok=True)

def run(command):
    subprocess.run([str(value) for value in command], check=True)

run(['javac', '-source', '8', '-target', '8', '-classpath',
     str(android) + os.pathsep + str(artifacts / 'classes.jar'),
     '-d', classes, *sorted(sources.glob('*.java'))])
run([tools / 'd8', '--lib', android, '--min-api', '24', '--output', dex,
     *sorted(classes.rglob('*.class')), artifacts / 'classes.jar'])
unsigned, aligned, signed = out / 'unsigned.apk', out / 'aligned.apk', out / 'runtime-probe.apk'
run([tools / 'aapt2', 'link', '-o', unsigned, '--manifest', sources / 'AndroidManifest.xml', '-I', android])
with zipfile.ZipFile(unsigned, 'a') as apk:
    apk.write(dex / 'classes.dex', 'classes.dex')
    for library in sorted((artifacts / 'jni').glob('*/*.so')):
        apk.write(library, 'lib/' + str(library.relative_to(artifacts / 'jni')))
run([tools / 'zipalign', '-f', '-P', '16', '4', unsigned, aligned])
key = out / 'debug.keystore'
if not key.exists():
    run(['keytool', '-genkeypair', '-keystore', key, '-storepass', 'android',
         '-keypass', 'android', '-alias', 'qualification', '-keyalg', 'RSA',
         '-validity', '30', '-dname', 'CN=Local runtime qualification'])
run([tools / 'apksigner', 'sign', '--ks', key, '--ks-pass', 'pass:android',
     '--key-pass', 'pass:android', '--out', signed, aligned])
run([tools / 'apksigner', 'verify', signed])
run([tools / 'zipalign', '-c', '-P', '16', '4', signed])
(out / 'provenance.json').write_text(json.dumps({
    'runtime_revision': manifest['revision'],
    'runtime_dirty': manifest['dirty'],
    'apk_sha256': hashlib.sha256(signed.read_bytes()).hexdigest(),
    'purpose': 'standalone worker-process ABI qualification, not client playback',
}, indent=2) + '\n')
print('Standalone probe APK:', signed)
