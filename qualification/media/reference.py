#!/usr/bin/env python3
"""Replay an observed single-input VOD plan through an owned reference HLS origin."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
from urllib.parse import urlsplit, urlunsplit, parse_qsl, urlencode
from corpus import write_json
from origin import Origin
from readiness import published_buffer


def reference_command(argv, source, directory, retain_all=True):
    if argv.count("-i") != 1 or not argv[-1].endswith(".m3u8"):
        raise ValueError("reference requires a single-input HLS file-output plan")
    old_root = str(Path(argv[-1]).parent)
    args = []
    index = 0
    while index < len(argv):
        arg = argv[index]
        if arg == "-http_proxy":
            index += 2
            continue
        if retain_all and arg == "-hls_list_size":
            args.extend([arg, "0"])
            index += 2
            continue
        if retain_all and arg == "-hls_flags":
            flags = [f for f in argv[index + 1].split("+") if f != "delete_segments"]
            if flags:
                args.extend([arg, "+".join(flags)])
            index += 2
            continue
        if arg == "-i":
            args.extend([arg, source])
            index += 2
            continue
        args.append(arg.replace(old_root, str(directory)))
        index += 1
    return args


def shared_graph(commands, source, directory, pipe_verified=False):
    """Only accept a proven two-stage file graph; never guess producer order."""
    if len(commands) != 2 or any(
        a.count("-i") != 1 or not a[-1].endswith(".m3u8") for a in commands
    ):
        raise ValueError(
            "shared reference requires exactly two single-input playlist producers"
        )
    edges = [
        (a, b)
        for a in commands
        for b in commands
        if a is not b and b[b.index("-i") + 1] == a[-1]
    ]
    if not edges and pipe_verified:
        edges = [
            (a, b)
            for a in commands
            for b in commands
            if a is not b
            and b[b.index("-i") + 1] == "pipe:0"
            and a[a.index("-i") + 1].startswith(("https://", "http://"))
        ]
    if len(edges) != 1:
        raise ValueError("shared reference graph is not one ingest feeding one output")
    ingest, output = edges[0]
    if not ingest[ingest.index("-i") + 1].startswith(("https://", "http://")):
        raise ValueError("shared reference ingress is not a verified HTTP input")
    ingest_dir = directory / "ingest"
    output_dir = directory / "output"
    ingest_dir.mkdir()
    output_dir.mkdir()
    first = reference_command(ingest, source, ingest_dir, retain_all=False)
    second = reference_command(
        output,
        "pipe:0" if output[output.index("-i") + 1] == "pipe:0" else first[-1],
        output_dir,
        retain_all=False,
    )
    return first, second


def run_reference(
    argv,
    source,
    seconds,
    consumer_mode="decode",
    shared=None,
    pipe_verified=False,
    source_age=None,
    inspection=None,
    source_duration=None,
):
    scratch = (
        "/state"
        if os.environ.get("MEDIA_LAB_ISOLATED") == "1" and Path("/state").is_dir()
        else None
    )
    with tempfile.TemporaryDirectory(prefix="media-reference-", dir=scratch) as tmp:
        directory = Path(tmp)
        first = None
        try:
            if shared is not None:
                if source_age is not None:
                    parsed = urlsplit(source)
                    query = [(k, v) for k, v in parse_qsl(parsed.query) if k != "age"]
                    query.append(("age", str(source_age)))
                    source = urlunsplit(parsed._replace(query=urlencode(query)))
                source += (
                    ("&" if "?" in source else "?") + "reference=" + directory.name
                )
                first, command = shared_graph(shared, source, directory, pipe_verified)
            else:
                command = reference_command(argv, source, directory)
                command[-1:-1] = [
                    "-t",
                    str(min(source_duration or seconds + 4, seconds + 6)),
                ]
        except ValueError as error:
            return dict(status="not_run", failure=str(error))
        origin = Origin(("127.0.0.1", 0), directory, None)
        thread = threading.Thread(target=origin.serve_forever, daemon=True)
        thread.start()
        producer = None
        ingest = None
        feeder = None
        feed_stop = threading.Event()
        feed_failures = []
        consumer = None
        result = {
            "status": "failed",
            "scope": "single-input finite VOD; identical codec/filter/segment settings and decoder; retain all reference segments to prevent a fast producer evicting before decoder startup",
        }
        if shared is not None:
            result["shared_graph"] = True
            result["source_age_aligned_seconds"] = source_age
            result["scope"] = (
                "shared live ingest and one output; identical two-stage FFmpeg file graph, decoder and publication settings; gateway metadata validation is measured separately"
            )
        try:
            with (directory / "producer.log").open("wb") as log:
                started = time.monotonic()
                producer_started = None
                if first:
                    ingest = subprocess.Popen(
                        first, stdin=subprocess.DEVNULL, stdout=log, stderr=log
                    )
                    producer_started = time.monotonic()
                    ingress = Path(first[-1])
                    deadline = started + 45
                    while time.monotonic() < deadline:
                        if ingress.exists():
                            files = [
                                line
                                for line in ingress.read_text().splitlines()
                                if line and not line.startswith("#")
                            ]
                            if files and all(
                                (ingress.parent / f).is_file()
                                and (ingress.parent / f).stat().st_size
                                for f in files
                            ):
                                break
                        if ingest.poll() is not None:
                            raise RuntimeError("reference_ingest_failed")
                        time.sleep(0.005)
                    else:
                        raise TimeoutError("reference_ingest_readiness")
                    if inspection is None:
                        raise RuntimeError("reference_replay_inspection_not_observed")
                    probe = inspection.copy()
                    probe[probe.index("-i") + 1] = first[-1]
                    checked = subprocess.run(
                        probe,
                        stdin=subprocess.DEVNULL,
                        stdout=subprocess.PIPE,
                        stderr=log,
                        timeout=8,
                    )
                    if checked.returncode:
                        raise RuntimeError("reference_replay_inspection_failed")
                producer = subprocess.Popen(
                    command,
                    stdin=subprocess.PIPE if pipe_verified else subprocess.DEVNULL,
                    stdout=log,
                    stderr=log,
                )
                producer_started = producer_started or time.monotonic()
                if pipe_verified:

                    def feed_ts():
                        import re

                        next_number = None
                        try:
                            while not feed_stop.is_set():
                                text = Path(first[-1]).read_text()
                                files = [
                                    line
                                    for line in text.splitlines()
                                    if line and not line.startswith("#")
                                ]
                                if (
                                    not files
                                    or len(files) > 12
                                    or any(
                                        not re.fullmatch(r"segment-\d{9}\.ts", f)
                                        for f in files
                                    )
                                ):
                                    raise ValueError("reference_replay_layout")
                                numbers = [int(f[8:-3]) for f in files]
                                if numbers != list(range(numbers[0], numbers[-1] + 1)):
                                    raise ValueError("reference_replay_gap")
                                if next_number is None:
                                    next_number = numbers[max(0, len(numbers) - 3)]
                                if next_number < numbers[0]:
                                    raise ValueError("reference_replay_evicted")
                                while next_number <= numbers[-1]:
                                    with (
                                        Path(first[-1]).parent
                                        / f"segment-{next_number:09}.ts"
                                    ).open("rb") as media:
                                        while block := media.read(64 * 1024):
                                            producer.stdin.write(block)
                                    next_number += 1
                                producer.stdin.flush()
                                feed_stop.wait(0.005)
                        except (OSError, ValueError) as error:
                            if not feed_stop.is_set():
                                feed_failures.append(type(error).__name__)
                        finally:
                            producer.stdin.close()

                    feeder = threading.Thread(target=feed_ts, daemon=True)
                    feeder.start()
                deadline = started + 45
                playlist = Path(command[-1])
                while time.monotonic() < deadline:
                    if published_buffer(playlist, live=shared is not None) is not None:
                        break
                    if producer.poll() is not None and producer.returncode:
                        raise RuntimeError("reference_producer_failed")
                    time.sleep(0.005)
                else:
                    raise TimeoutError("reference_readiness")
                ready = time.monotonic()
                consumer = subprocess.Popen(
                    [
                        argv[0],
                        "-nostdin",
                        "-v",
                        "error",
                        "-xerror",
                        "-threads",
                        "2",
                        "-live_start_index",
                        "0",
                        "-i",
                        f"http://127.0.0.1:{origin.server_port}/{playlist.relative_to(directory)}",
                        "-t",
                        str(seconds),
                        "-map",
                        "0:v:0",
                        "-map",
                        "0:a:0?",
                        *(["-c", "copy"] if consumer_mode == "copy" else []),
                        "-progress",
                        "pipe:1",
                        "-f",
                        "null",
                        "-",
                    ],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
                stdout, stderr = consumer.communicate(timeout=seconds + 60)
                if consumer.returncode:
                    raise RuntimeError("reference_decode_failed")
                times = [
                    int(l.split("=", 1)[1])
                    for l in stdout.splitlines()
                    if l.startswith("out_time_us=") and l.split("=", 1)[1].isdigit()
                ]
                duration = max(times, default=0) / 1e6
                if duration < seconds * 0.95:
                    raise RuntimeError("reference_short_decode")
                result.update(
                    status="passed",
                    ready_ms=(ready - started) * 1000,
                    total_wall_ms=(time.monotonic() - started) * 1000,
                    producer_wall_ms=(time.monotonic() - producer_started) * 1000,
                    producer_clock="popen_exec",
                    replay_inspection_included=bool(first and inspection),
                    decode_wall_ms=(time.monotonic() - ready) * 1000,
                    media_seconds=duration,
                    consumer_mode=consumer_mode,
                )
                if feed_failures:
                    raise RuntimeError("reference_replay_feed_failed")
        except (OSError, subprocess.SubprocessError, ValueError, RuntimeError) as error:
            result["status"] = "failed"
            result["failure"] = (
                str(error) if isinstance(error, RuntimeError) else type(error).__name__
            )
        finally:
            feed_stop.set()
            for process in (consumer, producer, ingest):
                if process and process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
            origin.shutdown()
            origin.server_close()
            thread.join()
            if feeder:
                feeder.join(timeout=5)
                if feeder.is_alive():
                    result.update(
                        status="failed", failure="reference_feeder_not_reaped"
                    )
        return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--commands", type=Path, required=True)
    p.add_argument("--source", required=True)
    p.add_argument("--seconds", type=int, default=30)
    p.add_argument("--output", type=Path, required=True)
    args = p.parse_args()
    commands = json.loads(args.commands.read_text())
    candidates = [
        a
        for a in commands.values()
        if Path(a[0]).name == "ffmpeg" and a[-1].endswith(".m3u8")
    ]
    if len(candidates) != 1:
        raise SystemExit(
            "ambiguous/multiple producer plan; requires a separate matched control"
        )
    result = run_reference(candidates[0], args.source, args.seconds)
    write_json(args.output, result)
    print(json.dumps(result))
    raise SystemExit(result["status"] != "passed")


if __name__ == "__main__":
    main()
