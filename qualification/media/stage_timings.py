#!/usr/bin/env python3
"""Correlate private gateway events with real backend/browser observations."""

import argparse
from datetime import datetime
import json
from pathlib import Path
import re

from corpus import write_json
from report import percentile, read_results


def correlate(rows, log):
    traces = {}
    for line in log.splitlines():
        trace = re.search(r"\btrace=(\d+)\b", line)
        if not trace:
            continue
        try:
            stamp = (
                datetime.fromisoformat(
                    line.split()[0].replace("Z", "+00:00")
                ).timestamp()
                * 1000
            )
        except ValueError:
            continue
        record = traces.setdefault(trace[1], {})
        if 'operation="create"' not in line:
            continue
        if "Gateway request completed" in line:
            elapsed = re.search(r"\belapsed_ms=(\d+)", line)
            if elapsed:
                record.update(
                    gateway_total_ms=int(elapsed[1]),
                    gateway_started_wall_ms=stamp - int(elapsed[1]),
                )
        if "Playback preparation completed" in line:
            for name in ("probe_ms", "engine_ready_ms"):
                value = re.search(r"\b" + name + r"=(\d+)", line)
                if value:
                    record[name] = int(value[1])
        if "Source inspection cache " in line:
            record["probe_cache_observed"] = "hit" if "cache hit" in line else "miss"
        if "Playback producer launched" in line:
            output = "output_id=" in line
            record[
                "output_launched_wall_ms" if output else "ingest_launched_wall_ms"
            ] = stamp
    results = []
    for row in rows:
        record = dict(
            fixture=row["fixture"],
            trial=row.get("trial"),
            cache=row.get("cache"),
            status=row["status"],
            **traces.get(row.get("gateway_trace"), {}),
        )
        for name in ("first_frame_ms", "first_media_ms", "first_audio_signal_ms"):
            record[name] = row.get("metrics", {}).get(name)
        for event in row.get("events", []):
            if (
                event["stage"]
                in ("backend_request", "backend_ready", "backend_response")
                and event.get("ms") is not None
            ):
                record.setdefault(event["stage"] + "_ms", event["ms"])
        start = record.get("gateway_started_wall_ms")
        if start:
            for name in ("ingest", "output"):
                launched = record.get(name + "_launched_wall_ms")
                if launched:
                    record[name + "_launch_after_request_ms"] = launched - start
        results.append(record)
    metrics = {}
    for name in (
        "gateway_total_ms",
        "probe_ms",
        "engine_ready_ms",
        "backend_ready_ms",
        "first_frame_ms",
        "first_media_ms",
        "first_audio_signal_ms",
    ):
        values = [
            r[name]
            for r in results
            if r.get(name) is not None and r["status"] == "passed"
        ]
        metrics[name] = dict(
            samples=len(values),
            p50=percentile(values, 0.5),
            p95=percentile(values, 0.95),
        )
    return dict(
        results=results,
        distributions=metrics,
        scope="Gateway spans and browser play-action observations use separate clocks. Cross-host source delay additionally requires clock_sync.py; no public API changed.",
    )


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--campaign", type=Path, required=True)
    p.add_argument("--gateway-log", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    args = p.parse_args()
    write_json(
        args.output,
        correlate(read_results(args.campaign), args.gateway_log.read_text()),
    )


if __name__ == "__main__":
    main()
