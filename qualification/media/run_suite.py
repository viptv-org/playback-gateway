#!/usr/bin/env python3
"""Run frozen, prepared Intel campaigns serially and retain failed/incomplete stages.

The config names owned local/remote run directories, not production services.
Provision runtimes with remote.py first. This controller never rebuilds a binary
or changes a running service's configuration. Resume skips completed stages;
an interrupted browser invocation is explicitly not an uninterrupted soak.
"""

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shlex
import signal
import subprocess
import time

from corpus import digest, write_json
from remote import settings

HERE = Path(__file__).resolve().parent


def stages(config, phase):
    local = Path(phase["local_run"])
    browser = [
        "python3",
        str(HERE / "lab.py"),
        "run",
        "--run",
        str(local),
        "--corpus",
        config["corpus"],
        "--manifest",
        config["manifest"],
        "--backend-binary",
        config["backend_binary"],
        "--tv-root",
        config["tv_root"],
        "--certs",
        config["certs"],
        "--origin",
        config["origin"],
        "--phase",
        phase["name"],
        "--cleanup-stride",
        "20",
    ]
    browser += ["--driver", config.get("driver", "current-api")]
    for key, flag in [
        ("web_dist", "--web-dist"),
        ("browser_executable", "--browser-executable"),
    ]:
        if config.get(key):
            browser += [flag, config[key]]
    yield "browser", False, browser, str(local / phase["name"] / "summary.json")
    for name, key, options in [
        ("browser-startup", "browser_startup_manifest", ["--cleanup-stride", "1"]),
        ("timing", "timing_manifest", []),
        ("clock-timing", "clock_manifest", []),
        ("quality", "quality_manifest", ["--capture-media", "--cleanup-stride", "1"]),
        ("captions", "caption_manifest", ["--player-profile", "mse"]),
        (
            "backend-soak",
            "soak_manifest",
            ["--persistent-backend", "--soak-hours", str(config.get("soak_hours", 8))],
        ),
    ]:
        if not config.get(key):
            continue
        argv = browser.copy()
        argv[argv.index("--manifest") + 1] = config[key]
        argv += ["--campaign-name", name + "-" + phase["name"], *options]
        yield (
            name,
            False,
            argv,
            str(local / (name + "-" + phase["name"]) / "summary.json"),
        )
    for name, argv in [
        (
            "startup",
            [
                "measure.py",
                "--selection",
                "diverse",
                "--count",
                "40",
                "--trials",
                str(config.get("api_startup_trials", 20)),
            ],
        ),
        (
            "throughput",
            [
                "measure.py",
                "--selection",
                "diverse",
                "--count",
                "40",
                "--trials",
                "1",
                "--seconds",
                "30",
                "--reference",
                "--consumer-mode",
                "copy",
            ],
        ),
        (
            "roku-hevc",
            [
                "measure.py",
                "--selection",
                "diverse",
                "--count",
                "40",
                "--trials",
                "1",
                "--profile",
                "roku-hevc",
            ],
        ),
        (
            "shared-live-throughput",
            [
                "measure.py",
                "--live",
                "--live-manifest",
                "/corpus/clock-live-manifest.json",
                "--count",
                "3",
                "--trials",
                "1",
                "--seconds",
                "30",
                "--reference",
                "--consumer-mode",
                "copy",
            ],
        ),
        ("network", ["network_campaign.py"]),
    ]:
        yield (
            name,
            True,
            [
                "python3",
                phase.get("remote_harness", "/lab") + "/" + argv[0],
                *argv[1:],
                "--output",
                "/run/lab/" + name,
            ],
            phase["remote_run"] + "/" + name + "/summary.json",
        )
    for name, options in [
        ("concurrency", []),
        (
            "live-concurrency",
            [
                "--live",
                "--live-manifest",
                config.get(
                    "live_manifest",
                    str(Path(config["corpus"]) / "clock-live-manifest.json"),
                ),
            ],
        ),
        ("concurrency-1080p", ["--workload", "1080p"]),
        ("concurrency-4k", ["--workload", "4k"]),
    ]:
        argv = [
            "python3",
            str(HERE / "load_clients.py"),
            "--run",
            str(local),
            "--corpus",
            config["corpus"],
            "--output",
            str(local / name),
            "--observer-config",
            str(local / (".observer-" + phase["name"] + ".json")),
            "--certs",
            config["certs"],
            "--origin",
            config["origin"],
            "--ffmpeg",
            config.get("consumer_ffmpeg", "/usr/bin/ffmpeg"),
            *options,
        ]
        yield name, False, argv, str(local / name / "summary.json")


def completed(name, summary):
    if name in (
        "browser",
        "browser-startup",
        "timing",
        "clock-timing",
        "quality",
        "captions",
    ):
        return summary.get("not_run") == 0 and summary.get("full_corpus") is True
    if name == "backend-soak":
        return (
            summary.get("complete") is True
            and summary.get("backend_continuous") is True
        )
    if "concurrency" in name:
        return summary.get("complete") is True
    return (
        summary.get("planned", 0) > 0 and summary.get("completed") == summary["planned"]
    )


