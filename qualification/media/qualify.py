#!/usr/bin/env python3
"""Fork an immutable corpus, auditing cached recipes before reusing payloads.

Only media payloads are hard linked. Receipts, manifests and logs are copied so
repairing a recipe cannot rewrite evidence from an already running campaign.
Missing or invalid recipes remain explicit in audit.json until regenerated.
"""

import argparse
import json
import os
from pathlib import Path
import shutil

from corpus import BUDGET, VERSION, digest, recipes, validate_metadata, write_json


def clone(source, destination):
    def copy(src, dst):
        if Path(src).suffix in (".json", ".log", ".tmp"):
            return shutil.copy2(src, dst)
        os.link(src, dst)
        return dst

    if not destination.exists():
        shutil.copytree(source, destination, copy_function=copy)


def fork(source, destination):
    assert source.resolve() != destination.resolve(), "use a new corpus root"
    destination.mkdir(parents=True, exist_ok=True, mode=0o700)
    old_manifest = source / "manifest.json"
    old = json.loads(old_manifest.read_text())["fixtures"]
    current = recipes()
    by_id = {i["id"]: i for i in current}
    rejected, reused, missing = [], [], []
    for item in old:
        receipt_path = source / item["id"] / "receipt.json"
        try:
            receipt = json.loads(receipt_path.read_text())
            assert receipt["status"] == "valid", "source receipt is invalid"
            validate_metadata(item, receipt["probe"])
            assert item == by_id.get(item["id"]), "recipe superseded"
            assert all(
                digest(receipt_path.parent / name) == checksum
                for name, checksum in receipt["files"].items()
            ), "payload checksum mismatch"
        except (AssertionError, OSError, ValueError, KeyError) as error:
            rejected.append(dict(id=item["id"], reason=str(error)))
            continue
        clone(receipt_path.parent, destination / item["id"])
        reused.append(item["id"])
    generated = []
    for item in current:
        if item["id"] in reused:
            continue
        receipt_path = destination / item["id"] / "receipt.json"
        try:
            receipt = json.loads(receipt_path.read_text())
            assert receipt["status"] == "valid"
            validate_metadata(item, receipt["probe"])
            assert all(
                digest(receipt_path.parent / name) == checksum
                for name, checksum in receipt["files"].items()
            )
        except (AssertionError, OSError, ValueError, KeyError):
            missing.append(item["id"])
        else:
            generated.append(item["id"])
    # Supplemental fixture directories and live packages are immutable too.
    # Keep obsolete long HDR files solely in the original evidence tree.
    old_ids = {i["id"] for i in old}
    for path in sorted(source.iterdir()):
        if path.is_dir() and path.name not in old_ids and path.name != "long-hdr-1080":
            clone(path, destination / path.name)
        elif (
            path.is_file()
            and path.suffix == ".json"
            and path.name
            not in {
                "manifest.json",
                "audit.json",
                "full-campaign.json",
                "browser-startup.json",
                "quality-manifest.json",
                "caption-manifest.json",
                "backend-soak-manifest.json",
                "extras.json",
            }
        ):
            target = destination / path.name
            if not target.exists():
                shutil.copy2(path, target)
    manifest = dict(
        version=VERSION, fixtures=current, budget_bytes=BUDGET, master_sha256=None
    )
    target = destination / "manifest.json"
    if target.exists():
        assert json.loads(target.read_text()) == manifest, (
            "destination manifest changed"
        )
    else:
        write_json(target, manifest)
    audit = dict(
        source_manifest_sha256=digest(old_manifest),
        destination_manifest_sha256=digest(target),
        reused_variants=len(reused),
        regenerated_variants=len(generated),
        excluded_from_playback_pass_counts=rejected,
        excluded_derived_fixtures=["long-hdr-1080"],
        generation_required=missing,
        complete=not missing,
        note="Excluded source fixtures remain untouched in the original evidence tree. Replacements require generation, metadata inspection and full decode.",
    )
    write_json(destination / "audit.json", audit)
    return audit


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--source", type=Path, required=True)
    p.add_argument("--destination", type=Path, required=True)
    args = p.parse_args()
    result = fork(args.source, args.destination)
    print(json.dumps({k: v for k, v in result.items() if k != "generation_required"}))


if __name__ == "__main__":
    main()
