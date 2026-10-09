#!/usr/bin/env python3
"""Verify the immutable Linux worker and redistributed dependency notices."""
import hashlib
import json
from pathlib import Path
root = Path(__file__).resolve().parents[1]
folder = root / 'vendor/torrent-runtime'
lock = json.loads((folder / 'lock.json').read_text())
assert lock['revision'] == (root / 'TORRENT_RUNTIME_REF').read_text().strip()
for name, digest in lock['files'].items():
    relative = Path(name)
    assert not relative.is_absolute() and '..' not in relative.parts
    path = folder / relative
    assert not path.is_symlink() and hashlib.sha256(path.read_bytes()).hexdigest() == digest
assert (folder / 'torrent-worker').read_bytes()[:4] == b'\x7fELF'
print('Pinned Linux shared runtime and notices verified')
