#!/usr/bin/env python3
"""Export capacity curves, retaining saturated and failed viewer counts."""

import argparse
import csv
import json
from pathlib import Path

from corpus import digest, write_json
from report import percentile


def summarize(directory):
    points = []
    for path in sorted(directory.glob("batch-*.json")):
        row = json.loads(path.read_text())
        if not isinstance(row, dict) or "mode" not in row:
            continue
        startup = [
            r["startup_ms"]
            for r in row.get("results", [])
            if r.get("startup_ms") is not None and r.get("status") == "passed"
        ]
        samples = path.with_name(path.stem + ".resources.json")
        peak_gateway = peak_ffmpeg = peak_processes = 0
        cpu = {}
        if samples.exists():
            for sample in json.loads(samples.read_text()):
                producers = [
                    p for p in sample.get("processes", []) if p.get("kind") == "ffmpeg"
                ]
                peak_processes = max(peak_processes, len(producers))
                peak_ffmpeg = max(
                    peak_ffmpeg, sum(p.get("rss_bytes", 0) for p in producers)
                )
                for process in sample.get("processes", []):
                    kind = process.get("kind")
                    if kind == "playback-gateway":
                        peak_gateway = max(peak_gateway, process.get("rss_bytes", 0))
                    if kind in ("ffmpeg", "playback-gateway"):
                        identity = (kind, process["pid"])
                        ticks = process.get("cpu_ticks", 0)
                        lo, hi = cpu.get(identity, (ticks, ticks))
                        cpu[identity] = (min(lo, ticks), max(hi, ticks))
        points.append(
            dict(
                mode=row["mode"],
                viewers=row["viewers"],
                status=row["status"],
                startup_p50_ms=percentile(startup, 0.5),
                startup_p95_ms=percentile(startup, 0.95),
                passed_viewers=sum(
                    r.get("status") == "passed" for r in row.get("results", [])
                ),
                capacity_reclaimed=row.get("capacity_reclaimed"),
                sharing_correct=row.get("sharing_correct"),
                upstream_ffmpeg_processes=row.get("upstream_ffmpeg_processes"),
                peak_ffmpeg_processes=peak_processes,
                peak_ffmpeg_rss_bytes=peak_ffmpeg,
                peak_gateway_rss_bytes=peak_gateway,
                observed_ffmpeg_cpu_ticks=sum(
                    hi - lo for (kind, _), (lo, hi) in cpu.items() if kind == "ffmpeg"
                ),
                observed_gateway_cpu_ticks=sum(
                    hi - lo
                    for (kind, _), (lo, hi) in cpu.items()
                    if kind == "playback-gateway"
                ),
                resource_sha256=digest(samples) if samples.exists() else None,
            )
        )
    return sorted(points, key=lambda p: (p["mode"], p["viewers"]))


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--campaign", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument(
        "--plot",
        action="store_true",
        help="also export a static SVG/PNG using installed Matplotlib",
    )
    args = p.parse_args()
    points = summarize(args.campaign)
    write_json(
        args.output,
        dict(
            points=points,
            scope="Observed CPU tick deltas exclude process lifetime outside the sampling window. Saturation attribution requires the preserved GPU/I/O/process traces; a failed point is never silently removed.",
        ),
    )
    with args.output.with_suffix(".csv").open("w") as output:
        writer = csv.DictWriter(
            output,
            fieldnames=list(points[0]) if points else ["mode", "viewers", "status"],
        )
        writer.writeheader()
        writer.writerows(points)
    lines = [
        "# Capacity observations",
        "",
        "| Mode | Viewers | Result | Startup p95 (ms) | FFmpeg processes, peak |",
        "|---|---:|---|---:|---:|",
    ]
    lines += [
        f"| {r['mode']} | {r['viewers']} | {r['status']} | {r['startup_p95_ms']} | {r['peak_ffmpeg_processes']} |"
        for r in points
    ]
    args.output.with_suffix(".md").write_text("\n".join(lines) + "\n")
    if args.plot:
        import matplotlib

        matplotlib.use("Agg")
        import matplotlib.pyplot as plt

        figure, axis = plt.subplots(figsize=(8, 4.5), layout="constrained")
        for mode in sorted({row["mode"] for row in points}):
            group = [row for row in points if row["mode"] == mode]
            axis.plot(
                [row["viewers"] for row in group],
                [
                    row["startup_p95_ms"]
                    if row["startup_p95_ms"] is not None
                    else float("nan")
                    for row in group
                ],
                marker="o",
                label=mode,
            )
        failed = [row for row in points if row["status"] != "passed"]
        if failed:
            axis.scatter(
                [row["viewers"] for row in failed],
                [row["startup_p95_ms"] or 0 for row in failed],
                marker="x",
                color="red",
                label="failed batch (0 = no timing)",
            )
        axis.set_xlabel("Concurrent viewers")
        axis.set_ylabel("Startup p95 (ms; successful starts)")
        axis.set_title(args.campaign.name + " — observed capacity")
        axis.legend()
        figure.savefig(args.output.with_suffix(".svg"))
        figure.savefig(args.output.with_suffix(".png"), dpi=160)
        plt.close(figure)


if __name__ == "__main__":
    main()
