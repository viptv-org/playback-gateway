#!/usr/bin/env python3
"""Own lab processes, freeze provenance, and run resumable real-browser campaigns."""

import argparse
import contextlib
import fcntl
import hashlib
import http.client
import itertools
import json
import math
import os
from pathlib import Path
import random
import secrets
import signal
import socket
import ssl
import subprocess
import sys
import time
import urllib.request
import urllib.parse
from corpus import digest, write_json

HERE = Path(__file__).resolve().parent


class UnixHTTP(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=45)
        self.path = str(path)

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def gateway(run, method, path, body=None, identity=None):
    config = json.loads((run / "fixture.json").read_text())
    conn = UnixHTTP(run / "gateway.sock")
    headers = {
        "Authorization": "Bearer " + config["key"],
        "Content-Type": "application/json",
    }
    if identity:
        headers["Idempotency-Key"] = identity
        headers["x-playback-trace"] = str(
            int.from_bytes(hashlib.sha256(identity.encode()).digest()[:8], "big")
        )
    try:
        conn.request(
            method, path, json.dumps(body) if body is not None else None, headers
        )
        response = conn.getresponse()
        raw = response.read()
        value = json.loads(raw) if raw else None
        if response.status >= 400:
            raise RuntimeError(
                f"gateway {response.status}: {value.get('error', {}).get('code', 'unknown')}"
            )
        return value
    finally:
        conn.close()


def port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def control_socket_alias(run, campaign):
    # Concurrent test consumers may share a gateway. Each campaign owns its
    # alias, so ending one cannot unlink another consumer's transport.
    identity = str(run) + "\0" + str(campaign)
    return Path("/tmp") / ("media-lab-" + hashlib.sha256(identity.encode()).hexdigest()[:12] + ".sock")


def process_usage(pid):
    try:
        raw = Path(f"/proc/{pid}/stat").read_text()
        fields = raw[raw.rfind(")") + 2 :].split()
        return dict(
            wall_ns=time.time_ns(),
            pid=pid,
            cpu_ticks=int(fields[11]) + int(fields[12]),
            ticks_per_second=os.sysconf("SC_CLK_TCK"),
            rss_bytes=int(fields[21]) * os.sysconf("SC_PAGE_SIZE"),
            start_ticks=int(fields[19]),
        )
    except (OSError, ValueError, IndexError):
        return None


def cleanup_checkpoint(run, pending, campaign, observe_idle=False):
    if not pending:
        return True
    started = time.monotonic()
    deadline = started + 45
    available = None
    while time.monotonic() < deadline:
        available = gateway(run, "GET", "/v1/capabilities")["available"]
        if available == dict(inputs=8, outputs=8, viewers=64):
            break
        time.sleep(0.25)
    clean = available == dict(inputs=8, outputs=8, viewers=64)
    if clean and observe_idle:
        # The independent one-Hz resource observer must see a fully reclaimed
        # checkpoint, rather than sampling only the next workload's allocation.
        time.sleep(1.1)
    name = "cleanup-" + pending[-1][1].get("case_id", pending[-1][1]["fixture"])
    write_json(
        campaign / (name + ".json"),
        dict(
            status="passed" if clean else "failed",
            available=available,
            wall_ns=time.time_ns(),
            wait_ms=(time.monotonic() - started) * 1000,
            cases=[result["fixture"] for _, result in pending],
        ),
    )
    for path, result in pending:
        result["capacity_reclaimed"] = clean
        result["cleanup_checkpoint"] = name
        if not clean:
            result["status"] = "failed"
            result["cleanup_failure"] = "admission_not_reclaimed"
        write_json(path, result)
    pending.clear()
    return clean


def wait_http(url, process, timeout=45):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError("owned process exited; inspect its private log")
        try:
            with urllib.request.urlopen(url, timeout=1) as response:
                if response.status == 200:
                    return
        except OSError:
            pass
        time.sleep(0.1)
    raise TimeoutError("owned service readiness")


