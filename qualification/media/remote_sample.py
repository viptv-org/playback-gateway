"""Private SSH resource observation; credentials never enter argv or receipts."""

import json
import os
from pathlib import Path
import shlex
import subprocess

from remote import settings


class RemoteSample:
    def __init__(self, config, output):
        self.config = json.loads(config.read_text())
        self.output = output
        self.rows = []
        self.commands = {}
        self.files = {}

    def __enter__(self):
        cfg = self.config
        password = None
        if cfg.get("kind") == "local":
            self.env = dict(os.environ)
            identity = (Path(cfg["local_run"]) / "container-id").read_text().strip()
            prefix = ["sudo", "-n"]
        else:
            values = settings(Path(cfg["credentials"]))
            password = values["SERVER_PASSWORD"]
            self.env = dict(os.environ, SSHPASS=password)
            self.ssh = [
                "sshpass",
                "-e",
                "ssh",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=2",
                values["SERVER_USER"] + "@" + values["SERVER_HOST"],
            ]
            run = cfg["remote_run"]
            assert "/.cache/viptv-media-lab-" in run and "/../" not in run
            read = subprocess.run(
                self.ssh + ["sudo -S -p '' cat " + shlex.quote(run + "/container-id")],
                env=self.env,
                input=password + "\n",
                capture_output=True,
                text=True,
                check=True,
            )
            identity = read.stdout.strip()
            prefix = None
        assert len(identity) == 64 and all(c in "0123456789abcdef" for c in identity)
        argv = [
            "docker",
            "exec",
            "-i",
            identity,
            "python3",
            cfg.get("harness", "/lab") + "/capture.py",
            "--output",
            "/run/lab/" + self.output,
        ]
        self.error = (Path(cfg["local_run"]) / "observer-errors.log").open("a")
        command = (
            prefix + argv
            if prefix
            else self.ssh + ["sudo -S -p '' sh -c " + shlex.quote(shlex.join(argv))]
        )
        self.process = subprocess.Popen(
            command,
            env=self.env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.error,
            text=True,
        )
        if password:
            self.process.stdin.write(password + "\n")
            self.process.stdin.flush()
        ready = json.loads(self.process.stdout.readline())
        assert ready.get("ready") is True, "server observer failed to initialize"
        return self

    def __exit__(self, *_):
        self.process.stdin.close()
        self.process.stdin = None
        raw, _ = self.process.communicate(timeout=60)
        code = self.process.returncode
        self.error.close()
        assert code == 0, "server resource observer failed"
        result = json.loads(raw)
        assert result.get("complete") is True, "server resource observation incomplete"
        self.commands = {int(k): v for k, v in result["commands"].items()}
        self.resources_file = result["resources_file"]
