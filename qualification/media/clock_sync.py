#!/usr/bin/env python3
"""Bound the origin/browser wall-clock offset without modifying either clock."""

import argparse
import os
from pathlib import Path
import subprocess
import time

from corpus import write_json
from remote import settings


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--credentials", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    args = p.parse_args()
    values = settings(args.credentials)
    command = [
        "sshpass",
        "-e",
        "ssh",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "ConnectTimeout=10",
        values["SERVER_USER"] + "@" + values["SERVER_HOST"],
        "python3 -c 'import time; print(time.time_ns())'",
    ]
    rows = []
    for _ in range(5):
        start = time.time_ns()
        remote = int(
            subprocess.check_output(
                command,
                env=dict(os.environ, SSHPASS=values["SERVER_PASSWORD"]),
                timeout=15,
            )
        )
        end = time.time_ns()
        rows.append(
            dict(
                local_start_ns=start,
                local_end_ns=end,
                remote_ns=remote,
                minimum_offset_ms=(remote - end) / 1e6,
                maximum_offset_ms=(remote - start) / 1e6,
                round_trip_ms=(end - start) / 1e6,
            )
        )
    best = min(rows, key=lambda row: row["round_trip_ms"])
    write_json(
        args.output,
        dict(
            remote_minus_local_ms=(
                best["minimum_offset_ms"] + best["maximum_offset_ms"]
            )
            / 2,
            uncertainty_ms=best["round_trip_ms"] / 2,
            measured_at_ns=time.time_ns(),
            samples=rows,
            scope="SSH round-trip bounds; no assumption of symmetric network delay; repeat after a long campaign to detect clock changes",
        ),
    )


if __name__ == "__main__":
    main()
