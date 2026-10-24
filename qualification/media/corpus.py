#!/usr/bin/env python3
"""Deterministic, resumable media corpus. Generated payloads never belong in git."""

import argparse
import concurrent.futures
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import time

VERSION = 1
BUDGET = 250 * 1024**3
# Explicitly valid muxer/encoder combinations, not a container x codec product.
PROFILES = [
    ("mp4", "libx264", "aac"),
    ("mov", "libx264", "aac"),
    ("mkv", "libx264", "aac"),
    ("ts", "libx264", "aac"),
    ("m2ts", "libx264", "ac3"),
    ("mp4", "libx265", "aac"),
    ("mkv", "libx265", "eac3"),
    ("ts", "libx265", "ac3"),
    ("webm", "libvpx-vp9", "libopus"),
    ("webm", "libvpx", "libvorbis"),
    ("mkv", "libsvtav1", "libopus"),
    ("mp4", "libsvtav1", "aac"),
    ("avi", "mpeg4", "libmp3lame"),
    ("mkv", "mpeg4", "aac"),
    ("mpg", "mpeg2video", "mp2"),
    ("ts", "mpeg2video", "mp2"),
    ("avi", "mjpeg", "pcm_s16le"),
    ("mov", "prores_ks", "pcm_s16le"),
    ("mkv", "ffv1", "flac"),
    ("ogv", "libtheora", "libvorbis"),
    ("flv", "flv", "libmp3lame"),
    ("flv", "libx264", "aac"),
    ("mkv", "libx264", "ac3"),
    ("mkv", "libx264", "eac3"),
    ("mkv", "libx264", "dca"),
    ("mkv", "libx264", "truehd"),
    ("mkv", "libx264", "flac"),
    ("mkv", "libx264", "pcm_s16le"),
    ("mkv", "libx264", "libopus"),
    ("mkv", "libx264", "libvorbis"),
    ("mkv", "libx264", "libmp3lame"),
    ("mp4", "libx264", None),
    ("mkv", "libx265", "flac"),
    ("mov", "mpeg4", "aac"),
    ("fmp4", "libx264", "aac"),
    ("fmp4", "libx265", "aac"),
    ("hls-ts", "libx264", "aac"),
    ("hls-fmp4", "libx264", "aac"),
    ("dash", "libx264", "aac"),
    ("hls-ts", "mpeg2video", "mp2"),
    ("hls-fmp4", "libx265", "aac"),
    ("dash", "libsvtav1", "aac"),
    ("mkv", "libvpx-vp9", "flac"),
    ("webm", "libsvtav1", "libopus"),
]
EXT = {"fmp4": "mp4", "hls-ts": "m3u8", "hls-fmp4": "m3u8", "dash": "mpd"}


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def digest(path):
    h = hashlib.sha256()
    with Path(path).open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def write_json(path, value):
    path = Path(path)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def recipes():
    result = []
    for p, (container, video, audio) in enumerate(PROFILES):
        for variant in range(24):
            modern = video in ("libx264", "libx265", "libvpx-vp9", "libsvtav1")
            size = [(640, 360), (854, 480), (1280, 720)][variant % 3]
            if modern and variant in (18, 19, 20, 21):
                size = (1920, 1080)
            if modern and variant in (22, 23):
                size = (3840, 2160)
            rate = ["24000/1001", "25", "30000/1001", "30", "50", "60"][variant % 6]
            if video == "mpeg2video":
                rate = ["24000/1001", "25", "30000/1001", "30"][variant % 4]
            if video == "flv":
                size = (640, 480)
            hdr = video == "libx265" and variant in (19, 21, 23)
            item = dict(
                version=VERSION,
                container=container,
                video=video,
                audio=audio,
                width=size[0],
                height=size[1],
                rate=rate,
                duration=36,
                gop_seconds=[1, 2, 4][variant % 3],
                pixel_format="yuv420p10le" if hdr else "yuv420p",
                transfer=("smpte2084" if variant != 21 else "arib-std-b67")
                if hdr
                else "bt709",
                channels=1 if variant % 4 == 0 else 2,
                faststart=variant % 2 == 0,
                fragmented=container == "fmp4",
                pattern=variant % 4,
                variant=variant,
                interlaced=video in ("libx264", "mpeg2video") and variant == 17,
                variable_rate=video == "libx264"
                and container in ("mkv", "mp4", "mov")
                and variant == 16,
                multi_audio=container in ("mkv", "mp4", "mov")
                and audio is not None
                and variant == 15,
                subtitles=container == "mkv" and variant == 14,
                expected="play",
                license="generated; CC0-1.0",
                seed=1000 + p * 24 + variant,
            )
            if audio in ("ac3", "eac3", "flac", "truehd", "aac") and variant == 13:
                item["channels"] = 6
            if hdr:
                item["color_recipe_version"] = 2
            item["id"] = "media-" + hashlib.sha256(canonical(item)).hexdigest()[:16]
            item["file"] = f"{item['id']}/media.{EXT.get(container, container)}"
            result.append(item)
    return result


