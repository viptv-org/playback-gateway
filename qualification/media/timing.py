#!/usr/bin/env python3
"""Create uniquely identified A/V markers for absolute browser timing checks."""

import argparse
import hashlib
import json
from pathlib import Path

from corpus import EXT, canonical, generate, recipes, write_json


def timing_recipes():
    profiles = [
        ("mp4", "libx264", "aac"),
        ("mkv", "libx265", "flac"),
        ("mkv", "libx264", "truehd"),
        ("hls-fmp4", "libx265", "aac"),
        ("ts", "mpeg2video", "mp2"),
        ("mkv", "libsvtav1", "libopus"),
    ]
    all_recipes = recipes()
    result = []
    for profile in profiles:
        item = dict(
            next(
                i
                for i in all_recipes
                if (i["container"], i["video"], i["audio"]) == profile
            )
        )
        item.update(
            width=640,
            height=360,
            rate="30",
            channels=2,
            pattern=0,
            gop_seconds=2,
            timing_markers=True,
            timing_identity={
                "version": 1,
                "bits": 6,
                "frequency_base": 1000,
                "frequency_step": 128,
                "pulse_seconds": 0.08,
            },
        )
        item.pop("id")
        item.pop("file")
        item["id"] = "timing-" + hashlib.sha256(canonical(item)).hexdigest()[:16]
        item["file"] = (
            item["id"] + "/media." + EXT.get(item["container"], item["container"])
        )
        result.append(item)
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--root", type=Path, required=True)
    p.add_argument("--ffmpeg", default="ffmpeg")
    p.add_argument("--ffprobe", default="ffprobe")
    args = p.parse_args()
    args.root.mkdir(parents=True, exist_ok=True)
    fixtures = timing_recipes()
    manifest = args.root / "manifest.json"
    if manifest.exists():
        assert json.loads(manifest.read_text())["fixtures"] == fixtures
    else:
        write_json(manifest, dict(version=1, fixtures=fixtures))
    for fixture in fixtures:
        result = generate(fixture, args.root, args.ffmpeg, args.ffprobe, None)
        print(
            json.dumps(dict(fixture=fixture["id"], status=result["status"])), flush=True
        )
        if result["status"] != "valid":
            raise SystemExit("timing fixture invalid; inspect generation receipt")


if __name__ == "__main__":
    main()
