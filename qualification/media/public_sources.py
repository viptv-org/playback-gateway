#!/usr/bin/env python3
"""Discover public demo URLs and verify actual media; never stress third-party origins."""

import argparse
import concurrent.futures
import hashlib
import json
from pathlib import Path
import re
import subprocess
import time
import urllib.parse
import urllib.request
from corpus import write_json

CATALOGS = [
    "https://raw.githubusercontent.com/shaka-project/shaka-player/main/demo/common/assets.js",
    "https://bitmovin.com/blog/mpeg-dash-hls-examples-sample-streams/",
    "https://media.xiph.org/video/misc/",
    "https://download.blender.org/durian/trailer/",
    "https://raw.githubusercontent.com/video-dev/hls.js/master/tests/test-streams.js",
]


def discover():
    found = {
        url: dict(
            id="public-" + hashlib.sha256(url.encode()).hexdigest()[:16],
            url=url,
            catalog="https://samples.ffmpeg.org/V-codecs/WVC1/",
            license="reference-only; redistribution not established",
            mirror_allowed=False,
            status="unverified",
        )
        for url in (
            "https://samples.ffmpeg.org/V-codecs/WVC1/FlightSimX_720p60_51_15Mbps.wmv",
            "https://samples.ffmpeg.org/V-codecs/WVC1/Test_1440x576_WVC1_6Mbps.wmv",
            "https://samples.ffmpeg.org/V-codecs/WVC1/vc1-interlaced-bframes.m2ts",
        )
    }
    for catalog in CATALOGS:
        request = urllib.request.Request(
            catalog, headers={"User-Agent": "VIPTV-Media-Qualification/1.0"}
        )
        try:
            with urllib.request.urlopen(request, timeout=30) as r:
                content = r.read(4 * 1024 * 1024).decode(errors="replace")
        except OSError:
            continue
        if catalog.endswith("assets.js"):
            blocks = content.split("new ShakaDemoAssetInfo(")[1:]
            urls = []
            for block in blocks:
                if any(
                    word in block
                    for word in (
                        ".addKeySystem(",
                        ".setLicenseServer(",
                        ".setAdTagUri(",
                    )
                ):
                    continue
                match = re.search(r"/\* manifestUri= \*/\s*['\"]([^'\"]+)", block)
                if match:
                    urls.append(match[1])
        else:
            urls = re.findall(r"""(?:href|src)=["']([^"']+)["']""", content)
            urls += re.findall(
                r"""https://[^\s<>"']+\.(?:mpd|m3u8|mp4|webm|ogv|mkv)(?:\?[^\s<>"']*)?""",
                content,
            )
        for raw in urls:
            url = urllib.parse.urljoin(catalog, raw.replace("&amp;", "&"))
            parts = urllib.parse.urlsplit(url)
            if (
                parts.scheme != "https"
                or parts.username
                or not re.search(
                    r"\.(mpd|m3u8|mp4|webm|ogv|mkv|mov)$", parts.path, re.I
                )
            ):
                continue
            found.setdefault(
                url,
                dict(
                    id="public-" + hashlib.sha256(url.encode()).hexdigest()[:16],
                    url=url,
                    catalog=catalog,
                    license="reference-only; redistribution requires asset-specific license verification",
                    mirror_allowed=False,
                    status="unverified",
                ),
            )
    return list(found.values())


def verify(asset, ffprobe, ffmpeg):
    result = dict(asset, checked_at=time.time())
    started = time.monotonic()
    try:
        req = urllib.request.Request(
            asset["url"],
            headers={
                "Range": "bytes=0-4095",
                "User-Agent": "VIPTV-Media-Qualification/1.0",
            },
        )
        with urllib.request.urlopen(req, timeout=20) as r:
            assert urllib.parse.urlsplit(r.url).scheme == "https", "non-HTTPS redirect"
            first = r.read(4096)
            result.update(
                http_status=r.status,
                final_url=r.url,
                initial_bytes_sha256=hashlib.sha256(first).hexdigest(),
            )
            if b"ContentProtection" in first or b"SAMPLE-AES" in first:
                result["status"] = "protected"
                return result
        p = subprocess.run(
            [
                ffprobe,
                "-v",
                "error",
                "-tls_verify",
                "1",
                "-rw_timeout",
                "10000000",
                "-show_streams",
                "-show_format",
                "-of",
                "json",
                asset["url"],
            ],
            capture_output=True,
            timeout=30,
            check=True,
        )
        probe = json.loads(p.stdout)
        assert any(s.get("codec_type") == "video" for s in probe["streams"])
        subprocess.run(
            [
                ffmpeg,
                "-nostdin",
                "-v",
                "error",
                "-xerror",
                "-tls_verify",
                "1",
                "-rw_timeout",
                "10000000",
                "-threads",
                "2",
                "-i",
                asset["url"],
                "-t",
                "2",
                "-map",
                "0:v:0",
                "-map",
                "0:a:0?",
                "-f",
                "null",
                "-",
            ],
            capture_output=True,
            timeout=30,
            check=True,
        )
        result.update(
            status="verified",
            streams=[
                {
                    k: s[k]
                    for k in (
                        "codec_type",
                        "codec_name",
                        "width",
                        "height",
                        "pix_fmt",
                        "sample_rate",
                        "channels",
                    )
                    if k in s
                }
                for s in probe["streams"]
            ],
        )
    except (
        OSError,
        subprocess.SubprocessError,
        ValueError,
        AssertionError,
        KeyError,
    ) as e:
        result.update(status="unavailable", failure=type(e).__name__)
    result["verification_seconds"] = time.monotonic() - started
    return result


def unique_verified(results):
    seen = set()
    count = 0
    for row in results:
        if row["status"] != "verified":
            continue
        canonical = row.get("final_url", row["url"])
        if canonical not in seen:
            count += 1
            seen.add(canonical)
    return count


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--ffmpeg", default="ffmpeg")
    p.add_argument("--ffprobe", default="ffprobe")
    p.add_argument("--limit", type=int, default=300)
    args = p.parse_args()
    assets = discover()[: args.limit]
    previous = {}
    if args.output.exists():
        previous = {x["url"]: x for x in json.loads(args.output.read_text())["assets"]}
    results = []
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        for result in pool.map(
            lambda a: previous.get(a["url"]) or verify(a, args.ffprobe, args.ffmpeg),
            assets,
        ):
            results.append(result)
            write_json(
                args.output,
                dict(
                    target_verified=100,
                    discovered=len(assets),
                    verified=unique_verified(results),
                    assets=results,
                ),
            )
            print(json.dumps({k: result[k] for k in ("id", "status")}), flush=True)


if __name__ == "__main__":
    main()