@contextlib.contextmanager
def child(argv, log, env=None):
    with Path(log).open("wb") as output:
        p = subprocess.Popen(
            argv,
            stdout=output,
            stderr=subprocess.STDOUT,
            env=env,
            start_new_session=True,
        )
        try:
            yield p
        finally:
            if p.poll() is None:
                os.killpg(p.pid, signal.SIGTERM)
                try:
                    p.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(p.pid, signal.SIGKILL)
                    p.wait()


def certificates(root):
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    if (root / "origin.crt").exists():
        return
    commands = [
        [
            "openssl",
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            str(root / "ca.key"),
            "-out",
            str(root / "ca.crt"),
            "-days",
            "30",
            "-subj",
            "/CN=Owned Media Lab CA",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
        ],
        [
            "openssl",
            "req",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            str(root / "origin.key"),
            "-out",
            str(root / "origin.csr"),
            "-subj",
            "/CN=11.255.255.1",
        ],
    ]
    (root / "extensions").write_text(
        "subjectAltName=IP:11.255.255.1\nextendedKeyUsage=serverAuth\nbasicConstraints=CA:FALSE\n"
    )
    commands.append(
        [
            "openssl",
            "x509",
            "-req",
            "-in",
            str(root / "origin.csr"),
            "-CA",
            str(root / "ca.crt"),
            "-CAkey",
            str(root / "ca.key"),
            "-CAcreateserial",
            "-out",
            str(root / "origin.crt"),
            "-days",
            "30",
            "-extfile",
            str(root / "extensions"),
        ]
    )
    for command in commands:
        subprocess.run(
            command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True
        )
    for key in root.glob("*.key"):
        key.chmod(0o600)
    (root / "bundle.crt").write_bytes(
        Path("/etc/ssl/certs/ca-certificates.crt").read_bytes()
        + (root / "ca.crt").read_bytes()
    )
    (root / "bundle.crt").chmod(0o644)


