#!/usr/bin/env python3
"""Compare real public HDR output against frozen baseline pixels or a software reference."""

import argparse
import hashlib
import json
import math
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

from corpus import digest, write_json


def media(path, row, temp):
    directory = Path(str(path) + ".media")
    files = {sample["file"]: sample for sample in row.get("media_samples", [])}
    segments = sorted(name for name in files if name.startswith("segment-"))
    if not segments or not segments[0].startswith("segment-000000000."):
        raise ValueError("first rendered output segment was not retained")
    name = segments[0]
    source = directory / name
    if digest(source) != files[name]["sha256"]:
        raise ValueError("output sample checksum changed")
    if name.endswith(".m4s"):
        init = directory / "init.mp4"
        if "init.mp4" not in files or digest(init) != files["init.mp4"]["sha256"]:
            raise ValueError("fMP4 initialization missing or changed")
        identity = hashlib.sha256(str(path.resolve()).encode()).hexdigest()[:12]
        joined = temp / (path.stem + "-" + identity + ".mp4")
        joined.write_bytes(init.read_bytes() + source.read_bytes())
        return joined
    return source


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--baseline", type=Path, required=True)
    p.add_argument("--candidate", type=Path, required=True)
    p.add_argument("--corpus", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--ffmpeg", default="ffmpeg")
    p.add_argument("--ffprobe", default="ffprobe")
    p.add_argument(
        "--ids",
        help="focused correctness subset; full HDR coverage remains required for acceptance",
    )
    args = p.parse_args()
    assert args.baseline.resolve() != args.candidate.resolve(), (
        "quality references must be independent"
    )
    manifest = json.loads((args.corpus / "manifest.json").read_text())
    results = []
    for fixture in manifest["fixtures"]:
        if fixture["transfer"] == "bt709":
            continue
        if args.ids and fixture["id"] not in args.ids.split(","):
            continue
        path = args.candidate / (fixture["id"] + ".json")
        before = args.baseline / path.name
        result = dict(
            fixture=fixture["id"],
            hdr_source=True,
            color_metadata_correct=False,
            reference_comparison_passed=False,
            status="failed",
        )
        try:
            row = json.loads(path.read_text())
            assert row["status"] == "passed", "candidate browser playback failed"
            with tempfile.TemporaryDirectory(prefix="media-quality-") as tmp:
                temp = Path(tmp)
                actual = media(path, row, temp)
                info = json.loads(
                    subprocess.check_output(
                        [
                            args.ffprobe,
                            "-v",
                            "error",
                            "-show_streams",
                            "-of",
                            "json",
                            str(actual),
                        ],
                        timeout=30,
                    )
                )
                video = next(s for s in info["streams"] if s["codec_type"] == "video")
                result["color_metadata_correct"] = (
                    all(
                        video.get(k) == "bt709"
                        for k in ("color_transfer", "color_space", "color_primaries")
                    )
                    and video.get("pix_fmt") == "yuv420p"
                )
                width, height = video["width"], video["height"]
                result["width"] = width
                result["height"] = height
                scale = min(1, 1920 / fixture["width"], 1080 / fixture["height"])
                expected = (
                    max(2, int(fixture["width"] * scale) // 2 * 2),
                    max(2, int(fixture["height"] * scale) // 2 * 2),
                )
                assert (width, height) == expected, (
                    "output resolution changed from the negotiated source fit"
                )
                old = json.loads(before.read_text()) if before.exists() else {}
                reference_normalization = ""
                if old.get("media_samples"):
                    reference = media(before, old, temp)
                    threshold = 40
                    kind = "frozen baseline public output"
                else:
                    reference = temp / "software.nut"
                    threshold = 30
                    kind = "bounded linear-light software HDR reference; baseline did not play"
                    filt = f"zscale=transfer=linear:npl=100,format=gbrpf32le,zscale=w={width}:h={height}:filter=bilinear:primaries=bt709,tonemap=tonemap=mobius:desat=2,zscale=transfer=bt709:matrix=bt709:range=limited,format=yuv420p,sidedata=mode=delete,setsar=1"
                    subprocess.run(
                        [
                            args.ffmpeg,
                            "-nostdin",
                            "-v",
                            "error",
                            "-y",
                            "-i",
                            str(args.corpus / fixture["file"]),
                            "-vf",
                            filt,
                            "-frames:v",
                            "1",
                            "-an",
                            "-c:v",
                            "rawvideo",
                            "-threads",
                            "1",
                            str(reference),
                        ],
                        check=True,
                        capture_output=True,
                        timeout=60,
                    )
                    result["reference_filter"] = filt
                    # Rawvideo in NUT does not preserve these color properties.
                    # Without restoring the known software-reference properties,
                    # FFmpeg's automatic negotiation can convert the reference
                    # before PSNR and report a false color/quality regression.
                    # Never retag captured gateway/baseline output: its metadata
                    # is independent evidence checked above.
                    reference_normalization = (
                        "setparams=range=limited:color_primaries=bt709:"
                        "color_trc=bt709:colorspace=bt709,"
                    )
                    result["reference_color_properties_restored"] = True
                measured = subprocess.run(
                    [
                        args.ffmpeg,
                        "-nostdin",
                        "-v",
                        "error",
                        "-i",
                        str(reference),
                        "-i",
                        str(actual),
                        "-lavfi",
                        f"[0:v]{reference_normalization}setpts=PTS-STARTPTS[a];[1:v]setpts=PTS-STARTPTS[b];[a][b]psnr=stats_file=-",
                        "-frames:v",
                        "1",
                        "-an",
                        "-f",
                        "null",
                        "-",
                    ],
                    check=True,
                    capture_output=True,
                    text=True,
                    timeout=60,
                )
                match = re.search(r"psnr_avg:([\d.]+|inf)", measured.stdout)
                assert match, "no decoded pixel comparison"
                score = float(match[1])
                result.update(
                    reference_kind=kind,
                    psnr_db=score if math.isfinite(score) else None,
                    identical_pixels=math.isinf(score),
                    minimum_psnr_db=threshold,
                    reference_comparison_passed=score >= threshold,
                    public_sample_sha256=digest(actual),
                )
                result["status"] = (
                    "passed"
                    if result["color_metadata_correct"]
                    and result["reference_comparison_passed"]
                    else "failed"
                )
        except (
            OSError,
            ValueError,
            AssertionError,
            subprocess.SubprocessError,
            StopIteration,
        ) as error:
            result["failure"] = (
                str(error)
                if isinstance(error, (ValueError, AssertionError))
                else type(error).__name__
            )
        results.append(result)
    write_json(
        args.output,
        dict(
            analysis=dict(
                script_sha256=digest(Path(__file__)),
                ffmpeg_sha256=digest(Path(shutil.which(args.ffmpeg) or args.ffmpeg)),
                ffprobe_sha256=digest(Path(shutil.which(args.ffprobe) or args.ffprobe)),
            ),
            complete=len(results)
            == sum(i["transfer"] != "bt709" for i in manifest["fixtures"]),
            planned=len(results),
            passed=sum(r["status"] == "passed" for r in results),
            results=results,
            scope="first decoded frame of each HDR format, public response bytes, metadata and resolution; existing multi-frame HDR reference regressions remain required",
        ),
    )
    raise SystemExit(any(r["status"] != "passed" for r in results))


if __name__ == "__main__":
    main()
