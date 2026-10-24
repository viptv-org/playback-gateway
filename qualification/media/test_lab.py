"""Correctness tests for the harness: failures must never turn into playback passes."""

import http.client
import json
from pathlib import Path
import tempfile
import threading
import unittest
import xml.etree.ElementTree as ET
from corpus import recipes, command, representatives, validate_metadata
from live import hls_window, dash_window, publication_times
from origin import Origin, byte_range
from reference import reference_command, shared_graph
from report import acceptance, compare, throughput_passed
from quality import media as quality_media
from corpus import digest
from readiness import published_buffer
from measure import observed_inspection


class CorpusTests(unittest.TestCase):
    def test_startup_representatives_include_expensive_paths(self):
        selected = representatives(recipes(), 40)
        self.assertEqual(len(selected), 40)
        self.assertTrue(any(i["width"] == 3840 for i in selected))
        self.assertTrue(any(i["transfer"] == "smpte2084" for i in selected))
        self.assertTrue(any(i["transfer"] == "arib-std-b67" for i in selected))
        self.assertTrue(any(i["interlaced"] for i in selected))
        self.assertTrue(any(i["multi_audio"] for i in selected))
        self.assertTrue(any(i["subtitles"] for i in selected))

    def test_matrix_is_unique_and_covers_required_families(self):
        items = recipes()
        self.assertEqual(len(items), 1056)
        self.assertEqual(len({i["id"] for i in items}), len(items))
        meaningful = [
            {k: v for k, v in i.items() if k not in ("id", "file", "variant")}
            for i in items
        ]
        self.assertEqual(len({json.dumps(i, sort_keys=True) for i in meaningful}), 1056)
        self.assertTrue(
            {
                "mp4",
                "mkv",
                "webm",
                "ts",
                "m2ts",
                "avi",
                "mpg",
                "flv",
                "ogv",
                "mov",
                "hls-ts",
                "hls-fmp4",
                "dash",
            }
            <= {i["container"] for i in items}
        )
        self.assertTrue(any(i["width"] == 3840 for i in items))
        self.assertTrue(any(i["channels"] == 6 for i in items))

    def test_every_recipe_has_shell_free_bounded_encoder_argv(self):
        for item in recipes():
            args = command(item, Path("/fixtures"), "ffmpeg")
            self.assertEqual(args[0], "ffmpeg")
            self.assertEqual(args[-1], "/fixtures/" + item["file"])
            self.assertIn("-t", args)
            self.assertIn(str(item["duration"]), args)
            self.assertIn(f"all_seed={item['seed']}", args[args.index("-vf") + 1])

    def test_silent_and_multiple_audio_mapping(self):
        silent = next(i for i in recipes() if i["audio"] is None)
        self.assertNotIn("-c:a", command(silent, Path("/fixtures"), "ffmpeg"))
        multi = next(i for i in recipes() if i["multi_audio"])
        argv = command(multi, Path("/fixtures"), "ffmpeg")
        self.assertIn("2:a:0", argv)
        self.assertIn("language=spa", argv)

    def test_ten_bit_matrix_alone_does_not_qualify_as_hdr(self):
        item = next(
            i for i in recipes() if i["transfer"] == "smpte2084" and i["audio"] == "aac"
        )
        video = dict(
            codec_type="video",
            codec_name="hevc",
            width=item["width"],
            height=item["height"],
            pix_fmt="yuv420p10le",
            color_space="bt2020nc",
        )
        metadata = dict(
            streams=[
                video,
                dict(codec_type="audio", codec_name="aac", channels=item["channels"]),
            ]
        )
        with self.assertRaisesRegex(AssertionError, "HDR metadata mismatch"):
            validate_metadata(item, metadata)
        video.update(color_transfer="smpte2084", color_primaries="bt2020")
        validate_metadata(item, metadata)


class RangeTests(unittest.TestCase):
    def test_finite_open_suffix_and_whole_ranges(self):
        for value, want in [
            (None, (0, 99, False)),
            ("bytes=10-19", (10, 19, True)),
            ("bytes=90-", (90, 99, True)),
            ("bytes=-5", (95, 99, True)),
            ("bytes=90-1000", (90, 99, True)),
        ]:
            self.assertEqual(byte_range(value, 100), want)
        for value in (
            "bytes=100-",
            "bytes=9-3",
            "bytes=-0",
            "bytes=1-2,4-5",
            "bytes=-",
            "bytes=a-b",
        ):
            with self.assertRaises(ValueError):
                byte_range(value, 100)

    def test_origin_exact_bytes_head_redirect_and_path_containment(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "media.mp4").write_bytes(bytes(range(100)))
            (root / "secret.json").write_text("private")
            server = Origin(("127.0.0.1", 0), root, None)
            thread = threading.Thread(target=server.serve_forever)
            thread.start()
            try:

                def request(method, path, headers=None):
                    c = http.client.HTTPConnection(
                        "127.0.0.1", server.server_port, timeout=2
                    )
                    try:
                        c.request(method, path, headers=headers or {})
                        r = c.getresponse()
                        return r.status, dict(r.getheaders()), r.read()
                    finally:
                        c.close()

                status, headers, data = request(
                    "GET", "/media.mp4", {"Range": "bytes=10-19"}
                )
                self.assertEqual(
                    (status, headers["Content-Range"], data),
                    (206, "bytes 10-19/100", bytes(range(10, 20))),
                )
                self.assertEqual(request("HEAD", "/media.mp4")[2], b"")
                self.assertEqual(
                    request("GET", "/media.mp4", {"Range": "bytes=100-"})[0], 416
                )
                self.assertEqual(request("GET", "/redirect/media.mp4")[0], 307)
                self.assertEqual(request("GET", "/%2e%2e/etc/passwd")[0], 404)
                self.assertEqual(request("GET", "/secret.json")[0], 404)
            finally:
                server.shutdown()
                server.server_close()
                thread.join()


