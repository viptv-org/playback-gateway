#!/usr/bin/env python3
"""Prepackage continuous live timelines before timing; expose only completed segments."""

import argparse
import datetime
from functools import lru_cache
import json
import math
import re
from pathlib import Path
import subprocess
import time
import xml.etree.ElementTree as ET
from corpus import digest, write_json


@lru_cache(maxsize=8)
def hls_segments(text):
    lines = text.splitlines()
    segments = []
    duration = None
    prefix = []
    for line in lines:
        if line.startswith("#EXTINF:"):
            duration = float(line.split(":", 1)[1].split(",")[0])
        elif line and not line.startswith("#") and duration is not None:
            segments.append((duration, line))
            duration = None
        elif line.startswith(("#EXT-X-MAP:", "#EXT-X-VERSION:")):
            prefix.append(line)
    if not segments:
        raise ValueError("no complete HLS segments")
    target_line = next(
        (line for line in lines if line.startswith("#EXT-X-TARGETDURATION:")), None
    )
    target = (
        int(target_line.split(":")[1])
        if target_line
        else math.ceil(max(d for d, _ in segments))
    )
    lead = 0
    for seconds, _ in segments:
        lead += seconds
        if lead >= 3 * target:
            break
    return segments, prefix, target, lead


def hls_window(text, elapsed, window=6):
    segments, prefix, target, lead = hls_segments(text)
    cursor = 0
    end = 0
    for seconds, _ in segments:
        cursor += seconds
        if cursor > elapsed + lead + 1e-6:
            break
        end += 1
    end = max(1, end)
    start = max(0, end - window)
    output = [
        "#EXTM3U",
        *prefix,
        f"#EXT-X-TARGETDURATION:{target}",
        f"#EXT-X-MEDIA-SEQUENCE:{start}",
    ]
    for seconds, name in segments[start:end]:
        output.extend([f"#EXTINF:{seconds:.6f},", name])
    if end == len(segments):
        output.append("#EXT-X-ENDLIST")
    return ("\n".join(output) + "\n").encode()


def dash_segments(template):
    return _dash_segments(ET.tostring(template))


@lru_cache(maxsize=8)
def _dash_segments(xml):
    template = ET.fromstring(xml)
    namespace = "urn:mpeg:dash:schema:mpd:2011"
    timeline = template.find(f"{{{namespace}}}SegmentTimeline")
    if timeline is None:
        raise ValueError("explicit SegmentTimeline required")
    segments = []
    cursor = 0
    for entry in timeline:
        duration = int(entry.attrib["d"])
        cursor = int(entry.get("t", str(cursor)))
        repeat = int(entry.get("r", "0"))
        if repeat < 0 or repeat > 100000:
            raise ValueError("unbounded timeline")
        for _ in range(repeat + 1):
            segments.append((cursor, duration))
            cursor += duration
    return segments


def publication_times(text, suffix, lead=10):
    """Completion times for prepackaged segments, relative to the source epoch."""
    if suffix == ".m3u8":
        segments, _, _, lead = hls_segments(text)
        cursor = 0
        times = {}
        for duration, name in segments:
            cursor += duration
            times[name] = cursor - lead
        return times
    ns = "urn:mpeg:dash:schema:mpd:2011"
    times = {}
    for representation in ET.fromstring(text).iter(f"{{{ns}}}Representation"):
        template = representation.find(f"{{{ns}}}SegmentTemplate")
        if template is None:
            raise ValueError("owned DASH requires representation SegmentTemplate")
        scale = int(template.get("timescale", "1"))
        first = int(template.get("startNumber", "1"))
        for i, (t, d) in enumerate(dash_segments(template)):
            name = template.get("media").replace(
                "$RepresentationID$", representation.get("id")
            )
            name = name.replace("$Time$", str(t))
            name = re.sub(
                r"\$Number(?:%0(\d+)d)?\$",
                lambda m: str(first + i).zfill(int(m[1] or 0)),
                name,
            )
            times[name] = (t + d) / scale - lead
    return times


