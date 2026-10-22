#!/usr/bin/env python3
"""Fast controlled BitTorrent request-pipeline comparison of two Go test binaries.

Build both with pipeline_qualification_test.go, then pass their paths here.
This measures exact verified payload delivery, not rendered-frame qualification.
Public swarms, discovery and GUI startup are excluded from this experiment.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import random
import statistics
import subprocess
import time


def compare_screen(rows, latency, allowed_percent):
    """A quick rejection gate; three pairs do not establish final parity."""
    medians = {}
    for name in ("baseline", "candidate"):
        selected = [row for row in rows if row["engine"] == name and row["latency_ms"] == latency]
        values = [row.get("elapsed_ms", math.inf) if row["passed"] else math.inf for row in selected]
        if not values or any(not isinstance(value, (int, float)) or not math.isfinite(value)
                             or value <= 0 for value in values):
            return dict(latency_ms=latency, passed=False, reason="failed_or_missing_measurement")
        medians[name] = statistics.median(values)
    change = 100 * (medians["candidate"] / medians["baseline"] - 1)
    return dict(latency_ms=latency, passed=change <= allowed_percent,
                median_change_percent=change, allowed_regression_percent=allowed_percent,
                reason="within_screen_limit" if change <= allowed_percent else "median_regression")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--latency-ms", type=int, nargs="+", default=[0, 120])
    parser.add_argument("--timeout", type=float, default=20)
    parser.add_argument("--seed", type=int, default=20261022)
    parser.add_argument("--mixed-peer", action="store_true", help="Introduce a fast peer after a slow peer's first response")
    parser.add_argument("--max-regression-percent", type=float, default=5,
                        help="Reject a controlled median regression above this percent (default: 5)")
    args = parser.parse_args()
    if not 1 <= args.trials <= 100 or not 0 < args.timeout <= 120:
        parser.error("invalid trial count or timeout")
    if not args.latency_ms or any(not 0 <= value <= 1000 for value in args.latency_ms):
        parser.error("latency must be between 0 and 1000 milliseconds")
    if not math.isfinite(args.max_regression_percent) or not 0 <= args.max_regression_percent <= 100:
        parser.error("regression allowance must be between 0 and 100 percent")
    os.umask(0o077)
    args.output.mkdir(parents=True, exist_ok=False)
    engines = {"baseline": args.baseline.resolve(), "candidate": args.candidate.resolve()}
    identity = {"seed": args.seed, "trials": args.trials, "latency_ms": args.latency_ms, "mixed_peer": args.mixed_peer,
                "max_regression_percent": args.max_regression_percent,
                "measurement": "controlled 4 MiB HTTP payload through BitTorrent; two verified pieces",
                "rendered_frame_qualification": False,
                "artifacts": {name: {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                              for name, path in engines.items()}}
    (args.output / "run.json").write_text(json.dumps(identity, indent=2))
    rows = []
    rng = random.Random(args.seed)
    for latency in args.latency_ms:
        for trial in range(args.trials):
            order = list(engines)
            rng.shuffle(order)
            for name in order:
                row = {"engine": name, "trial": trial, "latency_ms": latency, "passed": False}
                started = time.monotonic()
                log = args.output / f"{name}-{latency}-{trial}.log"
                try:
                    run = subprocess.run([str(engines[name]), "-test.run", "^TestPipelineQualification$",
                                          "-test.count=1", "-test.v"], capture_output=True, timeout=args.timeout,
                                         env=dict(os.environ, TORRENT_PIPELINE_FIXTURE="1",
                                                  TORRENT_PIPELINE_DELAY_MS=str(latency),
                                                  TORRENT_PIPELINE_MIXED="1" if args.mixed_peer else "0"))
                    log.write_bytes(run.stdout + run.stderr)
                    samples = [line.split("PIPELINE ", 1)[1] for line in run.stdout.decode().splitlines()
                               if "PIPELINE " in line]
                    if run.returncode != 0 or len(samples) != 1:
                        raise RuntimeError("fixture_failed_or_missing_result")
                    sample = json.loads(samples[0])
                    if not (sample.get("payload_equal") is True and sample.get("verified_pieces") == 2
                            and sample.get("payload_bytes") == 4 << 20
                            and sample.get("upload_bytes_per_second") == 4 << 20
                            and sample.get("request_latency_ms") == latency
                            and sample.get("mixed_peer", False) == args.mixed_peer
                            and isinstance(sample.get("elapsed_ms"), (int, float))
                            and math.isfinite(sample["elapsed_ms"]) and sample["elapsed_ms"] > 0):
                        raise RuntimeError("unmatched_or_invalid_fixture_result")
                    row.update(sample, passed=True)
                except subprocess.TimeoutExpired as error:
                    log.write_bytes((error.stdout or b"") + (error.stderr or b""))
                    row["error"] = "timeout"
                except (RuntimeError, ValueError, UnicodeError, OSError) as error:
                    row["error"] = type(error).__name__
                row["wall_seconds"] = time.monotonic() - started
                rows.append(row)
                (args.output / "results.json").write_text(json.dumps(rows, indent=2, allow_nan=False))
                print(json.dumps(row), flush=True)
    summary = []
    for latency in args.latency_ms:
        for name in engines:
            selected = [row for row in rows if row["engine"] == name and row["latency_ms"] == latency]
            values = [row["elapsed_ms"] if row["passed"] else math.inf for row in selected]
            median = statistics.median(values)
            summary.append({"engine": name, "latency_ms": latency, "trials": len(selected),
                            "failures": sum(not row["passed"] for row in selected),
                            "failure_inclusive_median_ms": median if math.isfinite(median) else None})
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2, allow_nan=False))
    comparisons = [compare_screen(rows, latency, args.max_regression_percent) for latency in args.latency_ms]
    screen = dict(screening_only=True, rendered_frame_qualification=False,
                  passed=all(item["passed"] for item in comparisons), comparisons=comparisons)
    (args.output / "screen.json").write_text(json.dumps(screen, indent=2, allow_nan=False))
    print(json.dumps(screen), flush=True)
    return 0 if screen["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
