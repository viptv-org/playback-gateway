#!/usr/bin/env python3
"""Shape only the owned upstream inside a Docker --network=none namespace."""

import argparse
from pathlib import Path
import subprocess


def configure(profile, bitrate=None):
    assert Path("/.dockerenv").exists(), "never shape host networking"
    assert sorted(p.name for p in Path("/sys/class/net").iterdir()) == ["lo"], (
        "requires network none"
    )
    run = lambda *args: subprocess.run(["tc", *args], check=True, capture_output=True)
    if profile == "unrestricted":
        subprocess.run(["tc", "qdisc", "del", "dev", "lo", "root"], capture_output=True)
        return
    run("qdisc", "replace", "dev", "lo", "root", "handle", "1:", "prio", "bands", "3")
    if profile == "rtt50":
        # Each direction traverses the loopback egress qdisc once.
        run(
            "qdisc",
            "replace",
            "dev",
            "lo",
            "parent",
            "1:3",
            "handle",
            "30:",
            "netem",
            "delay",
            "25ms",
        )
    else:
        assert bitrate and bitrate > 0
        run(
            "qdisc",
            "replace",
            "dev",
            "lo",
            "parent",
            "1:3",
            "handle",
            "30:",
            "netem",
            "rate",
            str(bitrate * 2) + "bit",
        )
    for direction in ("src", "dst"):
        run(
            "filter",
            "add",
            "dev",
            "lo",
            "parent",
            "1:",
            "protocol",
            "ip",
            "prio",
            "3",
            "u32",
            "match",
            "ip",
            direction,
            "11.255.255.1/32",
            "flowid",
            "1:3",
        )


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("profile", choices=["unrestricted", "rtt50", "bandwidth2x"])
    p.add_argument("--bitrate", type=int)
    a = p.parse_args()
    configure(a.profile, a.bitrate)


if __name__ == "__main__":
    main()
