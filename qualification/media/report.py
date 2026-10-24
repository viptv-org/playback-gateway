#!/usr/bin/env python3
"""Compare preserved evidence and fail closed on missing qualification suites."""

import argparse
import csv
import json
import math
from pathlib import Path
from corpus import write_json


def percentile(values, q):
    if not values:
        return None
    values = sorted(values)
    position = (len(values) - 1) * q
    low = math.floor(position)
    high = math.ceil(position)
    return values[low] + (values[high] - values[low]) * (position - low)


def read_results(directory):
    rows = []
    for path in sorted(directory.glob("*.json")):
        if any(word in path.name for word in (".resources.", ".private-commands.")):
            continue
        value = json.loads(path.read_text())
        if isinstance(value, dict) and "fixture" in value and "status" in value:
            sample = path.with_name(path.stem + ".resources.json")
            if (
                value.get("producer_wall_ms") is None
                and value.get("ready_wall_ns")
                and value.get("decode_wall_ms")
                and sample.exists()
            ):
                starts = [
                    r["time_ns"]
                    for r in json.loads(sample.read_text())
                    if any(p.get("kind") == "ffmpeg" for p in r.get("processes", []))
                ]
                if starts:
                    value["producer_wall_ms"] = (
                        value["ready_wall_ns"]
                        + value["decode_wall_ms"] * 1e6
                        - min(starts)
                    ) / 1e6
                    value["producer_clock"] = "10ms_process_sample_reconstructed"
            rows.append(value)
    return rows


def key(row):
    return (
        row["fixture"],
        row.get("profile", "browser"),
        row.get("cache", "browser"),
        row.get("trial", 0),
    )


def compare(before, after):
    baseline = {key(r): r for r in before}
    candidate = {key(r): r for r in after}
    if len(baseline) != len(before) or len(candidate) != len(after):
        raise ValueError("duplicate trial identity")
    pairs = []
    for identity in sorted(baseline.keys() | candidate.keys()):
        a = baseline.get(identity)
        b = candidate.get(identity)
        pair = dict(
            identity=identity,
            baseline_status=a.get("status") if a else "missing",
            candidate_status=b.get("status") if b else "missing",
        )
        if a and b and a["status"] == b["status"] == "passed":
            metric = (
                "ready_ms" if "ready_ms" in a and "ready_ms" in b else "first_frame_ms"
            )
            x = a.get(metric, a.get("metrics", {}).get(metric))
            y = b.get(metric, b.get("metrics", {}).get(metric))
            if x is not None and y is not None:
                pair.update(
                    metric=metric,
                    baseline_ms=x,
                    candidate_ms=y,
                    change_ms=y - x,
                    change_percent=(y / x - 1) * 100 if x else None,
                    regression=y - x > 100 and y > x * 1.1,
                )
        pairs.append(pair)
    paired = [p for p in pairs if "baseline_ms" in p]
    return dict(
        pairs=pairs,
        paired_successes=len(paired),
        baseline_p50_ms=percentile([p["baseline_ms"] for p in paired], 0.5),
        candidate_p50_ms=percentile([p["candidate_ms"] for p in paired], 0.5),
        baseline_p95_ms=percentile([p["baseline_ms"] for p in paired], 0.95),
        candidate_p95_ms=percentile([p["candidate_ms"] for p in paired], 0.95),
        regressions=sum(p.get("regression", False) for p in pairs),
        missing=sum(
            "missing" in (p["baseline_status"], p["candidate_status"]) for p in pairs
        ),
        baseline_failures=sum(p["baseline_status"] == "failed" for p in pairs),
        candidate_failures=sum(p["candidate_status"] == "failed" for p in pairs),
    )


def throughput_ratio(row):
    reference = row.get("reference", {})
    duration = row.get("media_seconds", row.get("decoded_seconds", 0))
    actual_wall = row.get("producer_wall_ms")
    reference_wall = reference.get("producer_wall_ms", reference.get("total_wall_ms"))
    if (
        row.get("status") != "passed"
        or reference.get("status") != "passed"
        or duration < 30
        or row.get("consumer_mode") != "copy"
        or not actual_wall
        or not reference_wall
    ):
        return None
    expected_duration = reference.get("media_seconds", reference.get("decoded_seconds"))
    if not expected_duration:
        return None
    return duration / actual_wall / (expected_duration / reference_wall)


