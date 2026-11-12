from pathlib import Path
import json
import os
import stat

REVISION = 'e283ca41df3a84b0e87fb1f5cb9b21580a286b09'
MARKER = '.viptv-subtitle-probe.json'


def owned_directory(path, owner_uid):
    if any(character in str(path) for character in ',\n\r'):
        raise ValueError('--directory must not contain commas or newlines')
    if path.is_symlink():
        raise ValueError('--directory must not be a symlink')
    root = path.resolve(strict=True)
    if any(character in str(root) for character in ',\n\r'):
        raise ValueError('--directory must not resolve to a comma or newline path')
    info = root.stat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != owner_uid or info.st_mode & 0o022:
        raise ValueError('--directory must be owned by the selected uid and not writable by other users')
    return root


def claim_empty_directory(path, owner_uid):
    root = owned_directory(path, owner_uid)
    if any(root.iterdir()):
        raise ValueError('probe build requires an empty owned scratch directory')
    marker = dict(kind='viptv-subtitle-probe', schema=1, owner_uid=owner_uid,
                  source_revision=REVISION, build_complete=False)
    descriptor = os.open(root / MARKER, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, 'w') as output:
        json.dump(marker, output)
    return root, marker
