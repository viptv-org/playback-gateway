#!/usr/bin/env python3
"""Generate continuous global A/V clock media before measuring live playback."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil

from corpus import BUDGET, canonical, digest, generate, recipes, write_json


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--root", type=Path, required=True)
    p.add_argument("--master", type=Path)
    p.add_argument("--seconds", type=int, default=30000)
    p.add_argument("--clock-offset", type=int, default=0)
    p.add_argument("--ffmpeg", default="ffmpeg")
    p.add_argument("--ffprobe", default="ffprobe")
    args = p.parse_args()
    assert 1 <= args.seconds <= 30000 and 0 <= args.clock_offset < 65536 - args.seconds
    args.root.mkdir(parents=True, exist_ok=True)
    assert shutil.disk_usage(args.root).free > min(
        BUDGET, 4 * 1024**3 + args.seconds * 1024**2
    ), "owned clock generation needs free-space reserve"
    item = dict(
        next(
            i
            for i in recipes()
            if i["container"] == "mp4" and i["video"] == "libx264" and i["variant"] == 0
        )
    )
    item.update(
        width=640,
        height=360,
        rate="30",
        channels=2,
        gop_seconds=2,
        duration=args.seconds,
        live_clock=True,
        timing_markers=True,
        timing_identity=dict(
            version=2,
            bits=16,
            frequency_base=1000,
            frequency_step=128,
            symbol_seconds=0.02,
            pulse_seconds=0.08,
            clock_offset=args.clock_offset,
        ),
    )
    if args.master:
        provenance = json.loads(args.master.with_suffix(".json").read_text())
        assert all(provenance.get(k) for k in ("url", "license", "attribution"))
        item.update(
            license=provenance["license"],
            attribution=provenance["attribution"],
            source_url=provenance["url"],
            source_sha256=digest(args.master),
        )
    item.pop("id")
    item.pop("file")
    item["id"] = "clock-" + hashlib.sha256(canonical(item)).hexdigest()[:16]
    item["file"] = item["id"] + "/media.mp4"
    path = args.root / "manifest.json"
    manifest = dict(version=1, fixtures=[item])
    if path.exists():
        assert json.loads(path.read_text()) == manifest, "clock source recipe changed"
    else:
        write_json(path, manifest)
    receipt = generate(item, args.root, args.ffmpeg, args.ffprobe, args.master)
    if receipt["status"] != "valid":
        raise SystemExit("invalid clock source; inspect retained receipt/logs")
    source = args.root / item["file"]
    write_json(
        source.with_suffix(".json"),
        dict(
            timing_identity=item["timing_identity"],
            license=item["license"],
            attribution=item.get("attribution"),
            source_sha256=digest(source),
        ),
    )
    print(
        json.dumps(dict(status="valid", source=str(source), seconds=args.seconds)),
        flush=True,
    )


if __name__ == "__main__":
    main()
