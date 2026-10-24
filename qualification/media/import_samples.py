#!/usr/bin/env python3
"""Cache explicitly selected public test samples privately; never publish payloads."""

import argparse
import json
from pathlib import Path
import shutil
import subprocess
import urllib.parse
import urllib.request
from corpus import digest, write_json, BUDGET


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--corpus", type=Path, required=True)
    p.add_argument("--catalog", type=Path, required=True)
    p.add_argument("--ids", required=True)
    p.add_argument("--ffmpeg", default="ffmpeg")
    p.add_argument("--ffprobe", default="ffprobe")
    args = p.parse_args()
    catalog = json.loads(args.catalog.read_text())
    wanted = set(args.ids.split(","))
    selected = [
        a for a in catalog["assets"] if a["id"] in wanted and a["status"] == "verified"
    ]
    assert len(selected) == len(wanted), "only verified catalog assets may be imported"
    fixtures = []
    for asset in selected:
        directory = args.corpus / asset["id"]
        directory.mkdir(parents=True, exist_ok=True)
        suffix = Path(urllib.parse.urlsplit(asset["url"]).path).suffix
        assert suffix.lower() in (
            ".wmv",
            ".m2ts",
            ".mkv",
            ".mp4",
            ".mov",
            ".webm",
            ".ogv",
        )
        target = directory / ("media" + suffix)
        if not target.exists():
            used = sum(f.stat().st_size for f in args.corpus.rglob("*") if f.is_file())
            assert (
                used + 512 * 1024**2 < BUDGET
                and shutil.disk_usage(args.corpus).free > 12 * 1024**3
            ), "corpus storage reserve"
            partial = target.with_suffix(target.suffix + ".partial")
            request = urllib.request.Request(
                asset["url"], headers={"User-Agent": "VIPTV-Media-Qualification/1.0"}
            )
            try:
                with (
                    urllib.request.urlopen(request, timeout=30) as response,
                    partial.open("wb") as output,
                ):
                    assert urllib.parse.urlsplit(response.url).scheme == "https"
                    size = 0
                    while block := response.read(1024 * 1024):
                        size += len(block)
                        assert size <= 512 * 1024**2, (
                            "sample exceeds private-cache bound"
                        )
                        output.write(block)
                partial.rename(target)
            finally:
                partial.unlink(missing_ok=True)
        metadata = json.loads(
            subprocess.check_output(
                [
                    args.ffprobe,
                    "-v",
                    "error",
                    "-show_streams",
                    "-show_format",
                    "-of",
                    "json",
                    str(target),
                ],
                timeout=60,
            )
        )
        video = next(s for s in metadata["streams"] if s["codec_type"] == "video")
        audio = next(
            (s for s in metadata["streams"] if s["codec_type"] == "audio"), None
        )
        frames = json.loads(
            subprocess.check_output(
                [
                    args.ffprobe,
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-read_intervals",
                    "%+1",
                    "-show_frames",
                    "-show_entries",
                    "frame=interlaced_frame",
                    "-of",
                    "json",
                    str(target),
                ],
                timeout=60,
            )
        )
        with (directory / "validate.log").open("wb") as log:
            subprocess.run(
                [
                    args.ffmpeg,
                    "-nostdin",
                    "-v",
                    "error",
                    "-xerror",
                    "-threads",
                    "2",
                    "-i",
                    str(target),
                    "-map",
                    "0:v:0",
                    "-map",
                    "0:a?",
                    "-f",
                    "null",
                    "-",
                ],
                stdout=log,
                stderr=log,
                timeout=600,
                check=True,
            )
        item = dict(
            id=asset["id"],
            file=str(target.relative_to(args.corpus)),
            container=metadata["format"]["format_name"],
            video=video["codec_name"],
            audio=audio["codec_name"] if audio else None,
            width=video["width"],
            height=video["height"],
            rate=video["r_frame_rate"],
            duration=float(metadata["format"]["duration"]),
            multi_audio=False,
            subtitles=False,
            timing_markers=False,
            interlaced=any(f.get("interlaced_frame") for f in frames.get("frames", [])),
            source_url=asset["url"],
            license=asset["license"],
            private_cache_only=True,
            public_redistribution_allowed=False,
        )
        write_json(
            directory / "receipt.json",
            dict(
                id=item["id"],
                status="valid",
                probe=metadata,
                provenance=asset,
                validation="full FFmpeg decode",
                files={target.name: digest(target)},
            ),
        )
        fixtures.append(item)
        print(
            json.dumps(
                dict(
                    id=item["id"],
                    status="validated",
                    video=item["video"],
                    audio=item["audio"],
                )
            ),
            flush=True,
        )
    write_json(args.corpus / "public-samples.json", dict(version=1, fixtures=fixtures))


if __name__ == "__main__":
    main()
