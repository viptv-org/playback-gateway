#!/usr/bin/env python3
"""Qualify a file larger than an owned Linux tmpfs; never mount production paths."""
import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("output", type=Path, help="Fresh directory for the controlled test log")
parser.add_argument("--go", default=os.environ.get("GO", "go"))
args = parser.parse_args()
if sys.platform != "linux":
    raise SystemExit("This controlled filesystem fixture requires Linux")
os.umask(0o077)
args.output.mkdir(parents=True, exist_ok=False)
root = Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix="viptv-owned-small-filesystem-") as directory:
    mount = Path(directory) / "cache"
    mount.mkdir()
    mounted = False
    try:
        subprocess.run([
            "sudo", "-n", "mount", "-t", "tmpfs", "-o",
            f"size=600m,mode=0700,uid={os.getuid()},gid={os.getgid()}",
            "viptv-owned-cache-qualification", str(mount),
        ], check=True)
        mounted = True
        with (args.output / "test.log").open("w") as log:
            result = subprocess.run([
                args.go, "test", "-race", "-p", "1", "-run",
                "^TestFileLargerThanCacheFilesystemStreamsAndReusesVerifiedPieces$", "-v",
            ], cwd=root, env=dict(os.environ, GOMAXPROCS="2", TORRENT_SMALL_FILESYSTEM=str(mount)),
                stdout=log, stderr=log, timeout=150)
        print(f"Owned filesystem qualification exit: {result.returncode}")
        print(f"Controlled test log: {args.output / 'test.log'}")
        raise SystemExit(result.returncode)
    finally:
        if mounted:
            subprocess.run(["sudo", "-n", "umount", str(mount)], check=True)