def start(args):
    assert math.isfinite(args.cpus) and 1 <= args.cpus <= 64, "invalid lab CPU budget"
    run = args.run.resolve()
    run.mkdir(parents=True, exist_ok=True, mode=0o700)
    if (run / "container-id").exists():
        raise SystemExit(
            "run already has a container; stop it or use a fresh run directory"
        )
    cert = run / "certs"
    certificates(cert)
    image_id = subprocess.check_output(
        ["sudo", "-n", "docker", "image", "inspect", "--format", "{{.Id}}", args.image],
        text=True,
    ).strip()
    command = [
        "sudo",
        "-n",
        "docker",
        "run",
        "--detach",
        "--init",
        "--user",
        "0:0",
        "--network",
        "bridge" if args.public_https_egress else "none",
        "--read-only",
        "--no-healthcheck",
        "--cap-drop",
        "ALL",
        "--cap-add",
        "NET_ADMIN",
        "--cap-add",
        "SETUID",
        "--cap-add",
        "SETGID",
        "--cap-add",
        "CHOWN",
        "--cap-add",
        "DAC_OVERRIDE",
        "--cap-add",
        "KILL",
        "--cap-add",
        "SYS_PTRACE",
        "--security-opt",
        "no-new-privileges",
        "--memory",
        "3g",
        "--pids-limit",
        "512",
        "--cpus",
        str(args.cpus),
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,size=128m",
        "--tmpfs",
        "/state:rw,nosuid,nodev,size=512m,uid=10001,gid=10001,mode=0700",
        "--env",
        "MEDIA_LAB_ISOLATED=1",
        "--mount",
        f"type=bind,source={run},target=/run/lab",
        "--mount",
        f"type=bind,source={args.corpus.resolve()},target=/corpus,readonly",
        "--mount",
        f"type=bind,source={args.binary.resolve()},target=/candidate/playback-gateway,readonly",
        "--mount",
        f"type=bind,source={HERE},target=/lab,readonly",
        "--mount",
        f"type=bind,source={cert},target=/certs,readonly",
        "--mount",
        f"type=bind,source={cert / 'bundle.crt'},target=/etc/ssl/certs/ca-certificates.crt,readonly",
    ]
    if args.public_https_egress:
        command += ["--env", "MEDIA_LAB_PUBLIC_HTTPS_EGRESS=1"]
    if args.device:
        command += [
            "--device",
            str(args.device),
            "--env",
            ("QSV_DEVICE=" if args.hardware == "qsv" else "VAAPI_DEVICE=")
            + str(args.device),
        ]
        if args.hardware == "qsv" and args.vaapi_with_qsv:
            command += ["--env", "VAAPI_DEVICE=" + str(args.device)]
    command += [image_id]
    identity = subprocess.check_output(command, text=True).strip()
    (run / "container-id").write_text(identity + "\n")
    revision = subprocess.run(
        ["git", "-C", str(HERE.parents[1]), "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
    )
    source_revision = (
        revision.stdout.strip()
        if revision.returncode == 0
        else (args.binary.parent / "revision").read_text().strip()
    )
    write_json(
        run / "provenance.json",
        dict(
            binary_sha256=digest(args.binary),
            manifest_sha256=digest(args.corpus / "manifest.json"),
            image=args.image,
            image_id=image_id,
            hardware=args.hardware,
            vaapi_with_qsv=args.vaapi_with_qsv,
            public_https_egress=args.public_https_egress,
            device=str(args.device) if args.device else None,
            revision=source_revision,
            cpu_limit=args.cpus,
            memory_limit_bytes=3 * 1024**3,
            started_at=time.time(),
        ),
    )
    for _ in range(600):
        if (run / "gateway.sock").exists():
            # Files are created by the container root; expose them only to this user.
            subprocess.run(
                [
                    "sudo",
                    "-n",
                    "chown",
                    f"{os.getuid()}:{os.getgid()}",
                    str(run / "gateway.sock"),
                    str(run / "fixture.json"),
                ],
                check=True,
            )
            print(json.dumps({"status": "ready", "run": str(run)}))
            return
        alive = subprocess.check_output(
            [
                "sudo",
                "-n",
                "docker",
                "inspect",
                "--format",
                "{{.State.Running}}",
                identity,
            ],
            text=True,
        ).strip()
        if alive != "true":
            raise SystemExit("lab container exited; inspect container/gateway logs")
        time.sleep(0.1)
    raise SystemExit("lab startup timeout")


def run_campaign(args):
    if args.driver == "kmp":
        assert args.web_dist and (args.web_dist / "index.html").is_file(), (
            "explicit isolated KMP distribution required"
        )
    assert math.isfinite(args.soak_hours) and 0 <= args.soak_hours <= 24, (
        "invalid soak duration"
    )
    run = args.run.resolve()
    web_dist = args.web_dist or args.tv_root / "dist"
    manifest_path = args.manifest or args.corpus / "manifest.json"
    manifest = json.loads(manifest_path.read_text())
    fixtures = manifest["fixtures"]
    live_epoch = hashlib.sha256(
        (str(run) + (args.campaign_name or args.phase) + str(args.seconds)).encode()
    ).hexdigest()[:16]
    fixtures = [
        dict(
            f,
            file=f["file"]
            + (
                "?campaign=" + live_epoch + "-" + f["id"]
                if "?" not in f["file"]
                else "&campaign=" + live_epoch + "-" + f["id"]
            ),
        )
        if f.get("live")
        else f
        for f in fixtures
    ]
    if not manifest.get("ordered"):
        random.Random(20261023).shuffle(fixtures)
    if args.ids:
        fixtures = [f for f in fixtures if f["id"] in args.ids.split(",")]
    if args.limit:
        fixtures = fixtures[: args.limit]
    campaign_name = args.campaign_name or args.phase
    assert campaign_name.replace("-", "").replace("_", "").isalnum(), (
        "invalid campaign name"
    )
    campaign = run / campaign_name
    campaign.mkdir(mode=0o700, exist_ok=True)
    browser_password = None
    if args.driver == "kmp":
        password_file = campaign / ".browser-password"
        if not password_file.exists():
            password_file.write_text(secrets.token_urlsafe(24))
            password_file.chmod(0o600)
        browser_password = password_file.read_text()
    player_bundle = campaign / "player.mjs"
    browser_executable = args.browser_executable or Path(
        subprocess.check_output(
            [
                "node",
                "--input-type=module",
                "-e",
                "const {chromium}=await import("
                + json.dumps(str(args.tv_root / "node_modules/playwright/index.mjs"))
                + ");process.stdout.write(chromium.executablePath());",
            ],
            text=True,
        )
    )
    browser_executable = browser_executable.resolve()
    browser_sha256 = digest(browser_executable)
    if args.driver == "current-api":
        entry = campaign / "player-entry.ts"
        entry.write_text(
            "export {Html5FallbackAdapter} from "
            + json.dumps(str(args.tv_root / "vendor/video/src/html5-fallback.ts"))
            + ";\nexport {VizioHtml5Adapter} from "
            + json.dumps(str(args.tv_root / "vendor/video/src/vizio-html5.ts"))
            + ";\n"
        )
        subprocess.run(
            [
                str(args.tv_root / "node_modules/.bin/esbuild"),
                str(entry),
                "--bundle",
                "--format=esm",
                "--platform=browser",
                "--outfile=" + str(player_bundle),
            ],
            check=True,
            capture_output=True,
        )
    frozen = {
        "manifest_sha256": digest(manifest_path),
        "backend_sha256": digest(args.backend_binary),
        "browser_entry_sha256": digest(web_dist / "index.html"),
        "browser_bundle_sha256": {
            str(p.relative_to(web_dist)): digest(p)
            for p in sorted(web_dist.rglob("*"))
            if p.is_file()
        },
        "browser_seconds": args.seconds,
        "browser_executable_sha256": browser_sha256,
        "browser_automation_sha256": digest(
            args.tv_root / "node_modules/playwright/package.json"
        ),
        "browser_request_ca_sha256": digest(args.certs / "ca.crt"),
        "browser_registry_sha256": digest(
            args.tv_root / "node_modules/playwright-core/browsers.json"
        ),
        "cleanup_stride": args.cleanup_stride,
        "persistent_backend": args.persistent_backend,
        "soak_hours": args.soak_hours,
        "driver": args.driver,
        "player_profile": args.player_profile,
        "capture_media": args.capture_media,
        "driver_sha256": digest(HERE / "flow.html")
        if args.driver == "current-api"
        else None,
        "player_bundle_sha256": digest(player_bundle)
        if args.driver == "current-api"
        else None,
        "harness_sha256": {
            p.name: digest(p)
            for p in sorted(HERE.iterdir())
            if p.suffix in (".py", ".mjs", ".html")
        },
        "phase": args.phase,
        "campaign_name": campaign_name,
        "planned": [f["id"] for f in fixtures],
        "gateway": json.loads((run / "provenance.json").read_text()),
    }
    if (campaign / "campaign.json").exists():
        assert json.loads((campaign / "campaign.json").read_text()) == frozen, (
            "resume provenance changed"
        )
    else:
        write_json(campaign / "campaign.json", frozen)
    lock = (campaign / ".lock").open("w")
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    if args.soak_hours:
        assert args.persistent_backend and 0 < len(fixtures) <= 20, (
            "continuous backend soak needs a small fixed catalog"
        )
        assert not (campaign / "soak-start.json").exists(), (
            "an interrupted soak cannot resume; use a new run/phase"
        )
        write_json(
            campaign / "soak-start.json",
            dict(started_at=time.time(), target_seconds=args.soak_hours * 3600),
        )
    # A short local alias avoids Unix sockaddr_un's 108-byte path limit.
    alias = control_socket_alias(run, campaign)
    if alias.is_symlink():
        alias.unlink()
    alias.symlink_to(run / "gateway.sock")
    ports = [port() for _ in range(3)]
    backend_port, control_port, addon_port = ports
    assert len(set(ports)) == 3
    gw = json.loads((run / "fixture.json").read_text())
    external = [fixture for fixture in fixtures if fixture.get("url")]
    if external:
        assert gw.get("public_https_egress") is True, (
            "external fixtures require a separate --public-https-egress runtime"
        )
        assert all(
            urllib.parse.urlsplit(fixture["url"]).scheme == "https"
            for fixture in external
        ), "external fixture references must use HTTPS"
    selection = campaign / "selection.json"
    # Keep the small owned catalog stable while randomizing playback order.
    # VOD cards precede channels, which are exercised through the real Live UI.
    catalog = sorted(fixtures, key=lambda f: (bool(f.get("live")), f["id"]))
    write_json(selection, catalog if args.persistent_backend else fixtures[:1])
    bridge_config = campaign / "bridge.json"
    write_json(
        bridge_config,
        dict(
            gateway_socket=str(alias),
            selection_file=str(selection),
            source_origin=gw["source_origin"],
            control_port=control_port,
            addon_port=addon_port,
        ),
    )
    backend_config = campaign / "backend.json"
    write_json(
        backend_config,
        dict(
            gateway_control=f"http://127.0.0.1:{control_port}/",
            gateway_endpoint=args.origin + "/gateway/",
            gateway_key=gw["key"],
            addon_manifest=f"http://127.0.0.1:{addon_port}/manifest.json",
            backend_bind=f"127.0.0.1:{backend_port}",
            selection_file=str(selection),
            source_origin=gw["source_origin"],
            **({"browser_password": browser_password} if browser_password else {}),
        ),
    )
    backend_config.chmod(0o600)
    caddy = campaign / "Caddyfile"
    caddy.write_text(f"""{{
 admin off
 auto_https disable_redirects
 log default {{
  output discard
 }}
}}
{args.origin} {{
 bind 127.0.0.1
 tls {args.certs / "viptv.local.test.crt"} {args.certs / "viptv.local.test.key"}
 handle /lab/player.mjs {{
  root * {campaign}
  rewrite * /player.mjs
  file_server
 }}
 handle_path /lab/* {{
  root * {HERE}
  file_server
 }}
 handle /api/* {{
  reverse_proxy 127.0.0.1:{backend_port}
 }}
 handle_path /gateway/* {{
  reverse_proxy unix/{alias}
 }}
 handle /tv/* {{
  uri strip_prefix /tv
  root * {web_dist}
  try_files {{path}} /index.html
  file_server
 }}
}}
""")
    outcomes = []
    pending_cleanup = []
    executed = []
    invocation_started = time.monotonic()
    invocation_wall = time.time()
    persistent_backend = None
    backend_continuity_ok = False
    try:
        with (
            child(
                ["node", str(HERE / "bridge.mjs"), str(bridge_config)],
                campaign / "bridge.log",
            ),
            child(
                ["caddy", "run", "--config", str(caddy)], campaign / "caddy.log"
            ) as ingress,
            contextlib.ExitStack() as services,
        ):
            wait_http(args.origin + "/tv/", ingress)
            if args.persistent_backend:
                persistent_backend = services.enter_context(
                    child(
                        [
                            str(args.backend_binary),
                            "gateway::media_lab_acceptance::media_lab_backend",
                            "--ignored",
                            "--exact",
                            "--nocapture",
                            "--test-threads=1",
                        ],
                        campaign / "persistent-backend.log",
                        dict(
                            os.environ,
                            VIPTV_MEDIA_LAB_CONFIG=str(backend_config),
                            VIPTV_AUTH_ORIGIN=args.origin,
                        ),
                    )
                )
                wait_http(
                    f"http://127.0.0.1:{backend_port}/api/health", persistent_backend
                )
            for case_index, fixture in enumerate(
                itertools.cycle(fixtures) if args.soak_hours else fixtures
            ):
                if (
                    args.soak_hours
                    and time.monotonic() - invocation_started >= args.soak_hours * 3600
                ):
                    break
                case_id = (
                    f"{case_index:06d}-{fixture['id']}"
                    if args.soak_hours
                    else fixture["id"]
                )
                result_path = campaign / (case_id + ".json")
                if result_path.exists():
                    previous = json.loads(result_path.read_text())
                    outcomes.append(previous)
                    if previous.get("capacity_reclaimed") is None:
                        pending_cleanup.append((result_path, previous))
                    continue
                receipt = (
                    args.corpus
                    / fixture.get("validation_id", fixture["id"])
                    / "receipt.json"
                )
                if (
                    not receipt.exists()
                    or json.loads(receipt.read_text()).get("status") != "valid"
                ):
                    outcomes.append(
                        dict(
                            fixture=fixture["id"],
                            status="not_run",
                            failure="fixture_not_validated",
                        )
                    )
                    continue
                if not args.persistent_backend:
                    write_json(selection, [fixture])
                case_seconds = fixture.get("play_seconds", args.seconds)
                config = campaign / "browser.json"
                write_json(
                    config,
                    dict(
                        tv_root=str(args.tv_root),
                        origin=args.origin,
                        fixture=fixture,
                        seconds=case_seconds,
                        result=str(result_path),
                        driver=args.driver,
                        player_profile=fixture.get(
                            "player_profile", args.player_profile
                        ),
                        capture_media=args.capture_media,
                        browser_executable=str(browser_executable),
                        request_ca=str(args.certs / "ca.crt"),
                        observer_config=str(run / (".observer-" + args.phase + ".json")),
                        **(
                            {
                                "login": {
                                    "username": "member1",
                                    "password": browser_password,
                                }
                            }
                            if browser_password
                            else {}
                        ),
                    ),
                )
                env = dict(
                    os.environ,
                    VIPTV_MEDIA_LAB_CONFIG=str(backend_config),
                    VIPTV_AUTH_ORIGIN=args.origin,
                )
                with (
                    contextlib.nullcontext(persistent_backend)
                    if persistent_backend
                    else child(
                        [
                            str(args.backend_binary),
                            "gateway::media_lab_acceptance::media_lab_backend",
                            "--ignored",
                            "--exact",
                            "--nocapture",
                            "--test-threads=1",
                        ],
                        campaign / (fixture["id"] + ".backend.log"),
                        env,
                    )
                ) as backend:
                    wait_http(f"http://127.0.0.1:{backend_port}/api/health", backend)
                    backend_before = process_usage(backend.pid)
                    with (campaign / (case_id + ".log")).open("wb") as log:
                        try:
                            subprocess.run(
                                ([sys.executable, str(HERE / "roku_flow.py"), str(config)]
                                 if args.driver == "roku-api" else
                                 ["node", str(HERE / "browser.mjs"), str(config)]),
                                stdout=log,
                                stderr=log,
                                timeout=case_seconds + 150,
                                check=False,
                                env=dict(
                                    os.environ,
                                    NODE_EXTRA_CA_CERTS=str(args.certs / "ca.crt"),
                                ),
                            )
                        except subprocess.TimeoutExpired:
                            write_json(
                                result_path,
                                dict(
                                    fixture=fixture["id"],
                                    status="failed",
                                    failure="browser_deadline",
                                ),
                            )
                    backend_after = process_usage(backend.pid)
                if not result_path.exists():
                    write_json(
                        result_path,
                        dict(
                            fixture=fixture["id"],
                            status="failed",
                            failure="browser_process_no_receipt",
                        ),
                    )
                result = json.loads(result_path.read_text())
                result["source_receipt_sha256"] = digest(receipt)
                if fixture.get("source_fixture"):
                    result["fixture"] = fixture["source_fixture"]
                    result.update(
                        {key: fixture[key] for key in ("trial", "cache", "profile")}
                    )
                result["gateway_trace"] = (
                    str(
                        int.from_bytes(
                            hashlib.sha256(fixture["id"].encode()).digest()[:8], "big"
                        )
                    )
                    if not args.persistent_backend
                    else None
                )
                result["source_query_sha256"] = hashlib.sha256(
                    urllib.parse.urlsplit(fixture["file"]).query.encode()
                ).hexdigest()[:16]
                result["case_id"] = case_id
                result["backend_resources"] = {
                    "before": backend_before,
                    "after": backend_after,
                }
                result["capacity_reclaimed"] = None
                result["validation_id"] = fixture.get("validation_id", fixture["id"])
                result["case_seconds"] = case_seconds
                write_json(result_path, result)
                outcomes.append(result)
                executed.append(result)
                pending_cleanup.append((result_path, result))
                if len(pending_cleanup) >= args.cleanup_stride:
                    if not cleanup_checkpoint(
                        run, pending_cleanup, campaign, bool(args.soak_hours)
                    ):
                        raise RuntimeError("cleanup checkpoint failed")
                write_json(
                    campaign / "progress.json",
                    dict(
                        completed=len(outcomes),
                        planned=None if args.soak_hours else len(fixtures),
                        passed=sum(r["status"] == "passed" for r in outcomes),
                        failed=sum(r["status"] == "failed" for r in outcomes),
                        elapsed_seconds=time.monotonic() - invocation_started,
                    ),
                )
                print(
                    json.dumps(
                        dict(
                            fixture=fixture["id"],
                            status=result["status"],
                            completed=len(outcomes),
                            planned=None if args.soak_hours else len(fixtures),
                        )
                    ),
                    flush=True,
                )
            cleanup_checkpoint(run, pending_cleanup, campaign, bool(args.soak_hours))
            backend_continuity_ok = (
                persistent_backend is not None and persistent_backend.poll() is None
            )
    finally:
        alias.unlink(missing_ok=True)
        summary = dict(
            phase=args.phase,
            planned=len(outcomes) if args.soak_hours else len(fixtures),
            total_manifest=len(manifest["fixtures"]),
            passed=sum(o["status"] == "passed" for o in outcomes),
            failed=sum(o["status"] == "failed" for o in outcomes),
            not_run=(len(outcomes) if args.soak_hours else len(fixtures))
            - sum(o["status"] in ("passed", "failed") for o in outcomes),
            full_corpus=not args.soak_hours
            and len(fixtures) == len(manifest["fixtures"]),
            outcomes=[{k: o[k] for k in ("fixture", "status")} for o in outcomes],
        )
        summary["accepted"] = (
            summary["full_corpus"]
            and summary["passed"] == summary["planned"]
            and all(o.get("capacity_reclaimed") for o in outcomes)
        )
        summary["browser_binary_unchanged"] = (
            digest(browser_executable) == browser_sha256
        )
        summary["browser_bundle_unchanged"] = frozen["browser_bundle_sha256"] == {
            str(p.relative_to(web_dist)): digest(p)
            for p in sorted(web_dist.rglob("*"))
            if p.is_file()
        }
        summary["browser_automation_unchanged"] = frozen[
            "browser_automation_sha256"
        ] == digest(args.tv_root / "node_modules/playwright/package.json") and frozen[
            "browser_registry_sha256"
        ] == digest(args.tv_root / "node_modules/playwright-core/browsers.json")
        summary["accepted"] &= summary["browser_binary_unchanged"]
        summary["accepted"] &= summary["browser_bundle_unchanged"]
        summary["accepted"] &= summary["browser_automation_unchanged"]
        elapsed = time.monotonic() - invocation_started
        playback = sum(
            o.get("metrics", {}).get("observed_seconds", 0) for o in executed
        )
        summary["continuous_run"] = dict(
            started_at=invocation_wall,
            elapsed_seconds=elapsed,
            observed_playback_seconds=playback,
            live_cases=sum(bool(o.get("delivery", {}).get("live")) for o in executed),
            vod_cases=sum(o.get("delivery", {}).get("live") is False for o in executed),
            cleanup_complete=all(o.get("capacity_reclaimed") for o in executed),
            resumed_cases=len(outcomes) - len(executed),
        )
        if args.soak_hours:
            summary.update(
                complete=elapsed >= args.soak_hours * 3600
                and not summary["not_run"]
                and backend_continuity_ok,
                elapsed_seconds=elapsed,
                observed_playback_seconds=playback,
                target_seconds=args.soak_hours * 3600,
                backend_pid=persistent_backend.pid if persistent_backend else None,
                backend_continuous=backend_continuity_ok,
                cleanup_complete=summary["continuous_run"]["cleanup_complete"],
            )
        write_json(campaign / "summary.json", summary)
        lock.close()
    return 0 if summary["passed"] == summary["planned"] else 1


def main():
    os.umask(0o077)
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest="action", required=True)
    s = sub.add_parser("start")
    s.add_argument("--run", type=Path, required=True)
    s.add_argument("--corpus", type=Path, required=True)
    s.add_argument("--binary", type=Path, required=True)
    s.add_argument("--image", default="playback-media-lab:20261023")
    s.add_argument("--device", type=Path)
    s.add_argument("--cpus", type=float, default=4, help="frozen lab CPU budget; controlled campaigns default to four")
    s.add_argument(
        "--public-https-egress",
        action="store_true",
        help="opt in to bridge egress for external HTTPS fixtures; never a controlled-origin performance run",
    )
    s.add_argument(
        "--hardware", choices=["software", "qsv", "vaapi"], default="software"
    )
    s.add_argument(
        "--vaapi-with-qsv",
        action="store_true",
        help="enable the same Intel device for qualified HDR processing/fallback",
    )
    s = sub.add_parser("stop")
    s.add_argument("--run", type=Path, required=True)
    s = sub.add_parser("run")
    s.add_argument("--run", type=Path, required=True)
    s.add_argument("--corpus", type=Path, required=True)
    s.add_argument("--backend-binary", type=Path, required=True)
    s.add_argument("--tv-root", type=Path, required=True)
    s.add_argument("--web-dist", type=Path, help="isolated current browser build")
    s.add_argument(
        "--browser-executable",
        type=Path,
        help="pinned copy of the actual browser executable",
    )
    s.add_argument(
        "--driver", choices=["current-api", "product", "kmp", "roku-api"], default="current-api"
    )
    s.add_argument(
        "--player-profile", choices=["browser", "mse", "tv-html"], default="browser"
    )
    s.add_argument("--certs", type=Path, required=True)
    s.add_argument("--origin", default="https://viptv.local.test:18449")
    s.add_argument(
        "--phase", choices=["baseline", "candidate", "jellyfin"], required=True
    )
    s.add_argument("--seconds", type=int, default=30)
    s.add_argument("--limit", type=int)
    s.add_argument("--ids")
    s.add_argument(
        "--capture-media",
        action="store_true",
        help="retain bounded output samples for focused correctness/quality checks; disabled during performance trials",
    )
    s.add_argument(
        "--campaign-name",
        help="separate evidence directory for timing and continuous-soak stages",
    )
    s.add_argument(
        "--persistent-backend",
        action="store_true",
        help="keep one real backend and fixed catalog for the entire campaign",
    )
    s.add_argument(
        "--soak-hours",
        type=float,
        default=0,
        help="repeat a small fixed catalog continuously; requires --persistent-backend and a fresh phase",
    )
    s.add_argument(
        "--cleanup-stride",
        type=int,
        choices=[1, 10, 20],
        default=1,
        help="normal-grace reclamation checkpoint frequency; no service restart or clock manipulation",
    )
    s.add_argument(
        "--manifest",
        type=Path,
        help="supplemental manifest; payloads still live under --corpus",
    )
    args = p.parse_args()
    if args.action == "start":
        start(args)
    elif args.action == "stop":
        identity = (args.run / "container-id").read_text().strip()
        assert len(identity) == 64 and all(c in "0123456789abcdef" for c in identity)
        subprocess.run(
            ["sudo", "-n", "docker", "stop", "--time", "20", identity], check=True
        )
    else:
        raise SystemExit(run_campaign(args))


if __name__ == "__main__":
    main()
