#!/usr/bin/env python3
"""Record media binaries, runtime libraries and hardware without dumping env/secrets."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import time


def sha(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def command(argv):
    p = subprocess.run(argv, capture_output=True, text=True, timeout=20)
    return dict(argv=argv, exit_code=p.returncode, stdout=p.stdout, stderr=p.stderr)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--output", type=Path, required=True)
    args = p.parse_args()
    data = dict(
        recorded_at=time.time(),
        kernel=platform.uname()._asdict(),
        binaries={},
        devices={},
        libraries={},
    )
    for name, env in (("ffmpeg", "LAB_FFMPEG"), ("ffprobe", "LAB_FFPROBE")):
        path = Path(os.environ.get(env, "/opt/ffmpeg/bin/" + name))
        data["binaries"][name] = dict(
            path=str(path), sha256=sha(path), version=command([str(path), "-version"])
        )
    for device in Path("/sys/class/drm").glob("renderD*"):
        fields = {}
        for name in ("vendor", "device", "revision", "uevent"):
            path = device / "device" / name
            if path.exists():
                fields[name] = path.read_text()
        fields["driver"] = str((device / "device/driver").resolve())
        data["devices"][device.name] = fields
    for root in (Path("/usr/lib/x86_64-linux-gnu"), Path("/opt/intel/mediasdk/lib64")):
        for pattern in (
            "dri/*_drv_video.so",
            "libmfx*.so*",
            "libvpl.so*",
            "libva*.so*",
            "libdrm*.so*",
        ):
            for path in root.glob(pattern):
                if path.is_file() and not path.is_symlink():
                    data["libraries"][str(path)] = sha(path)
    data["cpu"] = command(["lscpu", "--json"])
    data["packages"] = command(
        [
            "dpkg-query",
            "-W",
            "-f=${Package} ${Version}\n",
            "*intel*",
            "*mesa*",
            "*libva*",
            "*libmfx*",
            "*libvpl*",
            "*libdrm*",
        ]
    )
    args.output.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
