#!/usr/bin/env python3
"""Bounded one-Hz resource evidence for the owned gateway and its media children."""

import json
import os
from pathlib import Path
import time


def children(pid):
    result = set()
    for task in Path(f"/proc/{pid}/task").glob("*"):
        try:
            result.update(int(p) for p in (task / "children").read_text().split())
        except OSError:
            pass
    return result


def snapshot(pid, role):
    try:
        raw = Path(f"/proc/{pid}/stat").read_text()
        fields = raw[raw.rfind(")") + 2 :].split()
        row = dict(
            pid=pid,
            role=role,
            cpu_ticks=int(fields[11]) + int(fields[12]),
            reaped_child_cpu_ticks=int(fields[13]) + int(fields[14]),
            rss_bytes=int(fields[21]) * os.sysconf("SC_PAGE_SIZE"),
            start_ticks=int(fields[19]),
        )
        clients = {}
        try:
            for fd in Path(f"/proc/{pid}/fdinfo").iterdir():
                values = dict(
                    line.split(":", 1)
                    for line in fd.read_text().splitlines()
                    if line.startswith("drm-") and ":" in line
                )
                if values.get("drm-client-id"):
                    clients[values["drm-client-id"].strip()] = {
                        k: v.strip()
                        for k, v in values.items()
                        if k.startswith(("drm-engine-", "drm-memory-"))
                        or k == "drm-driver"
                    }
            row["gpu_clients"] = clients
        except OSError:
            row["gpu_clients"] = None
        return row
    except (OSError, ValueError, IndexError):
        return None


def main():
    run = Path("/run/lab")
    config = json.loads((run / "fixture.json").read_text())
    pid = config["gateway_pid"]
    with (run / "resources.jsonl").open("a", buffering=1) as output:
        while Path(f"/proc/{pid}").exists():
            started = time.monotonic()
            ids = {pid: "gateway", os.getpid(): "observer"}
            if config.get("origin_pid"):
                ids[config["origin_pid"]] = "origin"
            for child in children(pid):
                try:
                    ids[child] = Path(f"/proc/{child}/comm").read_text().strip()
                except OSError:
                    pass
            row = dict(
                wall_ns=time.time_ns(),
                monotonic_ns=time.monotonic_ns(),
                ticks_per_second=os.sysconf("SC_CLK_TCK"),
                processes=[s for p, role in ids.items() if (s := snapshot(p, role))],
            )
            count = 0
            directories = 0
            allocated = 0
            logical = 0
            for file in Path("/state/media").rglob("*"):
                try:
                    if file.is_dir():
                        directories += 1
                    if file.is_file():
                        stat = file.stat()
                        count += 1
                        allocated += stat.st_blocks * 512
                        logical += stat.st_size
                except OSError:
                    pass
            row["media_storage"] = dict(
                files=count,
                directories=directories,
                allocated_bytes=allocated,
                logical_bytes=logical,
            )
            for name in ("cpu.stat", "memory.current", "memory.peak", "io.stat"):
                try:
                    row[name] = Path("/sys/fs/cgroup", name).read_text().strip()
                except OSError:
                    row[name] = None
            output.write(json.dumps(row, separators=(",", ":")) + "\n")
            time.sleep(max(0, 1 - (time.monotonic() - started)))


if __name__ == "__main__":
    main()
