#!/usr/bin/env python3
"""Inspect a candidate without weakening the service's media-binary requirements."""

import argparse
import json
from pathlib import Path
import subprocess
from corpus import digest, write_json

RELEASE = "v8.1.3-1"
PACKAGE_URL = "https://github.com/jellyfin/jellyfin-ffmpeg/releases/download/v8.1.3-1/jellyfin-ffmpeg8_8.1.3-1-trixie_amd64.deb"
PACKAGE_SHA256 = "fbef9f81a53e175194e3f67832618a86111f7bd37197df21a5d748c34289f9c0"


def inspect(binary):
    result = {"path": str(binary), "sha256": digest(binary), "requirements": {}}

    def call(*args):
        p = subprocess.run(
            [str(binary), "-hide_banner", *args],
            capture_output=True,
            text=True,
            timeout=20,
        )
        if p.returncode:
            raise RuntimeError("candidate capability command failed")
        return p.stdout + p.stderr

    try:
        result["version"] = call("-version").splitlines()[0]
        build = call("-buildconf")
        tls = call("-h", "protocol=tls")
        http = call("-h", "protocol=http")
        result["buildconf"] = build
        result["requirements"] = {
            "gnutls": "--enable-gnutls" in build,
            "verify_tls_by_default": any(
                "-tls_verify" in l and "(default true)" in l for l in tls.splitlines()
            ),
            "finite_http_ranges": "-request_size" in http
            and "-initial_request_size" in http,
        }
        if binary.name == "ffmpeg":
            encoders = call("-encoders")
            filters = call("-filters")
            result["requirements"].update(
                h264_software="libx264" in encoders,
                h264_qsv="h264_qsv" in encoders,
                h264_vaapi="h264_vaapi" in encoders,
                aac=" AAC " in encoders or " aac " in encoders,
                zscale=" zscale " in filters,
                tonemap=" tonemap " in filters,
            )
            result["requirements"].update(
                dash_demuxer=any(
                    line.split()[1:2] == ["dash"]
                    for line in call("-demuxers").splitlines()
                ),
                software_av1="libdav1d" in call("-decoders"),
            )
    except (OSError, subprocess.SubprocessError, RuntimeError) as e:
        result["failure"] = type(e).__name__
    result["compatible"] = (
        bool(result["requirements"])
        and all(result["requirements"].values())
        and "failure" not in result
    )
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--directory", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    args = p.parse_args()
    results = [inspect(args.directory / name) for name in ("ffmpeg", "ffprobe")]
    result = dict(
        release=RELEASE,
        package_url=PACKAGE_URL,
        package_sha256=PACKAGE_SHA256,
        binaries=results,
        compatible=all(r["compatible"] for r in results),
        default_changed=False,
    )
    write_json(args.output, result)
    print(
        json.dumps(
            {
                "compatible": result["compatible"],
                "requirements": [r["requirements"] for r in results],
            }
        )
    )
    raise SystemExit(0 if result["compatible"] else 1)


if __name__ == "__main__":
    main()
