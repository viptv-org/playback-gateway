#!/usr/bin/env python3
"""Capture the owned server while independent clients run on another machine."""

import argparse
import json
from pathlib import Path
import sys
import time

from corpus import write_json
from measure import Sample


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--run", type=Path, default=Path("/run/lab"))
    p.add_argument("--output", type=Path, required=True)
    args = p.parse_args()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with Sample(args.run) as sample:
        print(json.dumps(dict(ready=True, wall_ns=time.time_ns())), flush=True)
        sys.stdin.buffer.read(1)
    write_json(args.output.with_suffix(".resources.json"), sample.rows)
    write_json(args.output.with_suffix(".private-commands.json"), sample.commands)
    print(
        json.dumps(
            dict(
                complete=True,
                commands=sample.commands,
                resources_file=str(args.output.with_suffix(".resources.json")),
            )
        ),
        flush=True,
    )


if __name__ == "__main__":
    main()
