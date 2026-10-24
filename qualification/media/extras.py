#!/usr/bin/env python3
"""Long VOD and multi-rendition fixtures; keeps the frozen main matrix unchanged."""

import argparse
import json
from pathlib import Path
import subprocess
import shutil
from corpus import digest, write_json, BUDGET


def create(args, item, command):
    directory = args.corpus / item["id"]
    directory.mkdir(exist_ok=True)
    receipt = directory / "receipt.json"
    if receipt.exists():
        previous = json.loads(receipt.read_text())
        if (
            previous.get("status") == "valid"
            and previous.get("recipe") == item
            and all(
                digest(directory / name) == value
                for name, value in previous["files"].items()
            )
        ):
            return
    used = sum(p.stat().st_size for p in args.corpus.rglob("*") if p.is_file())
    source = Path(command[command.index("-i") + 1])
    reserve = max(4 * 1024**3, int(source.stat().st_size * item["duration"] / 36 * 1.5))
    assert (
        used + reserve < BUDGET
        and shutil.disk_usage(args.corpus).free > reserve + 12 * 1024**3
    ), "corpus storage reserve reached"
    with (directory / "generate.log").open("wb") as log:
        subprocess.run(command, stdout=log, stderr=log, check=True, timeout=1800)
    target = args.corpus / item["file"]
    p = subprocess.run(
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
        capture_output=True,
        check=True,
        timeout=60,
    )
    metadata = json.loads(p.stdout)
    with (directory / "validate.log").open("wb") as log:
        # Every source was fully decoded already. Copy-extended files are fully
        # demuxed here; encoding/ABR output additionally receives a full decode.
        cmd = [
            args.ffmpeg,
            "-nostdin",
            "-v",
            "error",
            "-xerror",
            "-i",
            str(target),
            "-map",
            "0:v",
            "-map",
            "0:a?",
        ]
        if item["duration"] > 120:
            cmd += ["-c", "copy"]
        cmd += ["-f", "null", "-"]
        subprocess.run(cmd, stdout=log, stderr=log, check=True, timeout=1800)
    write_json(
        receipt,
        dict(
            id=item["id"],
            status="valid",
            recipe=item,
            argv=command,
            probe=metadata,
            validation="full demux of copied validated source"
            if item["duration"] > 120
            else "full decode",
            files={
                p.name: digest(p)
                for p in sorted(directory.iterdir())
                if p.suffix not in (".json", ".log")
            },
        ),
    )


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--corpus", type=Path, required=True)
    p.add_argument("--ffmpeg", default="ffmpeg")
    p.add_argument("--ffprobe", default="ffprobe")
    a = p.parse_args()
    items = json.loads((a.corpus / "manifest.json").read_text())["fixtures"]
    extra = []
    cases = [
        ("long-h264-1080", ("mp4", "libx264", "aac", 18)),
        ("long-hevc-4k", ("mkv", "libx265", "flac", 22)),
        ("long-hdr-1080", ("mkv", "libx265", "eac3", 19)),
        ("long-captions", ("mkv", "libx264", "flac", 14)),
    ]
    for name, key in cases:
        original = next(
            i
            for i in items
            if (i["container"], i["video"], i["audio"], i["variant"]) == key
        )
        receipt = a.corpus / original["id"] / "receipt.json"
        assert (
            receipt.exists() and json.loads(receipt.read_text())["status"] == "valid"
        ), "source not yet validated: " + original["id"]
        if original.get("color_recipe_version", 0) >= 2:
            name += "-v2"
        item = dict(
            original,
            id=name,
            file=f"{name}/media.{original['container']}",
            duration=1860,
            source_sha256=digest(a.corpus / original["file"]),
        )
        create(
            a,
            item,
            [
                a.ffmpeg,
                "-nostdin",
                "-v",
                "error",
                "-y",
                "-stream_loop",
                "-1",
                "-i",
                str(a.corpus / original["file"]),
                "-map",
                "0",
                "-c",
                "copy",
                "-t",
                "1860",
                str(a.corpus / item["file"]),
            ],
        )
        extra.append(item)
    original = next(
        i
        for i in items
        if (i["container"], i["video"], i["audio"], i["variant"])
        == ("mp4", "libx264", "aac", 18)
    )
    name = "extra-hls-master"
    item = dict(
        original,
        id=name,
        file=name + "/media.m3u8",
        container="hls-master",
        source_sha256=digest(a.corpus / original["file"]),
    )
    root = a.corpus / name
    create(
        a,
        item,
        [
            a.ffmpeg,
            "-nostdin",
            "-v",
            "error",
            "-y",
            "-filter_complex_threads",
            "2",
            "-i",
            str(a.corpus / original["file"]),
            "-filter_complex",
            "[0:v]split=2[lo][hi];[lo]scale=854:480[v0];[hi]scale=1920:1080[v1]",
            "-map",
            "[v0]",
            "-map",
            "0:a:0",
            "-map",
            "[v1]",
            "-map",
            "0:a:0",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-threads",
            "2",
            "-g",
            "48",
            "-c:a",
            "aac",
            "-f",
            "hls",
            "-hls_time",
            "2",
            "-hls_list_size",
            "0",
            "-hls_playlist_type",
            "vod",
            "-var_stream_map",
            "v:0,a:0 v:1,a:1",
            "-master_pl_name",
            "media.m3u8",
            "-hls_segment_filename",
            str(root / "segment-%v-%05d.ts"),
            str(root / "variant-%v.m3u8"),
        ],
    )
    extra.append(item)
    write_json(a.corpus / "extras.json", dict(version=1, fixtures=extra))


if __name__ == "__main__":
    main()
