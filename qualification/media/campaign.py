#!/usr/bin/env python3
"""Assemble a reproducible mixed campaign: broad coverage, long playback and soak."""

import argparse
import json
import os
from pathlib import Path
import random
import shutil
from corpus import write_json, digest


def assemble(root):
    corpus = root / "corpus"
    base = json.loads((corpus / "manifest.json").read_text())["fixtures"]
    real_root = root / "real-corpus"
    real = []
    for item in json.loads((real_root / "manifest.json").read_text())["fixtures"]:
        receipt = real_root / item["id"] / "receipt.json"
        if receipt.exists() and json.loads(receipt.read_text())["status"] == "valid":
            destination = corpus / item["id"]
            if not destination.exists():
                shutil.copytree(receipt.parent, destination, copy_function=os.link)
            real.append(item)
    extras = json.loads((corpus / "extras.json").read_text())["fixtures"]
    public = json.loads((corpus / "public-samples.json").read_text())["fixtures"]
    live = json.loads((corpus / "live-manifest.json").read_text())["fixtures"]
    for item in [*base, *real, *extras, *public, *live]:
        receipt = json.loads((corpus / item["id"] / "receipt.json").read_text())
        assert receipt["status"] == "valid", item["id"]
    short = [*base, *real, *public, *[x for x in extras if x["duration"] <= 120]]
    random.Random(20261023).shuffle(short)
    long = [dict(x, play_seconds=1800) for x in extras if x["duration"] > 120]
    for i in range(2):
        x = live[i]
        long.append(
            dict(x, id=f"long-{x['id']}", validation_id=x["id"], play_seconds=1800)
        )
    ordered = []
    live_serial = 0
    long_cursor = 0
    spacing = max(1, len(short) // len(long))
    for index, item in enumerate(short, 1):
        ordered.append(dict(item, play_seconds=30))
        if index % 32 == 0:
            source = live[live_serial % len(live)]
            ordered.append(
                dict(
                    source,
                    id=f"soak-{source['id']}-{live_serial:03}",
                    validation_id=source["id"],
                    play_seconds=60,
                )
            )
            live_serial += 1
        if index % spacing == 0 and long_cursor < len(long):
            ordered.append(long[long_cursor])
            long_cursor += 1
    ordered.extend(long[long_cursor:])
    assert len({x["id"] for x in ordered}) == len(ordered)
    result = dict(
        version=1,
        ordered=True,
        fixtures=ordered,
        main_manifest_sha256=digest(corpus / "manifest.json"),
        main_variants=len(base),
        real_motion_variants=len(real),
        public_samples=len(public),
        minimum_requested_playback_seconds=sum(x["play_seconds"] for x in ordered),
        soak_definition="one uninterrupted gateway process, mixed live/VOD playback, normal leases/releases and periodic normal-grace cleanup checkpoints",
    )
    write_json(corpus / "full-campaign.json", result)
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--root", type=Path, required=True)
    args = p.parse_args()
    result = assemble(args.root)
    print(json.dumps({k: v for k, v in result.items() if k != "fixtures"}))


if __name__ == "__main__":
    main()