class TimelineTests(unittest.TestCase):
    def test_fractional_hls_primes_three_target_durations_and_gates_future(self):
        text = "#EXTM3U\n#EXT-X-TARGETDURATION:3\n" + "".join(
            f"#EXTINF:2.002,\ns{i}.ts\n" for i in range(30)
        )
        initial = hls_window(text, 0).decode()
        self.assertEqual(initial.count("#EXTINF:"), 5)
        self.assertIn("#EXT-X-TARGETDURATION:3", initial)
        times = publication_times(text, ".m3u8")
        self.assertLessEqual(times["s4.ts"], 0)
        self.assertAlmostEqual(times["s5.ts"], 2.002)

    def test_live_hls_advances_without_rewinding_segment_identity(self):
        text = (
            "#EXTM3U\n#EXT-X-VERSION:3\n"
            + "".join(f"#EXTINF:2.0,\ns{i}.ts\n" for i in range(30))
            + "#EXT-X-ENDLIST\n"
        )
        first = hls_window(text, 0).decode()
        later = hls_window(text, 20).decode()
        self.assertIn("#EXT-X-MEDIA-SEQUENCE:0", first)
        self.assertNotIn("#EXT-X-ENDLIST", first)
        self.assertIn("#EXT-X-MEDIA-SEQUENCE:7", later)
        self.assertNotIn("\ns0.ts", later)
        self.assertIn("#EXT-X-ENDLIST", hls_window(text, 60).decode())

    def test_live_dash_preserves_real_timestamps_and_updates_start_number(self):
        text = '<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT60S"><Period><AdaptationSet><Representation><SegmentTemplate timescale="1000" startNumber="1" media="segment-$Number$.m4s"><SegmentTimeline><S t="0" d="2000" r="29"/></SegmentTimeline></SegmentTemplate></Representation></AdaptationSet></Period></MPD>'
        root = ET.fromstring(dash_window(text, 20, 100000))
        self.assertEqual(root.attrib["type"], "dynamic")
        self.assertNotIn("mediaPresentationDuration", root.attrib)
        t = next(root.iter("{urn:mpeg:dash:schema:mpd:2011}SegmentTemplate"))
        self.assertGreater(int(t.attrib["startNumber"]), 1)
        starts = [
            int(e.attrib["t"]) for e in root.iter("{urn:mpeg:dash:schema:mpd:2011}S")
        ]
        self.assertEqual(starts, sorted(set(starts)))


