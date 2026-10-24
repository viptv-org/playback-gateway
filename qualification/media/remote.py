#!/usr/bin/env python3
"""Stage only owned lab files on the Intel host; credentials stay in a private env file."""

import argparse
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile


def settings(path):
    values = {}
    for line in path.read_text().splitlines():
        if "=" in line and not line.startswith("#"):
            key, value = line.split("=", 1)
            values[key] = value.strip().strip("\"'")
    return values


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("action", choices=["stage", "start", "stop", "status", "fetch"])
    p.add_argument("--credentials", type=Path, required=True)
    p.add_argument(
        "--local",
        type=Path,
        required=True,
        help="owned lab root, containing baseline/ and corpus/",
    )
    p.add_argument("--remote", default="/home/vynxc/.cache/viptv-media-lab-20261023")
    p.add_argument("--label", default="baseline")
    p.add_argument("--image", default="playback-media-lab:intel-20261023")
    p.add_argument("--binary-label", default="baseline")
    p.add_argument("--vaapi-with-qsv", action="store_true")
    p.add_argument("--skip-build", action="store_true")
    p.add_argument(
        "--link-dest", help="existing immutable remote corpus for rsync payload reuse"
    )
    args = p.parse_args()
    assert (
        args.remote.startswith("/home/") and "/.cache/viptv-media-lab-" in args.remote
    )
    assert args.label.replace("-", "").isalnum()
    assert args.binary_label.replace("-", "").isalnum()
    if args.link_dest:
        assert (
            args.link_dest.startswith("/home/")
            and "/.cache/viptv-media-lab-" in args.link_dest
        )
        assert "/../" not in args.link_dest
    v = settings(args.credentials)
    env = dict(os.environ, SSHPASS=v["SERVER_PASSWORD"])
    host = v["SERVER_USER"] + "@" + v["SERVER_HOST"]
    ssh = [
        "sshpass",
        "-e",
        "ssh",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "ConnectTimeout=10",
        host,
    ]

    def execute(command, sudo=False):
        argv = ssh + (
            ["sudo -S -p '' sh -c " + shlex.quote(command)] if sudo else [command]
        )
        return subprocess.run(
            argv,
            env=env,
            input=v["SERVER_PASSWORD"] + "\n" if sudo else None,
            text=True,
            check=True,
        )

    def sync(source, target, reverse=False):
        transport = "sshpass -e ssh -o StrictHostKeyChecking=yes"
        src, dst = (
            (host + ":" + source, target) if reverse else (source, host + ":" + target)
        )
        subprocess.run(
            [
                "rsync",
                "-a",
                "--partial",
                "--exclude=__pycache__",
                "--exclude=*.key",
                "-e",
                transport,
                src,
                dst,
            ],
            env=env,
            check=True,
        )

    root = shlex.quote(args.remote)
    run = args.remote + "/" + args.label
    if args.action == "stage":
        execute(
            f"mkdir -p {root}/qualification/media {root}/baseline {root}/corpus; chmod 700 {root}"
        )
        sync(
            str(Path(__file__).resolve().parent) + "/",
            args.remote + "/qualification/media/",
        )
        for name in ("playback-gateway", "revision"):
            sync(str(args.local / "baseline" / name), args.remote + "/baseline/" + name)
        # Only completed, validated immutable assets are staged. Never race an
        # encoder by copying a partial segment from an actively generated case.
        corpus = args.local / "corpus"
        paths = [p.name for p in corpus.glob("*.json")]
        for receipt in sorted(corpus.glob("*/receipt.json")):
            data = json.loads(receipt.read_text())
            if data.get("status") != "valid":
                continue
            paths.extend(
                str((receipt.parent / name).relative_to(corpus))
                for name in data["files"]
            )
            paths.append(str(receipt.relative_to(corpus)))
        live = corpus / "live"
        if live.exists():
            for package in live.rglob("validated.json"):
                paths.extend(
                    str(p.relative_to(corpus))
                    for p in package.parent.iterdir()
                    if p.is_file() and p.suffix != ".log"
                )
        with tempfile.NamedTemporaryFile(mode="w") as listing:
            listing.write("\n".join(paths) + "\n")
            listing.flush()
            subprocess.run(
                [
                    "rsync",
                    "-ar",
                    "--partial",
                    *(
                        ["--link-dest=" + args.link_dest, "--checksum"]
                        if args.link_dest
                        else []
                    ),
                    "--files-from",
                    listing.name,
                    "-e",
                    "sshpass -e ssh -o StrictHostKeyChecking=yes",
                    str(corpus) + "/",
                    host + ":" + args.remote + "/corpus/",
                ],
                env=env,
                check=True,
            )
        if not args.skip_build:
            execute(
                f"cd {root} && docker build -f qualification/media/Dockerfile.lab -t playback-media-lab:20261023 .",
                sudo=True,
            )
    elif args.action == "start":
        # The remote helper runs as root; sudo -n in lab.py then remains noninteractive.
        extra = " --vaapi-with-qsv" if args.vaapi_with_qsv else ""
        execute(
            f"cd {root} && python3 qualification/media/lab.py start --run {shlex.quote(run)} --corpus {root}/corpus --binary {root}/{args.binary_label}/playback-gateway --hardware qsv --device /dev/dri/renderD128 --image {shlex.quote(args.image)}{extra}",
            sudo=True,
        )
        execute(
            f"chown {shlex.quote(v['SERVER_USER'])} {shlex.quote(run + '/gateway.sock')} {shlex.quote(run + '/fixture.json')} {shlex.quote(run + '/provenance.json')} {shlex.quote(run)}",
            sudo=True,
        )
    elif args.action == "stop":
        execute(
            f"python3 {root}/qualification/media/lab.py stop --run {shlex.quote(run)}",
            sudo=True,
        )
    elif args.action == "status":
        execute(
            f"test -f {shlex.quote(run + '/container-id')} && docker inspect --format '{{{{.State.Status}}}}' \"$(cat {shlex.quote(run + '/container-id')})\"",
            sudo=True,
        )
    else:
        target = args.local / ("intel-" + args.label)
        target.mkdir(mode=0o700, exist_ok=True)
        for name in ("fixture.json", "provenance.json"):
            sync(run + "/" + name, str(target / name), reverse=True)
        print(
            "Use an owned SSH tunnel to "
            + run
            + "/gateway.sock; never expose the control bridge publicly."
        )


if __name__ == "__main__":
    main()