def representatives(items, count=40):
    """Deterministic greedy coverage of formats and expensive playback paths."""

    def features(item):
        return {
            (key, str(item[key]))
            for key in (
                "container",
                "video",
                "audio",
                "height",
                "rate",
                "transfer",
                "channels",
                "interlaced",
                "variable_rate",
                "multi_audio",
                "subtitles",
            )
        }

    remaining = {item["id"]: item for item in items}
    uncovered = set().union(*(features(i) for i in items))
    selected = []
    weights = {"container": 3, "video": 3, "audio": 2, "height": 3, "transfer": 4}
    while remaining and len(selected) < count:
        item = max(
            remaining.values(),
            key=lambda i: (
                sum(weights.get(key, 1) for key, _ in features(i) & uncovered),
                i["id"],
            ),
        )
        selected.append(item)
        uncovered -= features(item)
        remaining.pop(item["id"])
    return selected


def command(item, root, ffmpeg, master=None):
    """Return argv without a shell; all paths are owned by the caller."""
    target = root / item["file"]
    size = f"{item['width']}x{item['height']}"
    input_rate = (
        str(float_fraction(item["rate"]) * 2) if item["interlaced"] else item["rate"]
    )
    args = [
        ffmpeg,
        "-hide_banner",
        "-nostdin",
        "-v",
        "warning",
        "-y",
        "-filter_threads",
        "2",
    ]
    if master:
        args += ["-stream_loop", "-1", "-i", str(master)]
    else:
        # Moving, textured scenes plus an explicit one-second timing flash.
        pattern = ["testsrc2", "mandelbrot", "testsrc2", "testsrc"][item["pattern"]]
        args += ["-f", "lavfi", "-i", f"{pattern}=size={size}:rate={input_rate}"]
    if item["audio"]:
        # One pulse every second, with a continuous non-silent background.
        if master and not item.get("timing_identity"):
            args += ["-stream_loop", "-1", "-i", str(master)]
        else:
            args += [
                "-f",
                "lavfi",
                "-i",
                "aevalsrc=0.06*sin(2*PI*440*t)+0.3*sin(2*PI*1000*t)*lt(mod(t\\,1)\\,0.04):s=48000",
            ]
        if item["multi_audio"]:
            args += ["-f", "lavfi", "-i", "sine=frequency=880:sample_rate=48000"]
    if item.get("timing_identity"):
        assert not item["multi_audio"]
        pulse = "aevalsrc=0.02*sin(2*PI*440*t)+0.5*sin(2*PI*(1000+128*mod(floor(t)\\,64))*t)*lt(mod(t\\,1)\\,0.08):s=48000"
        if item["timing_identity"]["version"] == 2:
            pulse = "aevalsrc=0.02*sin(2*PI*440*t)+if(lt(mod(t\\,1)\\,0.08)\\,0.5*sin(2*PI*(1000+128*mod(floor(floor(t)/pow(16\\,floor(mod(t\\,1)/0.02)))\\,16))*t)\\,0):s=48000"
            offset = item["timing_identity"].get("clock_offset", 0)
            if offset:
                pulse = pulse.replace("floor(t)", f"floor(t+{offset})")
        args = [pulse if arg.startswith("aevalsrc=") else arg for arg in args]
    if item["subtitles"]:
        args += ["-i", str(target.parent / "captions.srt")]
    args += ["-map", "0:v:0"]
    if item["audio"]:
        args += ["-map", "1:a:0"]
        if item["multi_audio"]:
            args += [
                "-map",
                "2:a:0",
                "-metadata:s:a:0",
                "language=eng",
                "-metadata:s:a:1",
                "language=spa",
                "-disposition:a:0",
                "0",
                "-disposition:a:1",
                "default",
            ]
    if item["subtitles"]:
        args += ["-map", "2:s:0", "-c:s", "srt", "-metadata:s:s:0", "language=eng"]
    vf = [
        f"scale={item['width']}:{item['height']}",
        f"fps={input_rate}",
        f"noise=alls=3:allf=t+u:all_seed={item['seed']}",
        "drawbox=x=0:y=0:w=48:h=48:color=white:t=fill:enable=lt(mod(t\\,1)\\,0.04)",
    ]
    if item.get("timing_identity"):
        bits = item["timing_identity"]["bits"]
        step = 12 if item["timing_identity"]["version"] == 2 else 16
        vf[-1:] = [
            f"drawbox=x=0:y=0:w={128 if item['timing_identity']['version'] == 1 else 16 + step * bits + 8}:h=48:color=black:t=fill",
            "drawbox=x=8:y=32:w=8:h=8:color=white:t=fill:enable=lt(mod(t\\,1)\\,0.08)",
            *[
                f"drawbox=x={16 + step * bit}:y=8:w=8:h=8:color=white:t=fill:enable=gte(mod(floor(t/{2**bit})\\,2)\\,1)"
                for bit in range(bits)
            ],
        ]
        offset = item["timing_identity"].get("clock_offset", 0)
        if offset:
            vf = [f.replace("floor(t/", f"floor((t+{offset})/") for f in vf]
    if item["interlaced"]:
        vf += ["tinterlace=mode=interleave_top"]
    if item["variable_rate"]:
        vf += ["select=not(eq(mod(n\\,5)\\,0))"]
    if item["transfer"] != "bt709":
        vf += [
            f"setparams=range=limited:color_primaries=bt2020:color_trc={item['transfer']}:colorspace=bt2020nc"
        ]
    video = item["video"]
    pixel = (
        "yuv422p10le"
        if video == "prores_ks"
        else "yuvj420p"
        if video == "mjpeg"
        else item["pixel_format"]
    )
    args += [
        "-vf",
        ",".join(vf),
        "-pix_fmt",
        pixel,
        "-c:v",
        video,
        "-threads",
        "2",
        "-g",
        str(round(float_fraction(item["rate"]) * item["gop_seconds"])),
        "-t",
        str(item["duration"]),
    ]
    if item["variable_rate"]:
        args += ["-fps_mode", "vfr"]
    if video in ("libx264", "libx265"):
        args += [
            "-preset",
            "ultrafast" if item.get("live_clock") else "veryfast",
            "-crf",
            "23",
        ]
        if item.get("live_clock"):
            args += ["-sc_threshold", "0"]
        if video == "libx265":
            parameters = "pools=2:frame-threads=2:log-level=error"
            if item["transfer"] != "bt709":
                parameters += (
                    ":colorprim=9:transfer="
                    + ("16" if item["transfer"] == "smpte2084" else "18")
                    + ":colormatrix=9"
                )
                if item["transfer"] == "smpte2084":
                    parameters += ":hdr10=1:master-display=G(8500,39850)B(6550,2300)R(35400,14600)WP(15635,16450)L(100000000,1):max-cll=10000,10000"
            args += ["-x265-params", parameters]
    elif video == "libsvtav1":
        args += ["-preset", "12", "-crf", "35", "-svtav1-params", "lp=2"]
    elif video in ("libvpx", "libvpx-vp9"):
        args += ["-deadline", "realtime", "-cpu-used", "8", "-b:v", "4M"]
    elif video in ("mpeg2video", "mpeg4", "mjpeg", "flv", "libtheora"):
        args += ["-q:v", "4"]
    if item["interlaced"]:
        args += ["-flags", "+ilme+ildct", "-field_order", "tt"]
    if item["transfer"] != "bt709":
        args += [
            "-color_primaries",
            "bt2020",
            "-color_trc",
            item["transfer"],
            "-colorspace",
            "bt2020nc",
        ]
    if item["audio"]:
        args += [
            "-c:a",
            item["audio"],
            "-ac",
            str(item["channels"]),
            "-ar",
            "48000",
            "-strict",
            "-2",
        ]
    container = item["container"]
    if container in ("mp4", "mov", "fmp4"):
        args += ["-f", "mov" if container == "mov" else "mp4"]
        if item["fragmented"]:
            args += ["-movflags", "+frag_keyframe+empty_moov+default_base_moof"]
        elif item["faststart"]:
            args += ["-movflags", "+faststart"]
        if video == "libx265":
            args += ["-tag:v", "hvc1"]
    elif container in ("ts", "m2ts"):
        args += ["-f", "mpegts"]
        if container == "m2ts":
            args += ["-mpegts_m2ts_mode", "1"]
    elif container.startswith("hls-"):
        args += [
            "-f",
            "hls",
            "-hls_time",
            "2",
            "-hls_list_size",
            "0",
            "-hls_playlist_type",
            "vod",
            "-hls_segment_type",
            "fmp4" if container.endswith("fmp4") else "mpegts",
        ]
    elif container == "dash":
        args += [
            "-f",
            "dash",
            "-seg_duration",
            "2",
            "-use_template",
            "1",
            "-use_timeline",
            "1",
        ]
    args += [str(target)]
    return args