def throughput_passed(row):
    ratio = throughput_ratio(row)
    copy = row.get("delivery", {}).get("video_mode") == "copy" and row.get(
        "delivery", {}
    ).get("audio_mode") in ("copy", "none")
    return ratio is not None and ratio >= (0.95 if copy else 0.9)


def acceptance(
    corpus,
    browser,
    startup,
    concurrency,
    soak,
    throughput_trials=None,
    live_concurrency=None,
    network=None,
    browser_startup=None,
    timing=None,
    quality=None,
    captions=None,
    capacity_1080p=None,
    capacity_4k=None,
    shared_throughput=None,
):
    ids = {f["id"] for f in corpus["fixtures"]}
    played = {
        r["fixture"]
        for r in browser
        if r["status"] == "passed" and r.get("capacity_reclaimed")
    }
    groups = {}
    for r in startup:
        if r["status"] == "passed":
            groups.setdefault((r["fixture"], r.get("cache")), set()).add(r.get("trial"))
    complete_startup = {
        f
        for f, _ in groups
        if all(len(groups.get((f, c), set())) >= 20 for c in ("cold", "warm"))
    }
    response = [
        r.get(
            "ready_detection_ms",
            r.get("playable_to_response_ms", r.get("playlist_to_response_ms")),
        )
        for r in startup
        if r.get(
            "ready_detection_ms",
            r.get("playable_to_response_ms", r.get("playlist_to_response_ms")),
        )
        is not None
        and r["status"] == "passed"
    ]
    av = [
        r.get("metrics", {})
        for r in (timing or [])
        if r.get("status") == "passed" and r.get("timing_markers") is True
    ]
    gates = dict(
        broad_browser=len(ids) >= 1000 and ids <= played,
        current_browser_application=len(ids) >= 1000
        and ids <= played
        and all(r.get("driver") == "kmp" for r in browser if r["fixture"] in ids),
        api_startup_coverage=len(
            {r["fixture"] for r in startup if r.get("status") == "passed"}
        )
        >= 40,
        ready_detection=bool(response)
        and len(response)
        >= 0.95 * len([r for r in startup if r.get("status") == "passed"])
        and percentile(response, 0.95) <= 50,
        av_measurement=len(av) >= 6
        and len({r["fixture"] for r in (timing or [])}) >= 6
        and len(av) == len(timing or [])
        and all(
            x.get("av_marker_identity_verified") is True
            and x.get("av_drift_ms") is not None
            and x["av_drift_ms"] <= 100
            for x in av
        ),
        concurrency=bool(
            concurrency
            and concurrency.get("complete")
            and concurrency.get("failed") == 0
            and concurrency.get("completed", 0) >= 16
        ),
        soak=bool(
            soak
            and soak.get("complete")
            and soak.get("failed") == 0
            and soak.get("elapsed_seconds", 0) >= 8 * 3600
            and soak.get("memory_stable") is True
            and soak.get("storage_stable") is True
            and soak.get("cleanup_complete") is True
            and soak.get("backend_continuous") is True
            and soak.get("gateway_continuous") is True
        ),
        live_concurrency=bool(
            live_concurrency
            and live_concurrency.get("complete")
            and live_concurrency.get("failed") == 0
            and live_concurrency.get("completed", 0) >= 16
        ),
        normal_network=bool(
            network
            and network.get("planned", 0) >= 80
            and network.get("completed") == network["planned"]
            and network.get("passed") == network["planned"]
        ),
    )
    throughput = startup if throughput_trials is None else throughput_trials
    exclusive = [
        r["exclusive_overhead_ms"]
        for r in startup
        if r.get("exclusive_overhead_ms") is not None and r["status"] == "passed"
    ]
    hdr_ids = {
        i["id"]
        for i in corpus["fixtures"]
        if i.get("transfer") in ("smpte2084", "arib-std-b67")
    }
    hdr = {r["fixture"]: r for r in (quality or {}).get("results", [])}
    long = [
        r
        for r in browser
        if r["status"] == "passed"
        and r.get("metrics", {}).get("observed_seconds", 0) >= 1800
    ]
    gates.update(
        matched_throughput=len(throughput) >= 40
        and all(throughput_passed(row) for row in throughput),
        gateway_exclusive_overhead=bool(exclusive)
        and percentile(exclusive, 0.95) <= 250,
        hdr_quality=bool(hdr_ids)
        and (quality or {}).get("complete") is True
        and len(hdr) == len((quality or {}).get("results", []))
        and hdr_ids == hdr.keys()
        and all(
            q.get("status") == "passed"
            and q.get("color_metadata_correct")
            and q.get("reference_comparison_passed")
            for q in hdr.values()
        ),
        long_playback=len(long) >= 6,
    )
    actual_groups = {}
    for row in browser_startup or []:
        if (
            row.get("status") == "passed"
            and row.get("driver") in ("current-api", "kmp")
            and row.get("capacity_reclaimed")
            and row.get("metrics", {}).get("first_frame_ms") is not None
        ):
            actual_groups.setdefault((row["fixture"], row.get("cache")), set()).add(
                row.get("trial")
            )
    complete_browser_startup = sum(
        all(
            len(actual_groups.get((fixture, cache), set())) >= 20
            for cache in ("cold", "warm")
        )
        for fixture in {key[0] for key in actual_groups}
    )
    gates["backend_browser_startup"] = complete_browser_startup >= 40
    gates["startup_repetitions"] = complete_browser_startup >= 40
    for name, summary in [
        ("capacity_1080p", capacity_1080p),
        ("capacity_4k", capacity_4k),
    ]:
        gates[name] = bool(
            summary
            and summary.get("complete")
            and summary.get("failed") == 0
            and summary.get("completed", 0) >= 16
        )
    caption_rows = captions or []
    gates["captions"] = len(caption_rows) >= 3 and all(
        r.get("status") == "passed"
        and r.get("player_profile") == "mse"
        and r.get("metrics", {}).get("caption_cues_seen", 0) > 0
        for r in caption_rows
    )
    shared = shared_throughput or []
    gates["matched_shared_live"] = len(shared) >= 3 and all(
        throughput_passed(r) and r.get("reference", {}).get("shared_graph") is True
        for r in shared
    )
    return dict(
        accepted=all(gates.values()),
        gates=gates,
        unplayed=sorted(ids - played),
        complete_startup_fixtures=len(complete_startup),
        complete_backend_browser_startup_fixtures=complete_browser_startup,
        ready_detection_observed=len(response),
    )


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--baseline", type=Path, required=True)
    p.add_argument("--candidate", type=Path, required=True)
    p.add_argument("--corpus", type=Path, required=True)
    p.add_argument("--browser", type=Path, required=True)
    p.add_argument("--concurrency", type=Path)
    p.add_argument("--soak", type=Path)
    p.add_argument("--live-concurrency", type=Path)
    p.add_argument("--network", type=Path)
    p.add_argument("--browser-startup", type=Path)
    p.add_argument("--baseline-browser-startup", type=Path)
    p.add_argument("--timing", type=Path)
    p.add_argument("--quality", type=Path)
    p.add_argument("--captions", type=Path)
    p.add_argument("--capacity-1080p", type=Path)
    p.add_argument("--capacity-4k", type=Path)
    p.add_argument("--shared-throughput", type=Path)
    p.add_argument(
        "--throughput",
        type=Path,
        help="separate matched FFmpeg trials; do not merge their identities into startup comparisons",
    )
    p.add_argument("--output", type=Path, required=True)
    args = p.parse_args()
    a = read_results(args.baseline)
    b = read_results(args.candidate)
    optional = lambda path: (
        json.loads(path.read_text()) if path and path.exists() else None
    )
    report = dict(
        comparison=compare(a, b),
        qualification=acceptance(
            json.loads(args.corpus.read_text()),
            read_results(args.browser),
            b,
            optional(args.concurrency),
            optional(args.soak),
            read_results(args.throughput) if args.throughput else None,
            optional(args.live_concurrency),
            optional(args.network),
            read_results(args.browser_startup) if args.browser_startup else None,
            read_results(args.timing) if args.timing else None,
            optional(args.quality),
            read_results(args.captions) if args.captions else None,
            optional(args.capacity_1080p),
            optional(args.capacity_4k),
            read_results(args.shared_throughput) if args.shared_throughput else None,
        ),
    )
    if args.baseline_browser_startup and args.browser_startup:
        report["backend_browser_comparison"] = compare(
            read_results(args.baseline_browser_startup),
            read_results(args.browser_startup),
        )
        report["qualification"]["gates"]["startup_regression_review"] = (
            report["backend_browser_comparison"]["regressions"] == 0
            and report["backend_browser_comparison"]["missing"] == 0
        )
        before_environment = (
            optional(args.baseline_browser_startup / "campaign.json") or {}
        )
        after_environment = optional(args.browser_startup / "campaign.json") or {}
        report["qualification"]["gates"]["matched_current_app_environment"] = (
            before_environment.get("driver") == after_environment.get("driver") == "kmp"
            and all(
                before_environment.get(key) is not None
                and before_environment.get(key) == after_environment.get(key)
                for key in (
                    "browser_bundle_sha256",
                    "backend_sha256",
                    "browser_executable_sha256",
                    "browser_automation_sha256",
                )
            )
        )
    else:
        report["qualification"]["gates"]["startup_regression_review"] = False
        report["qualification"]["gates"]["matched_current_app_environment"] = False
    browser_summary = optional(args.browser / "summary.json") or {}
    report["qualification"]["gates"]["browser_build_integrity"] = all(
        browser_summary.get(key) is True
        for key in (
            "browser_binary_unchanged",
            "browser_bundle_unchanged",
            "browser_automation_unchanged",
        )
    )
    browser_environment = optional(args.browser / "campaign.json") or {}
    observer_rows = read_results(args.browser)
    report["qualification"]["gates"]["browser_driver_integrity"] = bool(
        observer_rows
    ) and all(
        row.get("observer_sha256")
        and all(
            browser_environment.get("harness_sha256", {}).get(name) == checksum
            for name, checksum in row["observer_sha256"].items()
        )
        for row in observer_rows
    )
    report["qualification"]["accepted"] = all(report["qualification"]["gates"].values())
    fixtures = {i["id"]: i for i in json.loads(args.corpus.read_text())["fixtures"]}
    failures = []
    for row in read_results(args.browser):
        if row.get("status") != "passed":
            item = fixtures.get(row["fixture"], {})
            failures.append(
                dict(
                    fixture=row["fixture"],
                    container=item.get("container"),
                    video=item.get("video"),
                    audio=item.get("audio"),
                    status=row.get("status"),
                    failures=row.get("failures", [row.get("failure", "unspecified")]),
                )
            )
    report["format_failures"] = failures
    with args.output.with_suffix(".failures.csv").open("w") as output:
        writer = csv.DictWriter(
            output,
            fieldnames=["fixture", "container", "video", "audio", "status", "failures"],
        )
        writer.writeheader()
        writer.writerows(failures)
    write_json(args.output, report)
    c = report["comparison"]
    lines = [
        "# Gateway media comparison",
        "",
        f"Paired successful trials: {c['paired_successes']}; baseline failures: {c['baseline_failures']}; candidate failures: {c['candidate_failures']}; missing pairs: {c['missing']}.",
        "",
        "| Metric | Baseline | Candidate |",
        "|---|---:|---:|",
    ]
    for name in ("p50", "p95"):
        lines.append(
            f"| Startup {name} (ms) | {c['baseline_' + name + '_ms']} | {c['candidate_' + name + '_ms']} |"
        )
    lines += [
        "",
        f"Regressions exceeding both 10% and 100 ms: {c['regressions']}.",
        "",
        "Qualification gates:",
    ]
    lines += [
        f"- {name}: {'passed' if passed else 'not satisfied'}"
        for name, passed in report["qualification"]["gates"].items()
    ]
    args.output.with_suffix(".md").write_text("\n".join(lines) + "\n")
    print(json.dumps({k: c[k] for k in c if k != "pairs"}))
    raise SystemExit(0 if report["qualification"]["accepted"] else 2)


if __name__ == "__main__":
    main()
