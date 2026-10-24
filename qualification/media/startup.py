#!/usr/bin/env python3
"""Build paired cold/warm manifests for the actual backend and browser path."""

import argparse
import hashlib
import json
from pathlib import Path
import random

from corpus import digest, representatives, write_json


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--corpus", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--trials", type=int, default=20)
    p.add_argument("--count", type=int, default=40)
    p.add_argument("--ids")
    p.add_argument("--seconds", type=int, default=5)
    args = p.parse_args()
    assert args.trials > 0 and args.seconds >= 3
    manifest = args.corpus / "manifest.json"
    candidates = json.loads(manifest.read_text())["fixtures"]
    if args.ids:
        candidates = [i for i in candidates if i["id"] in args.ids.split(",")]
    selected = representatives(candidates, args.count)
    pairs = [(item, trial) for item in selected for trial in range(args.trials)]
    random.Random(20261023).shuffle(pairs)
    fixtures = []
    campaign = hashlib.sha256(digest(manifest).encode()).hexdigest()[:12]
    for item, trial in pairs:
        source = (
            item["file"]
            + ("?startup=" if "?" not in item["file"] else "&startup=")
            + f"{campaign}-{trial}"
        )
        for cache in ["cold", "warm"]:
            fixtures.append(
                dict(
                    item,
                    id=f"start-{item['id']}-{trial:02}-{cache}",
                    file=source,
                    validation_id=item["id"],
                    source_fixture=item["id"],
                    trial=trial,
                    cache=cache,
                    profile="backend-browser",
                    play_seconds=args.seconds,
                )
            )
    result = dict(
        version=1,
        ordered=True,
        fixtures=fixtures,
        source_manifest_sha256=digest(manifest),
        cache_definition="cold source identity; warm reuses that identity after normal output cleanup; backend/browser sessions are fresh; OS caches retained",
    )
    if args.output.exists():
        assert json.loads(args.output.read_text()) == result, (
            "startup trial manifest changed"
        )
    else:
        write_json(args.output, result)
    print(
        json.dumps(
            dict(
                fixtures=len(selected),
                planned_starts=len(fixtures),
                cold_per_fixture=args.trials,
                warm_per_fixture=args.trials,
            )
        )
    )


if __name__ == "__main__":
    main()
