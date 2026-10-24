#!/usr/bin/env python3
"""Real gateway/decoder baseline inside the owned lab network namespace."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import re
import shutil
import subprocess
import threading
import time
import uuid
from corpus import digest, write_json, representatives
from lab import gateway
from readiness import published_buffer
from urllib.parse import urlsplit


def processes(parent):
    children = set()
    for task in Path(f"/proc/{parent}/task").glob("*"):
        try:
            children.update(int(p) for p in (task / "children").read_text().split())
        except OSError:
            pass
    return children


class Sample:
    def __init__(self, run, live=False):
        self.pid = json.loads((run / "fixture.json").read_text())["gateway_pid"]
        self.rows = []
        self.commands = {}
        self.files = {}
        self.playable = {}
        self.live = live
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.sample, daemon=True)

    def sample(self):
        tick = 0
        while not self.stop.wait(0.01):
            tick += 1
            now = time.time_ns()
            row = {"time_ns": now, "processes": []}
            for pid in {self.pid} | processes(self.pid):
                try:
                    status = Path(f"/proc/{pid}/stat").read_text().split()
                    cmd = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
                    argv = [x.decode() for x in cmd if x]
                    if argv and Path(argv[0]).name in ("ffmpeg", "ffprobe"):
                        self.commands.setdefault(pid, argv)
                    process = dict(
                        pid=pid,
                        kind=Path(argv[0]).name if argv else "unknown",
                        cpu_ticks=int(status[13]) + int(status[14]),
                        reaped_child_cpu_ticks=int(status[15]) + int(status[16]),
                        rss_bytes=int(status[23]) * os.sysconf("SC_PAGE_SIZE"),
                    )
                    if tick % 100 == 1:
                        try:
                            process["io"] = {
                                k: int(value.strip())
                                for k, value in (
                                    line.split(":", 1)
                                    for line in Path(f"/proc/{pid}/io")
                                    .read_text()
                                    .splitlines()
                                )
                                if k in ("read_bytes", "write_bytes", "syscr", "syscw")
                            }
                        except (OSError, ValueError):
                            process["io"] = None
                        clients = {}
                        try:
                            for descriptor in Path(f"/proc/{pid}/fdinfo").iterdir():
                                fields = dict(
                                    line.split(":", 1)
                                    for line in descriptor.read_text().splitlines()
                                    if line.startswith("drm-") and ":" in line
                                )
                                if fields.get("drm-client-id"):
                                    clients[fields["drm-client-id"].strip()] = {
                                        k: v.strip()
                                        for k, v in fields.items()
                                        if k.startswith(("drm-engine-", "drm-memory-"))
                                        or k == "drm-driver"
                                    }
                            process["gpu_clients"] = clients
                            process["gpu_telemetry"] = (
                                "available" if clients else "no_drm_counters"
                            )
                        except OSError:
                            process["gpu_telemetry"] = "unavailable"
                    row["processes"].append(process)
                except (OSError, ValueError, IndexError):
                    pass
            for file in Path("/state/media").rglob("*"):
                try:
                    if file.is_file() and file.suffix in (
                        ".ts",
                        ".m4s",
                        ".m3u8",
                        ".mp4",
                    ):
                        info = file.stat()
                        if info.st_size:
                            self.files.setdefault(
                                str(file),
                                {
                                    "mtime_ns": info.st_mtime_ns,
                                    "seen_ns": now,
                                    "size": info.st_size,
                                },
                            )
                            if (
                                file.suffix == ".m3u8"
                                and str(file) not in self.playable
                            ):
                                duration = published_buffer(file, self.live)
                                if duration is not None:
                                    self.playable[str(file)] = dict(
                                        mtime_ns=info.st_mtime_ns,
                                        seen_ns=now,
                                        media_seconds=duration,
                                    )
                except OSError:
                    pass
            self.rows.append(row)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.stop.set()
        self.thread.join()


def settle(run, deadline=45):
    until = time.monotonic() + deadline
    while time.monotonic() < until:
        if gateway(run, "GET", "/v1/capabilities")["available"] == dict(
            inputs=8, outputs=8, viewers=64
        ):
            return True
        time.sleep(0.1)
    return False


def serial_overhead(samples, started_ns, ready_ns, publication_ns):
    """Upper bound outside observed probe/producer work, not transport attribution.

    Sampling underestimates subprocess lifetime, making this conservative. A
    matched raw pipeline is separately required to detect gateway transport
    delays while FFmpeg is alive. Never double-count overlapping subprocesses.
    """
    bounds = {}
    for row in samples.rows:
        if not started_ns <= row["time_ns"] <= ready_ns:
            continue
        for process in row["processes"]:
            if process["kind"] not in ("ffprobe", "ffmpeg"):
                continue
            lo, hi, kind = bounds.get(
                process["pid"], (row["time_ns"], row["time_ns"], process["kind"])
            )
            bounds[process["pid"]] = (lo, row["time_ns"], kind)
    intervals = []
    for lo, hi, kind in bounds.values():
        end = publication_ns if kind == "ffmpeg" else hi
        if end is not None and end > lo:
            intervals.append((lo, min(end, ready_ns)))
    occupied = 0
    last = started_ns
    for lo, hi in sorted(intervals):
        occupied += max(0, hi - max(lo, last))
        last = max(last, hi)
    return max(0, ready_ns - started_ns - occupied) / 1e6


def observed_inspection(commands, candidates, log, trace):
    local_inspection = None
    inspection_observation = "process_argv_sample"
    for argv in commands:
        if (
            Path(argv[0]).name == "ffprobe"
            and "-i" in argv
            and argv[argv.index("-i") + 1] in {c[-1] for c in candidates}
        ):
            local_inspection = argv
            break
    if local_inspection is None:
        ingresses = [
            c
            for c in candidates
            if c[c.index("-i") + 1].startswith(("http://", "https://"))
        ]
        if len(ingresses) == 1:
            playlist = ingresses[0][-1]
            for line in log:
                if (
                    f"trace={trace} " not in line
                    or 'inspection_profile="local_replay_streams_v1"' not in line
                    or f'ingest_id="{Path(playlist).parent.name}"' not in line
                ):
                    continue
                program = re.search(r"\binspection_program=(\S+)", line)
                extensions = re.search(
                    r'\ballowed_extensions="(ts,m3u8|mkv,m3u8)"', line
                )
                if program and extensions:
                    local_inspection = [
                        program[1],
                        "-v",
                        "error",
                        "-protocol_whitelist",
                        "file",
                        "-allowed_extensions",
                        extensions[1],
                        "-show_streams",
                        "-of",
                        "json",
                        "-i",
                        playlist,
                    ]
                    inspection_observation = "versioned_local_launch_event"
                    break
    return local_inspection, inspection_observation


def one(args, fixture, trial, cache):
    run = args.run
    output = args.output
    identity = f"{fixture['id']}-{cache}-{trial}"
    target = output / (identity + ".json")
    if target.exists():
        return json.loads(target.read_text())
    receipt = args.corpus / fixture["id"] / "receipt.json"
    result = dict(
        fixture=fixture["id"],
        trial=trial,
        cache=cache,
        status="failed",
        profile=args.profile,
        manifest_sha256=digest(args.corpus / "manifest.json"),
        binary=json.loads((run / "provenance.json").read_text())["binary_sha256"],
    )
    if not receipt.exists() or json.loads(receipt.read_text())["status"] != "valid":
        result.update(status="not_run", failure="fixture_not_validated")
        return result
    result["source_receipt_sha256"] = digest(receipt)
    source = (
        json.loads((run / "fixture.json").read_text())["source_origin"]
        + "/"
        + fixture["file"]
    )
    # Distinct benign query identity is a cold gateway probe key. This does not
    # claim to evict OS page caches, DNS or TLS session caches.
    source += (
        ("&trial=" if "?" in source else "?trial=")
        + hashlib.sha256(str(output.resolve()).encode()).hexdigest()[:12]
        + "-"
        + str(trial)
    )
    output_profile = dict(
        protocol="hls",
        video_codecs=["h264"],
        audio_codecs=["aac"],
        max_width=1920,
        max_height=1080,
    )
    if args.profile == "roku-hevc":
        output_profile.update(
            video_codecs=["h264", "hevc"], max_width=3840, max_height=2160
        )
    if args.conversion:
        output_profile["conversion"] = args.conversion
    body = dict(
        namespace="fixture",
        input=dict(url=source, live=bool(fixture.get("live"))),
        output=output_profile,
        position_seconds=0,
    )
    session = None
    request_identity = uuid.uuid4().hex
    trace = str(
        int.from_bytes(hashlib.sha256(request_identity.encode()).digest()[:8], "big")
    )
    try:
        started = time.monotonic_ns()
        wall = time.time_ns()
        with Sample(run, live=bool(fixture.get("live"))) as samples:
            session = gateway(run, "POST", "/v1/sessions", body, request_identity)
            while session["status"] == "starting":
                time.sleep(0.02)
                session = gateway(run, "GET", "/v1/sessions/" + session["id"])
            result["ready_ms"] = (time.monotonic_ns() - started) / 1e6
            result["ready_wall_ns"] = time.time_ns()
            if session["status"] != "ready":
                raise RuntimeError(session.get("error_code") or "not_ready")
            playback = session["playback"]
            result["delivery"] = {
                k: playback.get(k)
                for k in (
                    "mode",
                    "format",
                    "video_mode",
                    "audio_mode",
                    "width",
                    "height",
                )
            }
            url = "http://127.0.0.1:8090" + playback["url"]
            ffmpeg = os.environ.get("LAB_FFMPEG", "/opt/ffmpeg/bin/ffmpeg")
            decode_started = time.monotonic_ns()
            with (output / (identity + ".decode.log")).open("wb") as log:
                decoder = subprocess.Popen(
                    [
                        ffmpeg,
                        "-nostdin",
                        "-v",
                        "error",
                        "-xerror",
                        "-threads",
                        "2",
                        "-live_start_index",
                        "0",
                        "-i",
                        url,
                        "-t",
                        str(args.seconds),
                        "-map",
                        "0:v:0",
                        "-map",
                        "0:a:0?",
                        *(["-c", "copy"] if args.consumer_mode == "copy" else []),
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

                def renew():
                    while not renew_stop.wait(15):
                        try:
                            gateway(
                                run, "POST", "/v1/sessions/" + session["id"] + "/renew"
                            )
                        except Exception:
                            break

                renew_stop = threading.Event()
                worker = threading.Thread(target=renew, daemon=True)
                worker.start()
                try:
                    progress, _ = decoder.communicate(timeout=args.seconds + 60)
                except subprocess.TimeoutExpired:
                    decoder.kill()
                    decoder.communicate()
                    raise
                finally:
                    renew_stop.set()
                    worker.join()
                if decoder.returncode:
                    raise RuntimeError("output_decode_failed")
            result["decode_wall_ms"] = (time.monotonic_ns() - decode_started) / 1e6
            positions = [
                int(line.split("=", 1)[1])
                for line in progress.splitlines()
                if line.startswith("out_time_us=") and line.split("=", 1)[1].isdigit()
            ]
            result["consumer_mode"] = args.consumer_mode
            result["media_seconds"] = max(positions, default=0) / 1e6
            result[
                "copied_seconds" if args.consumer_mode == "copy" else "decoded_seconds"
            ] = result["media_seconds"]
            if (
                result["media_seconds"]
                < min(args.seconds, fixture["duration"] - 1) * 0.95
            ):
                raise RuntimeError("short_decode")
        write_json(output / (identity + ".resources.json"), samples.rows)
        # Raw argv is private because real gateways can contain source capabilities.
        write_json(output / (identity + ".private-commands.json"), samples.commands)
        result["commands_sha256"] = digest(
            output / (identity + ".private-commands.json")
        )
        result["processes"] = [
            dict(
                kind=Path(a[0]).name,
                encoder=next(
                    (
                        a[i + 1]
                        for i in range(len(a) - 2, -1, -1)
                        if a[i] in ("-c:v", "-vcodec")
                    ),
                    None,
                ),
            )
            for a in samples.commands.values()
        ]
        result["cache_observed"] = (
            "probe_process"
            if any(p["kind"] == "ffprobe" for p in result["processes"])
            else "no_probe_process_observed"
        )
        producer_commands = [
            a
            for a in samples.commands.values()
            if Path(a[0]).name == "ffmpeg" and a[-1].endswith(".m3u8")
        ]
        inputs = {a[a.index("-i") + 1] for a in producer_commands if "-i" in a}
        terminal = [a[-1] for a in producer_commands if a[-1] not in inputs]
        gateway_log = (run / "gateway.log").read_text().splitlines()
        pipe_outputs = [
            a for a in producer_commands if a[a.index("-i") + 1] == "pipe:0"
        ]
        ingress = [
            a
            for a in producer_commands
            if a[a.index("-i") + 1].startswith(("https://", "http://"))
        ]
        pipe_verified = (
            len(pipe_outputs) == 1
            and len(ingress) == 1
            and any(
                "pipe_replay=true" in line
                and "output_id=" + Path(pipe_outputs[0][-1]).parent.name in line
                and re.search(
                    r'\bingest_id="?'
                    + re.escape(Path(ingress[0][-1]).parent.name)
                    + r'"?(?: |$)',
                    line,
                )
                for line in gateway_log
            )
        )
        if pipe_verified:
            terminal = [pipe_outputs[0][-1]]
        result["shared_pipe_graph_verified"] = bool(pipe_verified)
        public_playlist = terminal[0] if len(terminal) == 1 else None
        new = [
            v["mtime_ns"]
            for p, v in samples.files.items()
            if p == public_playlist and wall <= v["mtime_ns"] <= result["ready_wall_ns"]
        ]
        # The final playlist mtime can advance after readiness: do not label it a startup measurement.
        result["playlist_to_response_ms"] = (
            (result["ready_wall_ns"] - min(new)) / 1e6 if new else None
        )
        playable = [
            v["mtime_ns"]
            for path, v in samples.playable.items()
            if path == public_playlist
            and wall <= v["mtime_ns"] <= result["ready_wall_ns"]
        ]
        for line in gateway_log:
            if f"trace={trace} " not in line or "Playback output detected" not in line:
                continue
            published = re.search(r"\bpublished_wall_ns=(\d+)", line)
            detected = re.search(r"\bdetected_wall_ns=(\d+)", line)
            output_id = re.search(r"\boutput_id=(\S+)", line)
            if (
                published
                and detected
                and output_id
                and public_playlist
                and output_id[1] == Path(public_playlist).parent.name
                and wall
                <= int(published[1])
                <= int(detected[1])
                <= result["ready_wall_ns"]
            ):
                playable.append(int(published[1]))
                result["ready_detection_ms"] = (
                    int(detected[1]) - int(published[1])
                ) / 1e6
                result["readiness_clock"] = "validated_published_inode_event"
        result["playable_to_response_ms"] = (
            (result["ready_wall_ns"] - min(playable)) / 1e6 if playable else None
        )
        result["initial_buffer_scope"] = (
            "two delivered reload intervals for live, one for VOD; or completed finite media"
        )
        result["exclusive_overhead_ms"] = (
            serial_overhead(samples, wall, result["ready_wall_ns"], min(playable))
            if playable
            else None
        )
        ffmpeg_pids = {
            pid
            for pid, argv in samples.commands.items()
            if Path(argv[0]).name == "ffmpeg" and argv[-1].endswith(".m3u8")
        }
        launches = {}
        for line in gateway_log:
            if "Playback producer launched" not in line:
                continue
            pid = re.search(r"\bprocess_id=(\d+)", line)
            stamp = re.search(r"\bstarted_wall_ns=(\d+)", line)
            if (
                pid
                and stamp
                and int(pid[1]) in ffmpeg_pids
                and wall <= int(stamp[1]) <= result["ready_wall_ns"]
            ):
                launches[int(pid[1])] = int(stamp[1])
        starts = [
            launches.get(pid)
            or next(
                (
                    row["time_ns"]
                    for row in samples.rows
                    if any(p["pid"] == pid for p in row["processes"])
                ),
                None,
            )
            for pid in ffmpeg_pids
        ]
        producer_start = min((n for n in starts if n is not None), default=None)
        result["producer_wall_ms"] = (
            (result["ready_wall_ns"] + result["decode_wall_ms"] * 1e6 - producer_start)
            / 1e6
            if producer_start
            else None
        )
        result["producer_clock"] = (
            "launch_event" if ffmpeg_pids <= launches.keys() else "10ms_process_sample"
        )
        result["overhead_scope"] = (
            "conservative sampled serial overhead; parallel transport requires the matched FFmpeg control"
        )
        log_lines = [
            line
            for line in (run / "gateway.log").read_text().splitlines()
            if f"trace={trace} " in line and "Playback preparation completed" in line
        ]
        if log_lines:
            for key in ("probe_ms", "engine_ready_ms"):
                match = re.search(r"\b" + key + r"=(\d+)", log_lines[-1])
                if match:
                    result[key] = int(match[1])
        result["status"] = "passed"
    except (Exception,) as error:
        result["failure"] = (
            str(error)[:300]
            if isinstance(error, RuntimeError)
            else type(error).__name__
        )
    finally:
        if session:
            try:
                gateway(run, "DELETE", "/v1/sessions/" + session["id"])
            except Exception:
                result["release_failed"] = True
        result["capacity_reclaimed"] = settle(run)
        if not result["capacity_reclaimed"]:
            result["status"] = "failed"
        if args.reference and result["status"] == "passed":
            from reference import run_reference

            candidates = [
                a
                for a in samples.commands.values()
                if Path(a[0]).name == "ffmpeg" and a[-1].endswith(".m3u8")
            ]
            local_inspection, inspection_observation = observed_inspection(
                samples.commands.values(), candidates, gateway_log, trace
            )
            source_age = None
            if fixture.get("live") and producer_start:
                query = hashlib.sha256(urlsplit(source).query.encode()).hexdigest()[:16]
                for _ in range(5):
                    rows = []
                    with (run / "origin.jsonl").open() as log:
                        for line in log:
                            try:
                                row = json.loads(line)
                            except ValueError:
                                continue
                            began = (
                                row.get("wall_ns", 0) - row.get("elapsed_ms", 0) * 1e6
                            )
                            if (
                                row.get("query_sha256") == query
                                and row.get("source_elapsed_seconds") is not None
                                and began >= producer_start - 5e6
                            ):
                                rows.append((began, row["source_elapsed_seconds"]))
                    if rows:
                        source_age = min(rows)[1]
                        break
                    time.sleep(0.05)
            result["reference"] = (
                run_reference(
                    candidates[0],
                    source,
                    args.seconds,
                    args.consumer_mode,
                    source_duration=fixture["duration"],
                )
                if len(candidates) == 1
                else run_reference(
                    candidates[0],
                    source,
                    args.seconds,
                    args.consumer_mode,
                    shared=candidates,
                    pipe_verified=bool(result.get("shared_pipe_graph_verified")),
                    source_age=source_age,
                    inspection=local_inspection,
                )
                if len(candidates) == 2 and fixture.get("live")
                else dict(status="not_run", failure="multiple_or_missing_producers")
            )
            if result["reference"]["status"] != "passed":
                result.update(
                    status="failed",
                    failure="matched_reference_" + result["reference"]["status"],
                )
            result["reference"]["inspection_observation"] = (
                inspection_observation if local_inspection else None
            )
        write_json(target, result)
    return result


def main():
    os.umask(0o077)
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--run", type=Path, default=Path("/run/lab"))
    p.add_argument("--corpus", type=Path, default=Path("/corpus"))
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--count", type=int, default=40)
    p.add_argument("--trials", type=int, default=20)
    p.add_argument("--seconds", type=int, default=3)
    p.add_argument(
        "--profile", choices=["browser", "roku", "roku-hevc"], default="browser"
    )
    p.add_argument("--conversion", choices=["audio", "video", "audio_video"])
    p.add_argument("--ids")
    p.add_argument("--selection", choices=["formats", "diverse"], default="formats")
    p.add_argument("--reference", action="store_true")
    p.add_argument("--consumer-mode", choices=["decode", "copy"], default="decode")
    p.add_argument(
        "--live",
        action="store_true",
        help="qualify owned live inputs and the shared FFmpeg graph",
    )
    p.add_argument("--live-manifest", type=Path)
    args = p.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    fixture_manifest = (
        (args.live_manifest or args.corpus / "live-manifest.json")
        if args.live
        else args.corpus / "manifest.json"
    )
    fixtures = json.loads(fixture_manifest.read_text())["fixtures"]
    fixtures = sorted(fixtures, key=lambda f: (f.get("variant", 0), f["id"]))
    if args.ids:
        fixtures = [f for f in fixtures if f["id"] in args.ids.split(",")]
    fixtures = (
        representatives(fixtures, args.count)
        if args.selection == "diverse"
        else fixtures[: args.count]
    )
    config = dict(
        manifest_sha256=digest(args.corpus / "manifest.json"),
        gateway=json.loads((args.run / "provenance.json").read_text()),
        fixtures=[f["id"] for f in fixtures],
        trials=args.trials,
        seconds=args.seconds,
        profile=args.profile,
        conversion=args.conversion,
        reference=args.reference,
        selection=args.selection,
        consumer_mode=args.consumer_mode,
        live=args.live,
        fixture_manifest_sha256=digest(fixture_manifest),
        harness_sha256={
            name: digest(Path(__file__).parent / name)
            for name in ("measure.py", "lab.py", "reference.py")
        },
    )
    frozen = args.output / "campaign.json"
    if frozen.exists():
        assert json.loads(frozen.read_text()) == config, (
            "campaign configuration changed; use a new output directory"
        )
    else:
        assert not list(args.output.glob("*.json")), (
            "unversioned receipts exist; use a fresh output directory"
        )
        write_json(frozen, config)
    pairs = [(f, n) for f in fixtures for n in range(args.trials)]
    random.Random(20261023).shuffle(pairs)
    jobs = [(f, n, cache) for f, n in pairs for cache in ("cold", "warm")]
    results = []
    for fixture, n, cache in jobs:
        result = one(args, fixture, n, cache)
        results.append(result)
        print(
            json.dumps(
                {
                    k: result.get(k)
                    for k in (
                        "fixture",
                        "trial",
                        "cache",
                        "status",
                        "ready_ms",
                        "failure",
                    )
                }
            ),
            flush=True,
        )
        write_json(
            args.output / "summary.json",
            dict(
                planned=len(jobs),
                completed=len(results),
                passed=sum(r["status"] == "passed" for r in results),
                results=results,
            ),
        )
    raise SystemExit(0 if all(r["status"] == "passed" for r in results) else 1)


if __name__ == "__main__":
    main()
