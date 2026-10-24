#!/usr/bin/env python3
"""Supervisor for an owned --network=none lab; never alters host networking."""

import asyncio
import json
import os
from pathlib import Path
import secrets
import signal
import subprocess
import time
import urllib.request


async def bridge(reader, writer):
    try:
        remote_reader, remote_writer = await asyncio.open_connection("127.0.0.1", 8090)

        async def pump(source, target):
            try:
                while data := await source.read(64 * 1024):
                    target.write(data)
                    await target.drain()
            finally:
                target.close()

        tasks = [
            asyncio.create_task(pump(reader, remote_writer)),
            asyncio.create_task(pump(remote_reader, writer)),
        ]
        await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
    except OSError:
        writer.close()


async def main():
    assert Path("/.dockerenv").exists()
    assert os.environ.get("MEDIA_LAB_ISOLATED") == "1"
    public_egress = os.environ.get("MEDIA_LAB_PUBLIC_HTTPS_EGRESS") == "1"
    if not public_egress:
        assert sorted(p.name for p in Path("/sys/class/net").iterdir()) == ["lo"], (
            "requires network none"
        )
    subprocess.run(["ip", "addr", "add", "11.255.255.1/32", "dev", "lo"], check=True)
    run = Path("/run/lab")
    state = Path("/state")
    state.mkdir(mode=0o700, exist_ok=True)
    os.chown(state, 10001, 10001)
    bootstrap = secrets.token_urlsafe(48)
    env = dict(
        os.environ,
        API_KEY=bootstrap,
        DATA_DIR=str(state),
        BIND_ADDRESS="127.0.0.1:8090",
        MAX_OUTPUTS="8",
        FFMPEG_PATH=os.environ.get("LAB_FFMPEG", "/opt/ffmpeg/bin/ffmpeg"),
        FFPROBE_PATH=os.environ.get("LAB_FFPROBE", "/opt/ffmpeg/bin/ffprobe"),
        TORRENT_ENABLED="false",
    )
    origin_log = (run / "origin.log").open("wb")
    origin = subprocess.Popen(
        [
            "python3",
            "/lab/origin.py",
            "--root",
            "/corpus",
            "--bind",
            "11.255.255.1",
            "--cert",
            "/certs/origin.crt",
            "--key",
            "/certs/origin.key",
            "--events",
            str(run / "origin.jsonl"),
        ],
        stdout=origin_log,
        stderr=origin_log,
    )

    def restrict():
        # Supplementary groups include only the explicitly mapped render node.
        os.setgroups(
            [os.stat("/dev/dri/renderD128").st_gid]
            if Path("/dev/dri/renderD128").exists()
            else []
        )
        os.setgid(10001)
        os.setuid(10001)

    output = (run / "gateway.log").open("wb")
    gateway = subprocess.Popen(
        ["/candidate/playback-gateway"],
        env=env,
        stdout=output,
        stderr=output,
        preexec_fn=restrict,
    )
    monitor = None
    stop = asyncio.Event()
    for sig in (signal.SIGTERM, signal.SIGINT):
        asyncio.get_running_loop().add_signal_handler(sig, stop.set)
    try:
        for _ in range(600):
            if gateway.poll() is not None:
                raise RuntimeError(
                    "gateway exited before readiness; inspect private gateway.log"
                )
            try:
                with urllib.request.urlopen(
                    "http://127.0.0.1:8090/ready", timeout=1
                ) as response:
                    if response.status == 200:
                        break
            except OSError:
                pass
            await asyncio.sleep(0.1)
        else:
            raise TimeoutError("gateway readiness")
        body = dict(
            label="Owned media qualification",
            namespaces=["fixture"],
            scopes=["capabilities", "create", "read", "renew", "release"],
            quotas=dict(inputs=8, outputs=8, viewers=64),
        )
        req = urllib.request.Request(
            "http://127.0.0.1:8090/v1/keys",
            data=json.dumps(body).encode(),
            headers={
                "Authorization": "Bearer " + bootstrap,
                "Content-Type": "application/json",
            },
        )
        with urllib.request.urlopen(req, timeout=10) as response:
            issued = json.load(response)
        config = dict(
            key=issued["secret"],
            source_origin="https://11.255.255.1:18443",
            gateway_pid=gateway.pid,
            origin_pid=origin.pid,
            public_https_egress=public_egress,
        )
        (run / "fixture.json").write_text(json.dumps(config))
        os.chmod(run / "fixture.json", 0o600)
        monitor = subprocess.Popen(
            ["python3", "/lab/monitor.py"], stdout=origin_log, stderr=origin_log
        )
        socket = run / "gateway.sock"
        socket.unlink(missing_ok=True)
        server = await asyncio.start_unix_server(bridge, str(socket))
        os.chmod(socket, 0o600)
        async with server:
            while not stop.is_set():
                if (
                    gateway.poll() is not None
                    or origin.poll() is not None
                    or monitor.poll() is not None
                ):
                    raise RuntimeError("owned child exited")
                try:
                    await asyncio.wait_for(stop.wait(), timeout=1)
                except asyncio.TimeoutError:
                    pass
    finally:
        owned = [
            process for process in (gateway, origin, monitor) if process is not None
        ]
        for process in owned:
            if process.poll() is None:
                process.terminate()
        for process in owned:
            try:
                process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        output.close()
        origin_log.close()


if __name__ == "__main__":
    asyncio.run(main())