class ReferenceTests(unittest.TestCase):
    def test_short_inspection_requires_versioned_event_and_matching_graph(self):
        candidates = [
            [
                "/media/ffmpeg",
                "-i",
                "https://owned/source",
                "/state/ingest-id/index.m3u8",
            ],
            ["/media/ffmpeg", "-i", "pipe:0", "/state/output-id/index.m3u8"],
        ]
        event = 'gateway_request{trace=42 operation="create"}: Replay inspection launched inspection_profile="local_replay_streams_v1" inspection_program=/media/ffprobe allowed_extensions="ts,m3u8" ingest_id="ingest-id"'
        argv, evidence = observed_inspection([], candidates, [event], "42")
        self.assertEqual(evidence, "versioned_local_launch_event")
        self.assertEqual(argv[0], "/media/ffprobe")
        self.assertEqual(argv[-1], candidates[0][-1])
        self.assertEqual(argv[argv.index("-protocol_whitelist") + 1], "file")
        self.assertIsNone(observed_inspection([], candidates, [event], "43")[0])
        self.assertIsNone(
            observed_inspection(
                [], candidates, [event.replace('ingest-id"', 'other-id"')], "42"
            )[0]
        )

    def test_reference_buffer_waits_for_same_published_media_and_init(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = root / "index.m3u8"
            header = '#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MAP:URI="init.mp4"\n'
            (root / "init.mp4").write_bytes(b"init")
            for i in range(3):
                (root / f"s{i}.m4s").write_bytes(b"media")
            path.write_text(header + "#EXTINF:2,\ns0.m4s\n")
            self.assertEqual(published_buffer(path, False), 2)
            self.assertIsNone(published_buffer(path, True))
            path.write_text(
                header + "".join(f"#EXTINF:2,\ns{i}.m4s\n" for i in range(3))
            )
            self.assertEqual(published_buffer(path, True), 6)
            (root / "init.mp4").unlink()
            self.assertIsNone(published_buffer(path, True))

    def test_shared_pipe_requires_recorded_graph_and_preserves_output_settings(self):
        ingress = [
            "ffmpeg",
            "-i",
            "https://owned/source",
            "-c",
            "copy",
            "-hls_segment_filename",
            "/old/ingest/segment-%09d.ts",
            "/old/ingest/index.m3u8",
        ]
        output = [
            "ffmpeg",
            "-protocol_whitelist",
            "pipe",
            "-f",
            "mpegts",
            "-i",
            "pipe:0",
            "-vf",
            "scale=1280:720",
            "-c:v",
            "h264_qsv",
            "/old/output/index.m3u8",
        ]
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(ValueError):
                shared_graph([output, ingress], "https://owned/reference", Path(tmp))
            first, second = shared_graph(
                [output, ingress],
                "https://owned/reference",
                Path(tmp),
                pipe_verified=True,
            )
            self.assertIn("https://owned/reference", first)
            self.assertIn(str(Path(tmp) / "ingest/segment-%09d.ts"), first)
            self.assertEqual(second[second.index("-i") + 1], "pipe:0")
            self.assertIn("h264_qsv", second)
            self.assertIn("scale=1280:720", second)

    def test_reference_retains_encoder_and_filter_and_replaces_private_transport(self):
        argv = [
            "/opt/ffmpeg/bin/ffmpeg",
            "-http_proxy",
            "http://127.0.0.1:1234",
            "-i",
            "https://private/source",
            "-vf",
            "scale=1920:1080",
            "-c:v",
            "h264_qsv",
            "-hls_segment_filename",
            "/state/media/abc/segment-%09d.ts",
            "/state/media/abc/index.m3u8",
        ]
        result = reference_command(
            argv, "https://11.255.255.1/fixture", Path("/tmp/reference")
        )
        self.assertNotIn("-http_proxy", result)
        self.assertIn("h264_qsv", result)
        self.assertIn("scale=1920:1080", result)
        self.assertEqual(result[-1], "/tmp/reference/index.m3u8")
        self.assertNotIn("https://private/source", result)

    def test_multiple_input_pipeline_cannot_be_reported_as_equivalent(self):
        with self.assertRaises(ValueError):
            reference_command(
                ["ffmpeg", "-i", "one", "-i", "two", "out.m3u8"],
                "source",
                Path("/tmp/reference"),
            )


class EvidenceTests(unittest.TestCase):
    def test_failed_or_slow_raw_control_cannot_pass_throughput(self):
        row = dict(
            status="passed",
            media_seconds=30,
            consumer_mode="copy",
            producer_wall_ms=1000,
            delivery=dict(video_mode="copy", audio_mode="copy"),
            reference=dict(status="passed", media_seconds=30, producer_wall_ms=960),
        )
        self.assertTrue(throughput_passed(row))
        row["reference"]["status"] = "failed"
        self.assertFalse(throughput_passed(row))
        row["reference"].update(status="passed", producer_wall_ms=940)
        self.assertFalse(throughput_passed(row))

    def test_fmp4_quality_references_cannot_overwrite_candidate_bytes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            out = root / "joined"
            out.mkdir()
            prepared = []
            for name, payload in [("before", b"baseline"), ("after", b"candidate")]:
                folder = root / name
                folder.mkdir()
                path = folder / "same-fixture.json"
                media = Path(str(path) + ".media")
                media.mkdir()
                (media / "init.mp4").write_bytes(b"init")
                (media / "segment-000000000.m4s").write_bytes(payload)
                row = {
                    "media_samples": [
                        {"file": file.name, "sha256": digest(file)}
                        for file in media.iterdir()
                    ]
                }
                prepared.append(quality_media(path, row, out))
            self.assertNotEqual(prepared[0], prepared[1])
            self.assertEqual(prepared[0].read_bytes(), b"initbaseline")
            self.assertEqual(prepared[1].read_bytes(), b"initcandidate")

    def test_empty_and_partial_campaigns_never_pass(self):
        result = acceptance({"fixtures": recipes()}, [], [], None, None)
        self.assertFalse(result["accepted"])
        self.assertEqual(len(result["unplayed"]), 1056)

    def test_failures_and_missing_pairs_are_retained(self):
        before = [
            dict(fixture="a", status="passed", ready_ms=100),
            dict(fixture="b", status="failed"),
        ]
        after = [
            dict(fixture="a", status="passed", ready_ms=250),
            dict(fixture="c", status="passed", ready_ms=1),
        ]
        result = compare(before, after)
        self.assertEqual(result["regressions"], 1)
        self.assertEqual(result["missing"], 2)
        self.assertEqual(result["baseline_failures"], 1)


if __name__ == "__main__":
    unittest.main()
