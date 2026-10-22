#!/usr/bin/env python3
"""Summarize a full cold screen and select a fixed number of diagnostic repeats.

Every first-pass failure or result within 20% of Stremio (including losses) gets
two additional pairs. Repeats never replace earlier attempts. This is screening,
not the twenty-pair per-source qualification gate.
"""
import argparse
import copy
import hashlib
import importlib.util
import json
import math
from pathlib import Path
import statistics

spec = importlib.util.spec_from_file_location("desktop", Path(__file__).with_name("benchmark-desktop.py"))
desktop = importlib.util.module_from_spec(spec)
spec.loader.exec_module(desktop)
ENGINES = ["candidate", "stremio"]
IDENTITY = ("seed", "states", "startup_only", "metadata_timeout_ms", "render_timeout_ms",
            "candidate_grace", "candidate_profile", "sha256")


def latency(row):
    if not isinstance(row.get("passed"), bool):
        raise ValueError("missing attempt outcome")
    if not row["passed"]:
        return math.inf
    value = row.get("first_frame_seconds")
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value <= 0:
        raise ValueError("missing successful first-frame measurement")
    return value


def repeat_needed(pair):
    c, s = (latency(pair[name]) for name in ENGINES)
    return not all(row["passed"] for row in pair.values()) or c > .8 * s


def load_batch(directory, cases):
    run_path, result_path = directory / "run.json", directory / "results.jsonl"
    run = json.loads(run_path.read_text())
    if any(key not in run for key in IDENTITY) or run["states"] != ["cold"] or run["startup_only"] is not True:
        raise ValueError("missing cold measurement identity")
    rows = [json.loads(line) for line in result_path.read_text().splitlines()]
    expected = [(trial, case, engine, "cold") for trial, case, order in desktop.trial_schedule(
        len(cases), ENGINES, run["trials"], run.get("trial_offset", 0), run["seed"]) for engine in order]
    actual = [(row["trial"], row["case"], row["engine"], row["state"]) for row in rows]
    if actual != expected:
        raise ValueError("missing, duplicate or reordered attempts")
    for row in rows:
        if row.get("hash") != cases[row["case"]]["hash"]:
            raise ValueError("source identity changed")
        latency(row)
    return run, rows, dict(directory=str(directory.resolve()),
                          run_sha256=hashlib.sha256(run_path.read_bytes()).hexdigest(),
                          results_sha256=hashlib.sha256(result_path.read_bytes()).hexdigest())


def summarize(cases, rows):
    cells = []
    for case in cases:
        current = [row for row in rows if row["hash"] == case["hash"]]
        cell = {key: case[key] for key in ("case", "label", "hash")}
        medians, failures = {}, {}
        for engine in ENGINES:
            attempts = [row for row in current if row["engine"] == engine]
            if not attempts:
                raise ValueError("missing engine attempts")
            values = sorted(latency(row) for row in attempts)
            median = statistics.median(values)
            medians[engine] = median
            failures[engine] = sum(not row["passed"] for row in attempts) / len(attempts)
            cell[engine] = dict(attempts=len(attempts), successes=sum(row["passed"] for row in attempts),
                                failures=sum(not row["passed"] for row in attempts),
                                median_seconds=median if math.isfinite(median) else None,
                                median_status="measured" if math.isfinite(median) else "failure_censored")
        c, s = (medians[name] for name in ENGINES)
        cf, sf = (failures[name] for name in ENGINES)
        if all(cell[name]["successes"] == 0 for name in ENGINES):
            result = "neither_started"
        elif math.isfinite(c) and c <= s and cf <= sf and (c < s or cf < sf):
            result = "candidate_favored"
        elif math.isfinite(s) and s <= c and sf <= cf and (s < c or sf < cf):
            result = "stremio_favored"
        elif math.isfinite(c) and c == s and cf == sf:
            result = "tied"
        else:
            result = "inconclusive"
        cell["diagnostic_result"] = result
        cell["qualification"] = False
        cells.append(cell)
    return cells


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cases", type=Path, required=True)
    parser.add_argument("--first-pass", type=Path, required=True)
    parser.add_argument("--followup", type=Path)
    parser.add_argument("--repeat-cases", type=Path)
    parser.add_argument("--write-followup", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if bool(args.followup) != bool(args.repeat_cases):
        parser.error("followup and repeat-cases must be provided together")
    cases = desktop.load_magnet_cases(args.cases)
    canonical_ids = {case["hash"]: case["case"] for case in cases}
    if len(canonical_ids) != len(cases):
        parser.error("source hashes must be unique")
    run, rows, receipt = load_batch(args.first_pass, cases)
    if run["trials"] != 1 or run.get("trial_offset", 0) != 0:
        parser.error("first pass must contain exactly trial zero")
    repeats = []
    for case in cases:
        pair = {row["engine"]: row for row in rows if row["hash"] == case["hash"]}
        if repeat_needed(pair):
            repeats.append(dict(copy.deepcopy(case), original_case=case["case"], case=len(repeats)))
    if args.write_followup:
        with args.write_followup.open("x") as file:
            json.dump(dict(sources=repeats), file, indent=2)
    batches = [receipt]
    if args.followup:
        repeat_cases = desktop.load_magnet_cases(args.repeat_cases)
        if repeat_cases != repeats:
            parser.error("followup sources differ from the predeclared selection rule")
        follow_run, follow_rows, follow_receipt = load_batch(args.followup, repeat_cases)
        if any(follow_run[key] != run[key] for key in IDENTITY):
            parser.error("artifact or measurement identity changed")
        if follow_run["trials"] != 2 or follow_run.get("trial_offset", 0) != 1:
            parser.error("followup must contain exactly trials one and two")
        rows += follow_rows
        batches.append(follow_receipt)
    report = dict(scope="Diagnostic Linux cold first-image retest; not per-source qualification",
                  qualification=False, screening_only=True, batches=batches,
                  source_manifest_sha256=hashlib.sha256(args.cases.read_bytes()).hexdigest(),
                  identity={key: run[key] for key in IDENTITY},
                  repeat_rule="Either engine fails, or candidate latency exceeds 80% of reference latency",
                  repeat_case_ids=[case["original_case"] for case in repeats],
                  repeat_completed=bool(args.followup), cases=summarize(cases, rows), rows=rows)
    for row in report["rows"]:
        row["original_case"] = canonical_ids[row["hash"]]
        row["batch"] = 0 if row["trial"] == 0 else 1
        # Verbose frame samples remain in the hashed original JSONL files.
        row.pop("player_samples", None)
    with args.output.open("x") as file:
        json.dump(report, file, indent=2, allow_nan=False)
        file.write("\n")
    for cell in report["cases"]:
        def fmt(engine):
            item = cell[engine]
            median = f"{item['median_seconds']:.3f}s" if item["median_seconds"] is not None else "failure-censored"
            return f"{item['successes']}/{item['attempts']} {median}"
        print(cell["label"], fmt("candidate"), fmt("stremio"), cell["diagnostic_result"])
    print("Repeat case IDs:", report["repeat_case_ids"])


if __name__ == "__main__":
    main()
