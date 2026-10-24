#!/usr/bin/env python3
"""Real HTTP admission, decoding, renewals, sharing and release under sustained load."""

import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import random
import subprocess
import threading
import time
import uuid
import hashlib
from corpus import write_json, digest
from lab import gateway, process_usage
from measure import Sample, settle


def batch(args, fixtures, count, mode, index):
    path = args.output / f"batch-{index:05d}-{mode}-{count}.json"
    if path.exists():
        return json.loads(path.read_text())
    sessions = []
    renewals = {}
    renew_failures = {}
    lock = threading.Lock()
    results = []
    source_origin = json.loads((args.run / "fixture.json").read_text())["source_origin"]
    ffmpeg = args.consumer_ffmpeg or os.environ.get(
        "LAB_FFMPEG", "/opt/ffmpeg/bin/ffmpeg"
    )
    stop = threading.Event()
    first_ready = threading.Event()
    first_done = threading.Event()
    decoders = set()
    consumer_samples = []

    def observe_consumers():
        while not stop.wait(1):
            with lock:
                active = list(decoders)
            consumer_samples.append(
                dict(
                    wall_ns=time.time_ns(),
                    processes=[row for pid in active if (row := process_usage(pid))],
                )
            )

    def renew():
        while not stop.wait(15):
            with lock:
                active = list(sessions)
            for session in active:
                try:
                    updated = gateway(
                        args.run, "POST", "/v1/sessions/" + session["id"] + "/renew"
                    )
                    if updated["status"] not in ("starting", "ready"):
                        raise RuntimeError("renewal_not_ready")
                    with lock:
                        renewals[session["id"]] = renewals.get(session["id"], 0) + 1
                except Exception:
                    with lock:
                        if any(s["id"] == session["id"] for s in sessions):
                            renew_failures[session["id"]] = (
                                renew_failures.get(session["id"], 0) + 1
                            )

    def viewer(n):
        fixture = (
            fixtures[n % len(fixtures)] if mode == "different-sources" else fixtures[0]
        )
        output = dict(
            protocol="hls",
            video_codecs=["h264"],
            audio_codecs=["aac"],
            max_width=1920,
            max_height=1080,
        )
        if mode == "different-plans":
            height = (
                [1080, 720, 576, 480, 360, 288, 240, 180]
                if args.workload != "formats"
                else [360, 320, 288, 256, 224, 192, 160, 128]
            )[n]
            output.update(max_height=height, max_width=(height * 16 // 9) // 2 * 2)
        source = source_origin + "/" + fixture["file"]
        if fixture.get("live"):
            source += (
                "&campaign=" if "?" in source else "?campaign="
            ) + hashlib.sha256(str(args.output).encode()).hexdigest()[:16]
        body = dict(
            namespace="fixture",
            input=dict(url=source, live=bool(fixture.get("live"))),
            output=output,
            position_seconds=0,
        )
        result = dict(viewer=n, fixture=fixture["id"], status="failed")
        session = None
        decoder = None
        try:
            if mode == "late" and n:
                deadline = time.monotonic() + 60
                while not first_ready.wait(0.1):
                    if first_done.is_set() or time.monotonic() > deadline:
                        raise RuntimeError("initial_viewer_not_ready")
                time.sleep(5)
                result["late_join_after_ready_seconds"] = 5
            started = time.monotonic()
            session = gateway(args.run, "POST", "/v1/sessions", body, uuid.uuid4().hex)
            with lock:
                sessions.append(session)
            deadline = started + 45
            while session["status"] == "starting" and time.monotonic() < deadline:
                time.sleep(0.02)
                session = gateway(args.run, "GET", "/v1/sessions/" + session["id"])
            if session["status"] != "ready":
                raise RuntimeError("not_ready")
            result.update(
                startup_ms=(time.monotonic() - started) * 1000, job_id=session["job_id"]
            )
            if n == 0:
                first_ready.set()
            url = (
                args.consumer_origin.rstrip("/") + "/gateway"
                if args.consumer_origin
                else "http://127.0.0.1:8090"
            ) + session["playback"]["url"]
            seconds = (
                args.seconds
                if fixture.get("live")
                else min(args.seconds, fixture["duration"] - 1)
            )
            with (args.output / f"batch-{index:05d}-viewer-{n}.log").open("wb") as log:
                # -re paces the decoder: this is a sustained viewer, not a fast download.
                decoder = subprocess.Popen(
                    [
                        ffmpeg,
                        "-nostdin",
                        "-v",
                        "error",
                        "-xerror",
                        "-re",
                        *(["-tls_verify", "1"] if args.consumer_origin else []),
                        "-threads",
                        "2",
                        "-live_start_index",
                        "-3" if fixture.get("live") else "0",
                        "-i",
                        url,
                        "-t",
                        str(seconds),
                        "-map",
                        "0:v:0",
                        "-map",
                        "0:a:0?",
                        "-progress",
                        "pipe:1",
                        "-f",
                        "null",
                        "-",
                    ],
                    stdout=subprocess.PIPE,
                    stderr=log,
                    text=True,
                )
                with lock:
                    decoders.add(decoder.pid)
                progress, _ = decoder.communicate(timeout=seconds + 60)
                if decoder.returncode:
                    raise RuntimeError("decoder_failed")
            timestamps = [
                int(l.split("=", 1)[1])
                for l in progress.splitlines()
                if l.startswith("out_time_us=") and l.split("=", 1)[1].isdigit()
            ]
            result["decoded_seconds"] = max(timestamps, default=0) / 1e6
            if result["decoded_seconds"] < seconds * 0.95:
                raise RuntimeError("short_decode")
            if (
                gateway(args.run, "GET", "/v1/sessions/" + session["id"])["status"]
                != "ready"
            ):
                raise RuntimeError("lease_not_ready")
            result["status"] = "passed"
        except (Exception,) as e:
            result["failure"] = (
                str(e)[:300] if isinstance(e, RuntimeError) else type(e).__name__
            )
        finally:
            if n == 0:
                first_done.set()
            if decoder and decoder.poll() is None:
                decoder.kill()
                decoder.wait()
            if decoder:
                with lock:
                    decoders.discard(decoder.pid)
            if session:
                with lock:
                    result["renewals"] = renewals.get(session["id"], 0)
                    result["renewal_failures"] = renew_failures.get(session["id"], 0)
                    if result["renewal_failures"]:
                        result.update(status="failed", failure="renewal_failed")
                    sessions[:] = [s for s in sessions if s["id"] != session["id"]]
                try:
                    gateway(args.run, "DELETE", "/v1/sessions/" + session["id"])
                except Exception:
                    result.update(status="failed", failure="release_failed")
        return result

    worker = threading.Thread(target=renew, daemon=True)
    consumer_observer = threading.Thread(target=observe_consumers, daemon=True)
    worker.start()
    consumer_observer.start()
    try:
        if args.observer_config:
            from remote_sample import RemoteSample

            observation = RemoteSample(
                args.observer_config, args.output.name + "/" + path.stem
            )
        else:
            observation = Sample(args.run)
        with (
            observation as samples,
            concurrent.futures.ThreadPoolExecutor(max_workers=count) as pool,
        ):
            results = list(pool.map(viewer, range(count)))
    finally:
        stop.set()
        worker.join()
        consumer_observer.join()
    jobs = {r.get("job_id") for r in results if r.get("job_id")}
    sharing = not (mode in ("same", "late") and len(jobs) != 1)
    upstream = {
        pid
        for pid, argv in samples.commands.items()
        if Path(argv[0]).name == "ffmpeg"
        and any(arg.startswith(source_origin + "/") for arg in argv)
    }
    expected_inputs = min(count, len(fixtures)) if mode == "different-sources" else 1
    if args.live:
        sharing = sharing and len(upstream) == expected_inputs
    result = dict(
        index=index,
        mode=mode,
        viewers=count,
        results=results,
        sharing_correct=sharing,
        upstream_ffmpeg_processes=len(upstream),
        expected_upstream_inputs=expected_inputs if args.live else None,
        capacity_reclaimed=settle(args.run),
    )
    result["status"] = (
        "passed"
        if sharing
        and result["capacity_reclaimed"]
        and all(r["status"] == "passed" for r in results)
        else "failed"
    )
    write_json(path, result)
    if args.observer_config:
        result["server_resources_file"] = samples.resources_file
        result["consumer_location"] = "independent workstation; trusted HTTPS ingress"
        write_json(path, result)
    else:
        write_json(path.with_suffix(".resources.json"), samples.rows)
    write_json(path.with_suffix(".consumer-resources.json"), consumer_samples)
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--run", type=Path, default=Path("/run/lab"))
    p.add_argument("--corpus", type=Path, default=Path("/corpus"))
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--seconds", type=int, default=60)
    p.add_argument("--soak-hours", type=float, default=0)
    p.add_argument("--live", action="store_true")
    p.add_argument("--live-manifest", type=Path)
    p.add_argument(
        "--consumer-origin",
        help="trusted HTTPS ingress for independent decoding clients",
    )
    p.add_argument("--consumer-ffmpeg", help="pinned client FFmpeg binary")
    p.add_argument(
        "--observer-config", type=Path, help="private server observer configuration"
    )
    p.add_argument("--workload", choices=["formats", "1080p", "4k"], default="formats")
    args = p.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    if args.soak_hours and (args.output / "summary.json").exists():
        raise SystemExit(
            "continuous soak cannot resume across an interruption; use a new output directory"
        )
    config = dict(
        manifest_sha256=digest(args.corpus / "manifest.json"),
        gateway=json.loads((args.run / "provenance.json").read_text()),
        seconds=args.seconds,
        soak_hours=args.soak_hours,
        live=args.live,
        live_manifest_sha256=digest(
            args.live_manifest or args.corpus / "live-manifest.json"
        )
        if args.live
        else None,
        workload=args.workload,
        consumer_origin=args.consumer_origin,
        consumer_ffmpeg_sha256=digest(Path(args.consumer_ffmpeg))
        if args.consumer_ffmpeg
        else None,
        harness_sha256={
            name: digest(Path(__file__).parent / name)
            for name in ("load.py", "measure.py", "lab.py")
        },
    )
    frozen = args.output / "campaign.json"
    if frozen.exists():
        assert json.loads(frozen.read_text()) == config, (
            "load configuration changed; use a new directory"
        )
    else:
        write_json(frozen, config)
    fixtures = json.loads((args.corpus / "manifest.json").read_text())["fixtures"]
    fixtures = [
        f
        for f in fixtures
        if f["variant"] == ({"formats": 0, "1080p": 20, "4k": 22}[args.workload])
        and (args.corpus / f["id"] / "receipt.json").exists()
    ]
    if args.live:
        fixtures = json.loads(
            (args.live_manifest or args.corpus / "live-manifest.json").read_text()
        )["fixtures"]
    assert bool(args.consumer_origin) == bool(args.observer_config), (
        "remote consumers require server-side observation"
    )
    assert fixtures
    results = []
    started = time.monotonic()
    index = 0
    schedules = [
        (n, mode)
        for n in (1, 2, 4, 8)
        for mode in ("same", "different-plans", "different-sources", "late")
    ]
    random.Random(20261023).shuffle(schedules)
    while index < len(schedules) or time.monotonic() - started < args.soak_hours * 3600:
        n, mode = schedules[index % len(schedules)]
        result = batch(args, fixtures, n, mode, index)
        results.append(result)
        index += 1
        summary = dict(
            planned_batches=max(len(schedules), index),
            elapsed_seconds=time.monotonic() - started,
            soak_target_seconds=args.soak_hours * 3600,
            passed=sum(r["status"] == "passed" for r in results),
            failed=sum(r["status"] != "passed" for r in results),
            completed=index,
            complete=index >= len(schedules)
            and time.monotonic() - started >= args.soak_hours * 3600,
        )
        write_json(args.output / "summary.json", summary)
        print(
            json.dumps({k: result[k] for k in ("index", "mode", "viewers", "status")}),
            flush=True,
        )
    raise SystemExit(any(r["status"] != "passed" for r in results))


if __name__ == "__main__":
    main()
