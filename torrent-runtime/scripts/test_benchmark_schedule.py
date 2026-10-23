import importlib.util
from pathlib import Path
import unittest
import json
import tempfile
from unittest.mock import Mock, patch

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

    def test_player_wait_has_its_own_deadline_without_reference_launch(self):
        for smoke in (False, True):
            with self.subTest(smoke=smoke):
                process, result = Mock(), Mock()
                process.poll.return_value = None
                result.is_file.return_value = False
                with patch.object(benchmark.time, "monotonic", side_effect=[0, 10000]):
                    with self.assertRaisesRegex(TimeoutError, "player_timeout"):
                        benchmark.wait_for_player(process, result, 30, 0, smoke)

    def test_player_wait_accepts_a_completed_result(self):
        process, result = Mock(), Mock()
        process.poll.return_value = None
        result.is_file.return_value = True
        benchmark.wait_for_player(process, result, 30, 0, True)

    def test_startup_or_short_watch_cannot_pass_full_playback_smoke(self):
        self.assertFalse(benchmark.complete_playback_smoke(dict(passed=True, first_frame_seconds=1)))
        row = dict(smoke_smooth=True, observed_duration_ms=200000, near_end_target_ms=190000,
                   backward_target_ms=30000, near_end_seconds=1, backward_seconds=.3,
                   playback_phases=[dict(phase=name, wall_seconds=seconds) for name, seconds in
                                    [("start", 45), ("near_end", 15), ("backward", 15)]])
        self.assertTrue(benchmark.complete_playback_smoke(row))
        row['playback_phases'][0]['wall_seconds'] = 1
        self.assertFalse(benchmark.complete_playback_smoke(row))

    def test_seek_screen_requires_real_far_seek_and_sustained_destination_frames(self):
        self.assertFalse(benchmark.complete_seek_screen(dict(passed=True, forward_seconds=.1, backward_seconds=.1)))
        phase=dict(seconds=3.1,rendered_fps=24,freeze_samples=0,position_advanced_ms=3000)
        row=dict(observed_duration_ms=200000,forward_target_ms=190000,backward_target_ms=10000,
                 forward_seconds=.1,backward_seconds=.1,forward_playback=phase.copy(),backward_playback=phase.copy())
        self.assertTrue(benchmark.complete_seek_screen(row))
        for field,value in [('seconds',2),('rendered_fps',float('nan')),('freeze_samples',1),('position_advanced_ms',0)]:
            changed={**row,'forward_playback':{**phase,field:value}}
            self.assertFalse(benchmark.complete_seek_screen(changed))
        self.assertFalse(benchmark.complete_seek_screen({**row,'forward_target_ms':120000}))


if __name__ == "__main__":
    unittest.main()
