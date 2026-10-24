#!/usr/bin/env python3
"""Keep capacity-test decoders off the gateway host, behind trusted HTTPS."""

import argparse
import json
from pathlib import Path
import subprocess

from corpus import digest, write_json
from lab import child, wait_http


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--run", type=Path, required=True)
    p.add_argument("--corpus", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--observer-config", type=Path, required=True)
    p.add_argument("--certs", type=Path, required=True)
    p.add_argument("--origin", required=True)
    p.add_argument("--ffmpeg", default="/usr/bin/ffmpeg")
    p.add_argument("--seconds", type=int, default=60)
    p.add_argument("--workload", choices=["formats", "1080p", "4k"], default="formats")
    p.add_argument("--live", action="store_true")
    p.add_argument("--live-manifest", type=Path)
    args = p.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    assert (
        args.origin.startswith("https://")
        and args.origin.split("//")[1].split(":")[0] == "viptv.local.test"
    )
    binary = Path(args.ffmpeg).resolve()
    receipt = dict(
        binary_sha256=digest(binary),
        version=subprocess.check_output([str(binary), "-version"], text=True),
        decoder_location="independent workstation",
        gateway_limits=json.loads((args.run / "provenance.json").read_text()),
    )
    record = args.output / "consumer-build.json"
    if record.exists():
        assert json.loads(record.read_text()) == receipt, "decoder build changed"
    else:
        write_json(record, receipt)
    config = args.output / "Caddyfile"
    socket = (args.run / "gateway.sock").resolve()
    config.write_text(f"""{{
 admin off
 auto_https disable_redirects
 log default {{
  output discard
 }}
}}
{args.origin} {{
 bind 127.0.0.1
 tls {args.certs / "viptv.local.test.crt"} {args.certs / "viptv.local.test.key"}
 handle_path /gateway/* {{
  reverse_proxy unix/{socket}
 }}
}}
""")
    command = [
        "python3",
        str(Path(__file__).with_name("load.py")),
        "--run",
        str(args.run),
        "--corpus",
        str(args.corpus),
        "--output",
        str(args.output),
        "--consumer-origin",
        args.origin,
        "--consumer-ffmpeg",
        str(binary),
        "--observer-config",
        str(args.observer_config),
        "--seconds",
        str(args.seconds),
        "--workload",
        args.workload,
    ]
    if args.live:
        command.append("--live")
    if args.live_manifest:
        command += ["--live-manifest", str(args.live_manifest)]
    with child(
        ["caddy", "run", "--config", str(config)], args.output / "caddy.log"
    ) as ingress:
        wait_http(args.origin + "/gateway/ready", ingress)
        result = subprocess.run(command)
    if digest(binary) != receipt["binary_sha256"]:
        raise SystemExit("decoder changed during the campaign")
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