def float_fraction(rate):
    a, _, b = rate.partition("/")
    return float(a) / float(b or 1)


def validate_metadata(item, metadata):
    streams = metadata["streams"]
    video = next(s for s in streams if s["codec_type"] == "video")
    expected = {
        "libx264": "h264",
        "libx265": "hevc",
        "libsvtav1": "av1",
        "libvpx": "vp8",
        "libvpx-vp9": "vp9",
        "prores_ks": "prores",
        "libtheora": "theora",
        "flv": "flv1",
    }.get(item["video"], item["video"])
    assert video["codec_name"] == expected, "video codec mismatch"
    assert (video["width"], video["height"]) == (item["width"], item["height"]), (
        "picture dimensions mismatch"
    )
    audio = [s for s in streams if s["codec_type"] == "audio"]
    assert len(audio) == (1 + int(item["multi_audio"]) if item["audio"] else 0), (
        "audio stream count mismatch"
    )
    if audio:
        expected = {
            "libopus": "opus",
            "libvorbis": "vorbis",
            "libmp3lame": "mp3",
            "dca": "dts",
        }.get(item["audio"], item["audio"])
        assert all(
            s["codec_name"] == expected and s["channels"] == item["channels"]
            for s in audio
        ), "audio codec/layout mismatch"
    if item["multi_audio"]:
        assert [s.get("tags", {}).get("language") for s in audio] == ["eng", "spa"], (
            "audio language mismatch"
        )
        assert [s.get("disposition", {}).get("default") for s in audio] == [0, 1], (
            "default audio track mismatch"
        )
    if item["interlaced"]:
        assert video.get("field_order") in ("tt", "tb") or any(
            f.get("interlaced_frame") == 1 and f.get("top_field_first") == 1
            for f in metadata.get("interlace_frames", [])
        ), "missing top-first interlace metadata"
        assert (
            abs(float_fraction(video["r_frame_rate"]) - float_fraction(item["rate"]))
            < 0.02
        ), "interlaced frame rate mismatch"
    if item["transfer"] != "bt709":
        assert (
            video.get("color_transfer") == item["transfer"]
            and video.get("color_primaries") == "bt2020"
        ), "HDR metadata mismatch"
        assert video.get("pix_fmt") == "yuv420p10le", "HDR bit depth mismatch"
    if item["subtitles"]:
        assert any(s["codec_type"] == "subtitle" for s in streams), (
            "missing subtitle stream"
        )


