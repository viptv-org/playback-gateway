#!/usr/bin/env python3
"""Combine consecutive cold-start batches without rerunning or discarding trials."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path

spec = importlib.util.spec_from_file_location("desktop", Path(__file__).with_name("benchmark-desktop.py"))
desktop = importlib.util.module_from_spec(spec)
spec.loader.exec_module(desktop)


def merge_batches(batches, engines):
    if not batches or set(engines) != {"candidate", "stremio"} or len(engines) != 2:
        raise ValueError("candidate and stremio batches required")
    fields = ("seed", "states", "startup_only", "metadata_timeout_ms", "render_timeout_ms",
              "candidate_grace", "candidate_profile", "sha256")
    first = batches[0][0]
    if any(key not in first for key in fields):
        raise ValueError("missing artifact or measurement identity")
    if first["states"] != ["cold"] or first["startup_only"] is not True:
        raise ValueError("only cold startup batches can be combined")
    combined, next_trial, identities = [], 0, None
    for run, rows in batches:
        if any(key not in run or run[key] != first[key] for key in fields):
            raise ValueError("artifact or measurement identity changed")
        offset, count = run.get("trial_offset", 0), run["trials"]
        if offset != next_trial or not isinstance(count, int) or count <= 0:
            raise ValueError("batches must be consecutive from trial zero")
        cases = {row["case"] for row in rows}
        if cases != set(range(len(cases))) or not cases:
            raise ValueError("missing or noncontiguous cases")
        observed = {}
        for row in rows:
            if not isinstance(row.get("passed"), bool) or not row.get("hash"):
                raise ValueError("missing attempt outcome or source identity")
            if row["case"] in observed and observed[row["case"]] != row["hash"]:
                raise ValueError("source changed within batch")
            observed[row["case"]] = row["hash"]
        if identities is not None and identities != observed:
            raise ValueError("source identity changed between batches")
        identities = observed
        expected = [(trial, case, engine, "cold")
                    for trial, case, order in desktop.trial_schedule(len(cases), engines, count, offset, run["seed"])
                    for engine in order]
        actual = [(row["trial"], row["case"], row["engine"], row["state"]) for row in rows]
        if actual != expected:
            raise ValueError("missing, duplicate or reordered attempts")
        combined.extend(rows)
        next_trial += count
    return combined, next_trial


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inputs", type=Path, nargs="+", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--engines", nargs=2, default=["candidate", "stremio"])
    args = parser.parse_args()
    batches, inputs = [], []
    for directory in args.inputs:
        run, results = directory / "run.json", directory / "results.jsonl"
        batches.append((json.loads(run.read_text()), [json.loads(line) for line in results.read_text().splitlines()]))
        inputs.append(dict(directory=str(directory.resolve()),
                           run_sha256=hashlib.sha256(run.read_bytes()).hexdigest(),
                           results_sha256=hashlib.sha256(results.read_bytes()).hexdigest()))
    try:
        rows, trials = merge_batches(batches, args.engines)
    except (ValueError, KeyError, TypeError) as error:
        parser.error(str(error))
    args.output.mkdir(parents=True, exist_ok=False)
    (args.output / "results.jsonl").write_text("".join(json.dumps(row) + "\n" for row in rows))
    report = desktop.screen.summary(rows, trials)
    report.update(measurement="KMP rendered images on owned virtual display", cold_start_only=True,
                  qualification=False, inputs=inputs, paired_trials_per_case=trials,
                  performance_gate=desktop.screen.cold_start_gate(rows))
    report["cold_start_qualification"] = report["performance_gate"]["passed"]
    (args.output / "summary.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report["performance_gate"]))
    return 0 if report["cold_start_qualification"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
