#!/usr/bin/env python3
"""Decode diverse fixtures under normal RTT and twice-bitrate origin profiles."""

import argparse
import json
from pathlib import Path
import subprocess
import time

from corpus import digest, representatives, write_json
from network import configure


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--run", type=Path, default=Path("/run/lab"))
    p.add_argument("--corpus", type=Path, default=Path("/corpus"))
    p.add_argument("--output", type=Path, required=True)
    args = p.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    fixtures = representatives(
        json.loads((args.corpus / "manifest.json").read_text())["fixtures"], 40
    )
    frozen = dict(
        manifest_sha256=digest(args.corpus / "manifest.json"),
        fixtures=[f["id"] for f in fixtures],
        profiles=["rtt50", "bandwidth2x"],
        gateway=json.loads((args.run / "provenance.json").read_text()),
    )
    path = args.output / "campaign.json"
    if path.exists():
        assert json.loads(path.read_text()) == frozen, "network campaign changed"
    else:
        write_json(path, frozen)
    results = []
    try:
        for profile in frozen["profiles"]:
            for fixture in fixtures:
                directory = args.output / (profile + "-" + fixture["id"])
                directory.mkdir(exist_ok=True)
                receipt = json.loads(
                    (args.corpus / fixture["id"] / "receipt.json").read_text()
                )
                assert receipt["status"] == "valid"
                size = sum(
                    (args.corpus / fixture["id"] / name).stat().st_size
                    for name in receipt["files"]
                )
                bitrate = max(1, int(size * 8 / fixture["duration"]))
                configure(profile, bitrate)
                started = time.monotonic()
                with (directory / "run.log").open("w") as log:
                    process = subprocess.run(
                        [
                            "python3",
                            str(Path(__file__).with_name("measure.py")),
                            "--run",
                            str(args.run),
                            "--corpus",
                            str(args.corpus),
                            "--output",
                            str(directory),
                            "--ids",
                            fixture["id"],
                            "--count",
                            "1",
                            "--trials",
                            "1",
                            "--seconds",
                            "30",
                        ],
                        stdout=log,
                        stderr=subprocess.STDOUT,
                    )
                summary = json.loads((directory / "summary.json").read_text())
                results.append(
                    dict(
                        fixture=fixture["id"],
                        profile=profile,
                        bitrate=bitrate,
                        elapsed_seconds=time.monotonic() - started,
                        complete=summary.get("completed") == 2,
                        status="passed" if process.returncode == 0 else "failed",
                    )
                )
                write_json(
                    args.output / "summary.json",
                    dict(
                        planned=len(fixtures) * 2,
                        completed=len(results),
                        passed=sum(r["status"] == "passed" for r in results),
                        results=results,
                    ),
                )
                if not results[-1]["complete"]:
                    raise RuntimeError("incomplete network case")
    finally:
        configure("unrestricted")
    raise SystemExit(any(r["status"] != "passed" for r in results))


if __name__ == "__main__":
    main()
