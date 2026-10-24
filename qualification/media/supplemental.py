#!/usr/bin/env python3
"""Assemble browser startup, caption, HDR and continuous-backend workloads."""

import argparse
import json
from pathlib import Path
import subprocess
import sys

from corpus import write_json


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--corpus", type=Path, required=True)
    args = p.parse_args()
    root = args.corpus
    base = json.loads((root / "manifest.json").read_text())["fixtures"]
    extra = json.loads((root / "extras.json").read_text())["fixtures"]
    clock = json.loads((root / "clock-live-manifest.json").read_text())["fixtures"]
    captions = [
        dict(
            next(
                i
                for i in base
                if i["container"] == "mkv" and i["video"] == codec and i["subtitles"]
            ),
            play_seconds=30,
        )
        for codec in ("libx264", "libx265")
    ]
    captions.append(dict(next(i for i in extra if i["subtitles"]), play_seconds=1800))
    quality = [dict(i, play_seconds=30) for i in base if i["transfer"] != "bt709"]
    soak = [
        dict(
            i, play_seconds=60, **({"player_profile": "mse"} if i["subtitles"] else {})
        )
        for i in extra
        if i["duration"] > 120
    ]
    soak += [dict(i, play_seconds=60) for i in clock]
    for name, fixtures in [
        ("caption-manifest.json", captions),
        ("quality-manifest.json", quality),
        ("backend-soak-manifest.json", soak),
    ]:
        assert all(
            json.loads(
                (root / i.get("validation_id", i["id"]) / "receipt.json").read_text()
            )["status"]
            == "valid"
            for i in fixtures
        )
        write_json(root / name, dict(version=1, fixtures=fixtures))
    subprocess.run(
        [
            sys.executable,
            str(Path(__file__).with_name("startup.py")),
            "--corpus",
            str(root),
            "--output",
            str(root / "browser-startup.json"),
        ],
        check=True,
    )
    print(
        json.dumps(
            dict(
                caption_cases=len(captions),
                hdr_cases=len(quality),
                soak_sources=len(soak),
            )
        )
    )


if __name__ == "__main__":
    main()
