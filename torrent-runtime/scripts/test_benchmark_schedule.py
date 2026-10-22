import importlib.util
from pathlib import Path
import unittest
import json
import tempfile

spec = importlib.util.spec_from_file_location("desktop_benchmark", Path(__file__).with_name("benchmark-desktop.py"))
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)


class ScheduleTests(unittest.TestCase):
    def load_case(self, case):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "cases.json"
            path.write_text(json.dumps({"sources": [case]}))
            return benchmark.load_magnet_cases(path)

    def test_magnet_preserves_matched_inputs(self):
        digest = "a" * 40
        tracker = "udp://tracker.example:1337/announce"
        case = {"hash": digest, "magnet": "magnet:?xt=urn%3Abtih%3A" + digest + "&tr=udp%3A%2F%2Ftracker.example%3A1337%2Fannounce",
                "trackers": [tracker]}
        self.assertEqual(self.load_case(case), [case])

    def test_magnet_cannot_change_reference_identity_or_tracker_set(self):
        case = {"hash": "a" * 40, "magnet": "magnet:?xt=urn:btih:" + "b" * 40,
                "trackers": ["udp://tracker.example:1337/announce"]}
        with self.assertRaises(ValueError):
            self.load_case(case)
        case["magnet"] = "magnet:?xt=urn:btih:" + "a" * 40 + "&tr=udp://other.example:1337/announce"
        with self.assertRaises(ValueError):
            self.load_case(case)
        case["trackers"] = []
        with self.assertRaises(ValueError):
            self.load_case(case)

    def test_resume_keeps_uninterrupted_pair_order(self):
        engines = ["baseline", "candidate", "stremio"]
        uninterrupted = list(benchmark.trial_schedule(4, engines, 7, seed=71))
        resumed = list(benchmark.trial_schedule(4, engines, 3, offset=4, seed=71))
        self.assertEqual(resumed, [row for row in uninterrupted if row[0] >= 4])

    def test_each_trial_covers_each_source_and_engine_once(self):
        engines = ["candidate", "stremio"]
        rows = list(benchmark.trial_schedule(5, engines, 3, seed=91))
        for trial in range(3):
            current = [row for row in rows if row[0] == trial]
            self.assertEqual(sorted(row[1] for row in current), list(range(5)))
            for _, _, order in current:
                self.assertCountEqual(order, engines)
        self.assertEqual(engines, ["candidate", "stremio"])

    def test_invalid_schedule_fails_before_trials(self):
        for count, engines, trials, offset in [(0, ["candidate"], 1, 0),
                                               (1, ["candidate", "candidate"], 1, 0),
                                               (1, [], 1, 0), (1, ["candidate"], 0, 0),
                                               (1, ["candidate"], 1, -1)]:
            with self.subTest(count=count, engines=engines, trials=trials, offset=offset):
                with self.assertRaises(ValueError):
                    list(benchmark.trial_schedule(count, engines, trials, offset))


if __name__ == "__main__":
    unittest.main()