def main():
    def interrupted(*_):
        raise KeyboardInterrupt("suite interrupted; stopping only its active lab")

    signal.signal(signal.SIGTERM, interrupted)
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--config", type=Path, required=True)
    p.add_argument("--credentials", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    args = p.parse_args()
    os.umask(0o077)
    args.output.mkdir(parents=True, exist_ok=True)
    lock = (args.output / ".lock").open("w")
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    config = json.loads(args.config.read_text())
    frozen = dict(
        config=config,
        files={
            p.name: digest(p)
            for p in HERE.iterdir()
            if p.suffix in (".py", ".mjs", ".html")
        },
    )
    receipt = args.output / "campaign.json"
    if receipt.exists():
        assert json.loads(receipt.read_text()) == frozen, "suite configuration changed"
    else:
        write_json(receipt, frozen)
    credentials = settings(args.credentials)
    env = dict(os.environ, SSHPASS=credentials["SERVER_PASSWORD"])
    ssh = [
        "sshpass",
        "-e",
        "ssh",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "ConnectTimeout=10",
        credentials["SERVER_USER"] + "@" + credentials["SERVER_HOST"],
    ]

    def remote(command, log=None):
        return subprocess.run(
            ssh + ["sudo -S -p '' sh -c " + shlex.quote(command)],
            env=env,
            input=credentials["SERVER_PASSWORD"] + "\n",
            text=True,
            stdout=log or subprocess.PIPE,
            stderr=log or subprocess.PIPE,
        )

    outcomes = []
    for phase in config["phases"]:
        local = Path(phase["local_run"])
        remote_root = phase["remote_run"]
        assert "/.cache/viptv-media-lab-" in remote_root and "/../" not in remote_root
        identity = remote("cat " + shlex.quote(remote_root + "/container-id"))
        identity.check_returncode()
        identity = identity.stdout.strip()
        assert len(identity) == 64 and all(c in "0123456789abcdef" for c in identity)
        alive = remote("docker inspect --format '{{.State.Running}}' " + identity)
        assert alive.stdout.strip() == "true", "prepared lab container is not running"
        write_json(
            local / (".observer-" + phase["name"] + ".json"),
            dict(
                credentials=str(args.credentials.resolve()),
                local_run=str(local),
                remote_run=remote_root,
                harness=phase.get("remote_harness", "/lab"),
            ),
        )
        socket = Path("/tmp") / (
            "media-suite-"
            + hashlib.sha256(str(local).encode()).hexdigest()[:12]
            + ".sock"
        )
        assert not socket.exists(), "owned tunnel already exists"
        link = local / "gateway.sock"
        if link.is_symlink():
            link.unlink()
        assert not link.exists(), "refusing to replace an existing control socket"
        with (args.output / (phase["name"] + "-tunnel.log")).open("w") as log:
            tunnel = subprocess.Popen(
                ssh[:-1]
                + [
                    "-o",
                    "ExitOnForwardFailure=yes",
                    "-o",
                    "ServerAliveInterval=15",
                    "-o",
                    "ServerAliveCountMax=3",
                    "-N",
                    "-L",
                    str(socket) + ":" + remote_root + "/gateway.sock",
                    ssh[-1],
                ],
                env=env,
                stdout=log,
                stderr=log,
            )
            try:
                until = time.monotonic() + 20
                while not socket.exists():
                    if tunnel.poll() is not None or time.monotonic() >= until:
                        raise RuntimeError("owned SSH tunnel failed")
                    time.sleep(0.1)
                link.symlink_to(socket)
                for name, is_remote, argv, summary_path in stages(config, phase):
                    if phase.get("stages") and name not in phase["stages"]:
                        continue
                    label = phase["name"] + "-" + name
                    stage_file = args.output / (label + ".json")
                    if stage_file.exists() and json.loads(stage_file.read_text()).get(
                        "complete"
                    ):
                        outcomes.append(json.loads(stage_file.read_text()))
                        continue
                    started = time.time()
                    write_json(
                        stage_file,
                        dict(
                            stage=label,
                            complete=False,
                            started_at=started,
                            status="running",
                        ),
                    )
                    print(json.dumps(dict(stage=label, status="running")), flush=True)
                    with (args.output / (label + ".log")).open("w") as output:
                        result = (
                            remote(
                                "docker exec " + identity + " " + shlex.join(argv),
                                output,
                            )
                            if is_remote
                            else subprocess.run(
                                argv, stdout=output, stderr=subprocess.STDOUT
                            )
                        )
                    if is_remote:
                        read = remote("cat " + shlex.quote(summary_path))
                        read.check_returncode()
                        summary = json.loads(read.stdout)
                    else:
                        summary = json.loads(Path(summary_path).read_text())
                    record = dict(
                        stage=label,
                        exit_code=result.returncode,
                        started_at=started,
                        elapsed_seconds=time.time() - started,
                        complete=completed(name, summary),
                        status="passed" if result.returncode == 0 else "failed",
                        summary=summary,
                    )
                    write_json(stage_file, record)
                    outcomes.append(record)
                    write_json(
                        args.output / "summary.json",
                        dict(complete=False, stages=outcomes),
                    )
                    print(
                        json.dumps(
                            {
                                k: record[k]
                                for k in (
                                    "stage",
                                    "status",
                                    "complete",
                                    "elapsed_seconds",
                                )
                            }
                        ),
                        flush=True,
                    )
                    if not record["complete"]:
                        raise RuntimeError(
                            "incomplete stage; inspect preserved evidence before resuming"
                        )
            except BaseException:
                remote("docker stop --time 15 " + identity)
                raise
            finally:
                tunnel.terminate()
                tunnel.wait(timeout=20)
                link.unlink(missing_ok=True)
                socket.unlink(missing_ok=True)
    write_json(
        args.output / "summary.json",
        dict(
            complete=True,
            passed=all(r["status"] == "passed" for r in outcomes),
            stages=outcomes,
        ),
    )


if __name__ == "__main__":
    main()