def dash_window(text, elapsed, epoch, query="", window=12, lead=10):
    namespace = "urn:mpeg:dash:schema:mpd:2011"
    ET.register_namespace("", namespace)
    root = ET.fromstring(text)
    stamp = lambda value: (
        datetime.datetime.fromtimestamp(value, datetime.timezone.utc)
        .isoformat(timespec="milliseconds")
        .replace("+00:00", "Z")
    )
    root.attrib.pop("mediaPresentationDuration", None)
    root.attrib.update(
        type="dynamic",
        availabilityStartTime=stamp(epoch - lead),
        publishTime=stamp(time.time()),
        minimumUpdatePeriod="PT1S",
        timeShiftBufferDepth=f"PT{window}S",
        suggestedPresentationDelay="PT4S",
    )
    for period in root.findall(f"{{{namespace}}}Period"):
        period.attrib.pop("duration", None)
    for template in root.iter(f"{{{namespace}}}SegmentTemplate"):
        timeline = template.find(f"{{{namespace}}}SegmentTimeline")
        if timeline is None:
            raise ValueError("explicit SegmentTimeline required")
        scale = int(template.get("timescale", "1"))
        segments = dash_segments(template)
        visible = [
            (i, t, d)
            for i, (t, d) in enumerate(segments)
            if (t + d) / scale <= elapsed + lead
            and (t + d) / scale > elapsed + lead - window
        ]
        if not visible:
            visible = [(0, *segments[0])]
        timeline.clear()
        template.set(
            "startNumber", str(int(template.get("startNumber", "1")) + visible[0][0])
        )
        for _, t, d in visible:
            ET.SubElement(timeline, f"{{{namespace}}}S", dict(t=str(t), d=str(d)))
        if query:
            for attr in ("media", "initialization"):
                template.set(attr, template.get(attr) + "?" + query)
    return ET.tostring(root, encoding="utf-8", xml_declaration=True)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--source", type=Path, required=True)
    p.add_argument("--root", type=Path, required=True)
    p.add_argument("--seconds", type=int, default=30000)
    p.add_argument("--ffmpeg", default="ffmpeg")
    p.add_argument("--ffprobe", default="ffprobe")
    p.add_argument("--id-prefix", default="live")
    p.add_argument("--manifest-name", default="live-manifest.json")
    p.add_argument(
        "--corpus",
        type=Path,
        help="also write live browser fixture descriptors in this corpus",
    )
    args = p.parse_args()
    assert args.id_prefix.replace("-", "").isalnum()
    assert Path(
        args.manifest_name
    ).name == args.manifest_name and args.manifest_name.endswith(".json")
    sidecar = args.source.with_suffix(".json")
    provenance = json.loads(sidecar.read_text()) if sidecar.exists() else {}
    root = args.root
    root.mkdir(parents=True, exist_ok=True)
    for mode, name in [
        ("hls", "index.m3u8"),
        ("dash", "index.mpd"),
        ("ts", "stream.ts"),
    ]:
        directory = root / mode
        directory.mkdir(exist_ok=True)
        receipt = directory / "live.json"
        if receipt.exists():
            previous = json.loads(receipt.read_text())
            if (
                previous.get("source_sha256") == digest(args.source)
                and previous.get("duration") == args.seconds
                and (directory / name).exists()
                and digest(directory / name) == previous.get("manifest_sha256")
            ):
                continue
        command = [
            args.ffmpeg,
            "-nostdin",
            "-v",
            "error",
            "-y",
            "-stream_loop",
            "-1",
            "-i",
            str(args.source),
            "-map",
            "0:v:0",
            "-map",
            "0:a:0?",
            "-c",
            "copy",
            "-t",
            str(args.seconds),
        ]
        if mode == "hls":
            command += [
                "-f",
                "hls",
                "-hls_time",
                "2",
                "-hls_list_size",
                "0",
                "-hls_playlist_type",
                "vod",
            ]
        elif mode == "dash":
            command += [
                "-f",
                "dash",
                "-seg_duration",
                "2",
                "-use_template",
                "1",
                "-use_timeline",
                "1",
            ]
        else:
            command += ["-f", "mpegts"]
            if provenance.get("timing_identity", {}).get("version") == 2:
                command += ["-mpegts_flags", "+pat_pmt_at_frames"]
        command += [str(directory / name)]
        with (directory / "generate.log").open("wb") as log:
            subprocess.run(command, stdout=log, stderr=log, timeout=1800, check=True)
        write_json(
            directory / "live.json",
            dict(
                source_sha256=digest(args.source),
                duration=args.seconds,
                mode=mode,
                argv=command,
                manifest_sha256=digest(directory / name),
            ),
        )
    if provenance.get("timing_identity", {}).get("version") == 2:
        target = root / "ts/stream.ts"
        package = json.loads((target.parent / "live.json").read_text())
        index = target.parent / "keyframes.json"
        if (
            not index.exists()
            or json.loads(index.read_text()).get("source_sha256")
            != package["manifest_sha256"]
        ):
            entries = []
            base = None
            with (target.parent / "index.log").open("w") as log:
                child = subprocess.Popen(
                    [
                        args.ffprobe,
                        "-v",
                        "error",
                        "-select_streams",
                        "v:0",
                        "-show_packets",
                        "-show_entries",
                        "packet=pts_time,pos,flags",
                        "-of",
                        "compact=p=0:nk=0",
                        str(target),
                    ],
                    stdout=subprocess.PIPE,
                    stderr=log,
                    text=True,
                )
                for line in child.stdout:
                    fields = dict(
                        s.split("=", 1) for s in line.strip().split("|") if "=" in s
                    )
                    if (
                        "K" not in fields.get("flags", "")
                        or "pos" not in fields
                        or "pts_time" not in fields
                    ):
                        continue
                    stamp = float(fields["pts_time"])
                    base = stamp if base is None else base
                    entries.append(
                        [stamp - base, max(0, (int(fields["pos"]) // 188 - 32) * 188)]
                    )
                assert child.wait(timeout=30) == 0 and len(entries) > 1, (
                    "clock TS keyframe index failed"
                )
            write_json(
                index,
                dict(
                    source_sha256=package["manifest_sha256"],
                    pts_offset=base,
                    entries=entries,
                ),
            )
    if args.corpus:
        source_info = json.loads(
            subprocess.check_output(
                [
                    args.ffprobe,
                    "-v",
                    "error",
                    "-show_streams",
                    "-of",
                    "json",
                    str(args.source),
                ],
                timeout=60,
            )
        )
        video = next(s for s in source_info["streams"] if s["codec_type"] == "video")
        audio = next(
            (s for s in source_info["streams"] if s["codec_type"] == "audio"), None
        )
        fixtures = []
        for mode, name in [
            ("hls", "index.m3u8"),
            ("dash", "index.mpd"),
            ("ts", "stream.ts"),
        ]:
            target = root / mode / name
            package = json.loads((target.parent / "live.json").read_text())
            validation = target.parent / "validated.json"
            if (
                not validation.exists()
                or json.loads(validation.read_text()).get("manifest_sha256")
                != package["manifest_sha256"]
            ):
                measured = subprocess.run(
                    [
                        args.ffprobe,
                        "-v",
                        "error",
                        "-count_packets",
                        "-show_streams",
                        "-show_format",
                        "-of",
                        "json",
                        str(target),
                    ],
                    capture_output=True,
                    check=True,
                    timeout=1800,
                )
                facts = json.loads(measured.stdout)
                assert any(
                    int(s.get("nb_read_packets", "0")) > 0
                    for s in facts["streams"]
                    if s["codec_type"] == "video"
                )
                assert not measured.stderr.strip(), "live package has demux errors"
                write_json(
                    validation,
                    dict(manifest_sha256=package["manifest_sha256"], probe=facts),
                )
            item = dict(
                id=args.id_prefix + "-" + mode,
                container=mode,
                video=video["codec_name"],
                audio=audio["codec_name"] if audio else None,
                width=video["width"],
                height=video["height"],
                rate=video["r_frame_rate"],
                duration=args.seconds,
                live=True,
                multi_audio=False,
                subtitles=False,
                timing_markers=True,
                license="inherits source; see source_sha256",
                source_sha256=digest(args.source),
                file=("paced/" if mode == "ts" else "")
                + str(target.relative_to(args.corpus)),
            )
            if provenance.get("timing_identity"):
                item["timing_identity"] = provenance["timing_identity"]
                item["source_provenance"] = provenance
            if mode == "dash" and args.id_prefix == "clock":
                # FFmpeg starts timelines sixty seconds behind the end. Leave
                # a normal DVR window behind that read head instead of evicting it.
                item["file"] += "?window=90"
            directory = args.corpus / item["id"]
            directory.mkdir(exist_ok=True)
            write_json(
                directory / "receipt.json",
                dict(
                    id=item["id"],
                    status="valid",
                    validation="fully demuxed copy of validated generated source",
                    files={},
                    source_package=package,
                    probe=json.loads(validation.read_text())["probe"],
                ),
            )
            fixtures.append(item)
        write_json(args.corpus / args.manifest_name, dict(version=1, fixtures=fixtures))
    print(json.dumps({"status": "packaged", "duration_seconds": args.seconds}))


if __name__ == "__main__":
    main()