def generate(item, root, ffmpeg, ffprobe, master, encode=True):
    directory = root / item["id"]
    directory.mkdir(exist_ok=True)
    receipt_path = directory / "receipt.json"
    previous_arguments = None
    if receipt_path.exists():
        receipt = json.loads(receipt_path.read_text())
        previous_arguments = receipt.get("argv")
        if receipt.get("status") == "valid" and all(
            (directory / name).is_file() and digest(directory / name) == value
            for name, value in receipt["files"].items()
        ):
            try:
                validate_metadata(item, receipt["probe"])
                return receipt
            except (AssertionError, KeyError, StopIteration):
                pass
        attempt = str(time.time_ns())
        receipt_path.rename(directory / f"receipt-attempt-{attempt}.json")
        for name in ("generate.log", "validate.log"):
            if (directory / name).exists():
                (directory / name).rename(
                    directory
                    / (name.removesuffix(".log") + "-attempt-" + attempt + ".log")
                )
    if item["subtitles"] and encode:
        (directory / "captions.srt").write_text(
            "1\n00:00:00,000 --> 00:00:10,000\nMedia qualification English caption\n\n2\n00:00:12,000 --> 00:00:30,000\nSecond qualification caption\n"
        )
    started = time.monotonic()
    args = command(item, root, ffmpeg, master)
    if not encode and previous_arguments:
        args = previous_arguments
    receipt = dict(
        id=item["id"],
        recipe_sha256=hashlib.sha256(canonical(item)).hexdigest(),
        status="invalid",
        argv=args,
    )
    try:
        if encode:
            with (directory / "generate.log").open("wb") as log:
                subprocess.run(
                    args,
                    stdout=log,
                    stderr=log,
                    check=True,
                    timeout=max(1800, item["duration"] / 5),
                )
        else:
            assert previous_arguments and (root / item["file"]).is_file(), (
                "no attributable generated output to validate"
            )
        probe = subprocess.run(
            [
                ffprobe,
                "-v",
                "error",
                "-show_streams",
                "-show_format",
                "-of",
                "json",
                str(root / item["file"]),
            ],
            capture_output=True,
            check=True,
            timeout=60,
        )
        metadata = json.loads(probe.stdout)
        if item["interlaced"]:
            frames = subprocess.run(
                [
                    ffprobe,
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-read_intervals",
                    "%+0.2",
                    "-show_frames",
                    "-show_entries",
                    "frame=interlaced_frame,top_field_first",
                    "-of",
                    "json",
                    str(root / item["file"]),
                ],
                capture_output=True,
                check=True,
                timeout=60,
            )
            metadata["interlace_frames"] = json.loads(frames.stdout).get("frames", [])
        validate_metadata(item, metadata)
        with (directory / "validate.log").open("wb") as log:
            subprocess.run(
                [
                    ffmpeg,
                    "-nostdin",
                    "-v",
                    "error",
                    "-xerror",
                    "-threads",
                    "2",
                    "-i",
                    str(root / item["file"]),
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
                check=True,
                timeout=max(600, item["duration"] / 10),
            )
        receipt.update(
            status="valid",
            probe=metadata,
            files={
                p.name: digest(p)
                for p in sorted(directory.iterdir())
                if p.is_file() and p.suffix not in (".log", ".json", ".tmp")
            },
        )
    except (subprocess.SubprocessError, AssertionError, OSError, ValueError) as error:
        receipt["failure"] = type(error).__name__
        if isinstance(error, AssertionError):
            receipt["failure_detail"] = str(error)
    receipt["generation_seconds"] = time.monotonic() - started
    write_json(receipt_path, receipt)
    return receipt


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("action", choices=["manifest", "generate", "validate"])
    p.add_argument("--root", type=Path, required=True)
    p.add_argument("--ffmpeg", default="ffmpeg")
    p.add_argument("--ffprobe", default="ffprobe")
    p.add_argument("--limit", type=int)
    p.add_argument("--ids", help="comma-separated fixture IDs")
    p.add_argument("--workers", type=int, choices=[1, 2], default=2)
    p.add_argument(
        "--master",
        type=Path,
        help="licensed real-motion master; provenance is required in master.json",
    )
    args = p.parse_args()
    args.root.mkdir(parents=True, exist_ok=True, mode=0o700)
    with (args.root / ".corpus.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        items = recipes()
        if args.master:
            assert args.master.with_suffix(".json").is_file(), (
                "master provenance sidecar required"
            )
            attribution = json.loads(args.master.with_suffix(".json").read_text())
            assert (
                attribution.get("license")
                and attribution.get("attribution")
                and attribution.get("url")
            )
            master_hash = digest(args.master)
            for item in items:
                item.update(
                    license=attribution["license"],
                    attribution=attribution["attribution"],
                    source_url=attribution["url"],
                    source_sha256=master_hash,
                    timing_markers=False,
                )
                item.pop("id")
                item.pop("file")
                item["id"] = "real-" + hashlib.sha256(canonical(item)).hexdigest()[:16]
                item["file"] = (
                    f"{item['id']}/media.{EXT.get(item['container'], item['container'])}"
                )
        manifest = dict(
            version=VERSION,
            fixtures=items,
            budget_bytes=BUDGET,
            master_sha256=digest(args.master) if args.master else None,
        )
        existing = args.root / "manifest.json"
        if existing.exists():
            assert json.loads(existing.read_text()) == manifest, (
                "frozen manifest differs; use another root"
            )
        else:
            write_json(existing, manifest)
        if args.action == "manifest":
            print(
                json.dumps(
                    {"fixtures": len(items), "manifest_sha256": digest(existing)}
                )
            )
            return
        if args.ids:
            ids = set(args.ids.split(","))
            items = [i for i in items if i["id"] in ids]
            assert len(items) == len(ids), "unknown fixture ID"
        # Stratify early subsets across formats rather than taking only H264 MP4.
        items = sorted(items, key=lambda i: (i["variant"], i["id"]))[: args.limit]
        failures = 0
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as pool:
            for start in range(0, len(items), args.workers):
                used = sum(
                    f.stat().st_size for f in args.root.rglob("*") if f.is_file()
                )
                # Reserve two worst-case batch payloads; never allocate the entire budget.
                if (
                    used > BUDGET - 8 * 1024**3
                    or shutil.disk_usage(args.root).free < 12 * 1024**3
                ):
                    raise SystemExit("storage reserve reached; corpus incomplete")
                futures = [
                    pool.submit(
                        generate,
                        i,
                        args.root,
                        args.ffmpeg,
                        args.ffprobe,
                        args.master,
                        args.action == "generate",
                    )
                    for i in items[start : start + args.workers]
                ]
                for f in futures:
                    r = f.result()
                    failures += r["status"] != "valid"
                    print(
                        json.dumps(
                            {k: r[k] for k in ("id", "status", "generation_seconds")}
                        ),
                        flush=True,
                    )
        raise SystemExit(1 if failures else 0)


if __name__ == "__main__":
    main()
