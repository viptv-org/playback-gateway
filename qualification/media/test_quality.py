"""Opt-in real FFmpeg checks for raw HDR reference color metadata loss."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from corpus import digest


@unittest.skipUnless(
    os.environ.get("PLAYBACK_TEST_FFMPEG") and os.environ.get("PLAYBACK_TEST_FFPROBE"),
    "requires explicit qualified FFmpeg/FFprobe paths",
)
class SoftwareReferenceTests(unittest.TestCase):
    def check_reference(self, transfer, tags="bt709"):
        ffmpeg = os.environ["PLAYBACK_TEST_FFMPEG"]
        ffprobe = os.environ["PLAYBACK_TEST_FFPROBE"]
        with tempfile.TemporaryDirectory(prefix="quality-reference-") as directory:
            root = Path(directory)
            corpus, baseline, candidate = (root / name for name in ("corpus", "before", "after"))
            for folder in (corpus, baseline, candidate):
                folder.mkdir()
            fixture = dict(id="hdr", transfer=transfer, width=64, height=48, file="source.mkv")
            (corpus / "manifest.json").write_text(json.dumps(dict(fixtures=[fixture])))

            def run(args):
                subprocess.run(
                    [ffmpeg, "-nostdin", "-v", "error", "-threads", "1", "-filter_threads", "1", *args],
                    check=True, capture_output=True, timeout=30,
                )

            run([
                "-f", "lavfi", "-i", "testsrc2=size=64x48:rate=1",
                "-vf", f"zscale=transferin=bt709:primariesin=bt709:matrixin=bt709:transfer={transfer}:primaries=bt2020:matrix=bt2020nc:npl=100,format=yuv420p10le",
                "-frames:v", "1", "-c:v", "ffv1", "-threads", "1",
                "-color_primaries", "bt2020", "-color_trc", transfer,
                "-colorspace", "bt2020nc", "-color_range", "tv", str(corpus / fixture["file"]),
            ])
            observation = candidate / "hdr.json"
            samples = Path(str(observation) + ".media")
            samples.mkdir()
            segment = samples / "segment-000000000.ts"
            run([
                "-i", str(corpus / fixture["file"]), "-vf",
                "zscale=transfer=linear:npl=100,format=gbrpf32le,zscale=w=64:h=48:filter=bilinear:primaries=bt709,tonemap=tonemap=mobius:desat=2,zscale=transfer=bt709:matrix=bt709:range=limited,format=yuv420p,sidedata=mode=delete,setsar=1",
                "-frames:v", "1", "-c:v", "libx264", "-qp", "0", "-threads", "1",
                "-color_primaries", tags, "-color_trc", tags, "-colorspace", tags,
                "-color_range", "tv", "-f", "mpegts", str(segment),
            ])
            observation.write_text(json.dumps(dict(
                status="passed", media_samples=[dict(file=segment.name, sha256=digest(segment))],
            )))
            output = root / "quality.json"
            result = subprocess.run([
                sys.executable, str(Path(__file__).with_name("quality.py")),
                "--baseline", str(baseline), "--candidate", str(candidate),
                "--corpus", str(corpus), "--output", str(output),
                "--ffmpeg", ffmpeg, "--ffprobe", ffprobe,
            ], capture_output=True, text=True, timeout=60)
            self.assertIn(result.returncode, (0, 1), result.stderr)
            row = json.loads(output.read_text())["results"][0]
            self.assertNotIn("failure", row)
            self.assertTrue(row["reference_color_properties_restored"])
            return result.returncode, row

    def test_pq_lossless_output_matches_raw_reference(self):
        code, row = self.check_reference("smpte2084")
        self.assertEqual(code, 0, row)
        self.assertTrue(row["identical_pixels"], row)

    def test_hlg_lossless_output_matches_raw_reference(self):
        code, row = self.check_reference("arib-std-b67")
        self.assertEqual(code, 0, row)
        self.assertTrue(row["identical_pixels"], row)

    def test_reference_retagging_does_not_accept_wrong_gateway_metadata(self):
        code, row = self.check_reference("arib-std-b67", "smpte170m")
        self.assertEqual(code, 1, row)
        self.assertFalse(row["color_metadata_correct"])
        self.assertEqual(row["status"], "failed")


if __name__ == "__main__":
    unittest.main()
