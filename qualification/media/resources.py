#!/usr/bin/env python3
"""Evaluate continuous-soak resource evidence; missing observations never pass."""

import argparse
import json
from pathlib import Path
import statistics

from corpus import digest, write_json


def bounded_growth(samples):
    if len(samples) < 6:
        return dict(stable=False, reason="insufficient_idle_samples")
    samples = sorted(samples)
    warmed = samples[len(samples) // 5 :]
    count = max(1, len(warmed) // 3)
    first = statistics.median(x[1] for x in warmed[:count])
    last = statistics.median(x[1] for x in warmed[-count:])
    return dict(
        stable=last - first <= max(32 * 1024**2, first * 0.1),
        first_median_bytes=first,
        last_median_bytes=last,
        growth_bytes=last - first,
        samples=len(samples),
        warmup_samples=len(samples) - len(warmed),
        peak_bytes=max(x[1] for x in samples),
    )


def analyze(campaign, resources, origin=None, clock_sync=None):
    summary = json.loads((campaign / "summary.json").read_text())
    elapsed = summary.get(
        "elapsed_seconds", summary["continuous_run"]["elapsed_seconds"]
    )
    start = summary["continuous_run"]["started_at"] * 1e9
    end = start + elapsed * 1e9
    rows = []
    invalid = 0
    for line in resources.open():
        try:
            row = json.loads(line)
        except ValueError:
            invalid += 1
            continue
        if start - 2e9 <= row["wall_ns"] <= end + 2e9:
            rows.append(row)
    cases = []
    for path in campaign.glob("*.json"):
        value = json.loads(path.read_text())
        if (
            isinstance(value, dict)
            and value.get("case_id")
            and value.get("backend_resources")
        ):
            cases.append(value)
    backends = [v for row in cases for v in row["backend_resources"].values() if v]
    backend_ids = {(v["pid"], v.get("start_ticks")) for v in backends}
    backend = bounded_growth([(v["wall_ns"], v["rss_bytes"]) for v in backends])
    checkpoints = [
        json.loads(path.read_text()) for path in campaign.glob("cleanup-*.json")
    ]
    idle = []
    gateway_ids = set()
    peak_storage = 0
    directory_evidence = True
    for row in rows:
        storage = row.get("media_storage", {})
        peak_storage = max(peak_storage, storage.get("allocated_bytes", 0))
        for process in row.get("processes", []):
            if process["role"] != "gateway":
                continue
            gateway_ids.add((process["pid"], process.get("start_ticks")))
            if (
                storage.get("files") == 0
                and storage.get("allocated_bytes") == 0
                and any(
                    abs(row["wall_ns"] - c["wall_ns"]) < 3e9 and c["status"] == "passed"
                    for c in checkpoints
                )
            ):
                idle.append((row["wall_ns"], process["rss_bytes"]))
                directory_evidence &= storage.get("directories") == 0
    gateway = bounded_growth(idle)
    continuity = len(gateway_ids) == 1 and all(
        ticks is not None for _, ticks in gateway_ids
    )
    complete = bool(
        summary.get("complete")
        and summary.get("backend_continuous")
        and len(backend_ids) == 1
        and rows
    )
    sample_coverage = len(rows) / max(1, elapsed)
    cleanup = bool(
        checkpoints
        and all(c["status"] == "passed" for c in checkpoints)
        and all(c.get("capacity_reclaimed") for c in cases)
    )
    result = dict(
        complete=complete,
        failed=summary["failed"],
        elapsed_seconds=elapsed,
        observed_playback_seconds=summary["continuous_run"][
            "observed_playback_seconds"
        ],
        backend_continuous=len(backend_ids) == 1
        and summary.get("backend_continuous") is True,
        gateway_continuous=continuity,
        sample_coverage=sample_coverage,
        memory_stable=backend["stable"]
        and gateway["stable"]
        and sample_coverage >= 0.8,
        storage_stable=bool(idle)
        and directory_evidence
        and peak_storage < 512 * 1024**2,
        cleanup_complete=cleanup,
        backend_memory=backend,
        gateway_memory=gateway,
        peak_media_storage_bytes=peak_storage,
        idle_samples=len(idle),
        invalid_resource_lines=invalid,
        source_sha256={
            "summary": digest(campaign / "summary.json"),
            "resources": digest(resources),
        },
    )
    if origin:
        alignment = json.loads(clock_sync.read_text()) if clock_sync else None
        offset = alignment["remote_minus_local_ms"] if alignment else 0
        result["source_clock_alignment"] = alignment or dict(
            scope="same-host clocks assumed; remote source delay requires an explicit clock-sync receipt"
        )
        clocks = {}
        for line in origin.open():
            try:
                row = json.loads(line)
            except ValueError:
                continue
            if row.get("source_epoch") is not None:
                clocks[row["query_sha256"]] = (
                    row["source_epoch"],
                    row["source_lead_seconds"],
                )
        delays = {}
        for case in cases:
            clock = clocks.get(case.get("source_query_sha256"))
            if (
                not clock
                or not case.get("browser_time_origin_ms")
                or not case.get("metrics", {}).get("av_marker_identity_verified")
            ):
                continue
            offset = (case.get("marker_recipe") or {}).get("clock_offset", 0)
            values = [
                case["browser_time_origin_ms"]
                + offset
                + p["video_ms"]
                - (clock[0] + p["id"] - offset - clock[1]) * 1000
                for p in case["timing_identity"]["pairs"]
            ]
            if values:
                delays.setdefault(case["fixture"], []).extend(values)
        result["source_to_render_delay_ms"] = {
            key: dict(
                minimum=min(values),
                median=statistics.median(values),
                maximum=max(values),
                samples=len(values),
            )
            for key, values in delays.items()
        }
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--campaign", type=Path, required=True)
    p.add_argument("--resources", type=Path, required=True)
    p.add_argument("--origin", type=Path)
    p.add_argument("--clock-sync", type=Path)
    p.add_argument("--output", type=Path, required=True)
    args = p.parse_args()
    write_json(
        args.output,
        analyze(args.campaign, args.resources, args.origin, args.clock_sync),
    )


if __name__ == "__main__":
    main()
