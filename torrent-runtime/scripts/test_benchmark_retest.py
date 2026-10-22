import importlib.util
import math
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("retest", Path(__file__).with_name("summarize-magnet-retest.py"))
retest = importlib.util.module_from_spec(spec)
spec.loader.exec_module(retest)


class RetestTests(unittest.TestCase):
    def pair(self, c, s, cp=True, sp=True):
        return dict(candidate=dict(passed=cp, first_frame_seconds=c),
                    stremio=dict(passed=sp, first_frame_seconds=s))

    def test_every_failure_is_repeated_even_if_it_has_a_fast_time(self):
        self.assertTrue(retest.repeat_needed(self.pair(1, 10, cp=False)))
        self.assertTrue(retest.repeat_needed(self.pair(1, 10, sp=False)))
        self.assertEqual(retest.latency(dict(passed=False, first_frame_seconds=1)), math.inf)

    def test_losses_and_close_results_get_a_fixed_followup(self):
        self.assertTrue(retest.repeat_needed(self.pair(11, 10)))
        self.assertTrue(retest.repeat_needed(self.pair(8.1, 10)))
        self.assertFalse(retest.repeat_needed(self.pair(8, 10)))

    def test_missing_successful_measurements_cannot_be_faster(self):
        for value in (None, math.nan, math.inf, True, 0):
            with self.subTest(value=value), self.assertRaises(ValueError):
                retest.latency(dict(passed=True, first_frame_seconds=value))

    def summary(self, c_values, s_values):
        case = dict(case=18, hash="a" * 40, label="Movie")
        rows = [dict(hash=case["hash"], engine=name, passed=value is not None,
                     first_frame_seconds=value, case=0)
                for name, values in (("candidate", c_values), ("stremio", s_values)) for value in values]
        return retest.summarize([case], rows)[0]

    def test_original_source_identity_survives_subset_renumbering(self):
        cell = self.summary([4, 5, 6], [8, 9, 10])
        self.assertEqual(cell["case"], 18)
        self.assertEqual(cell["candidate"]["attempts"], 3)
        self.assertEqual(cell["diagnostic_result"], "candidate_favored")
        self.assertFalse(cell["qualification"])

    def test_faster_median_with_more_failures_is_inconclusive(self):
        cell = self.summary([1, 2, None], [8, 9, 10])
        self.assertEqual(cell["candidate"]["failures"], 1)
        self.assertEqual(cell["diagnostic_result"], "inconclusive")

    def test_unavailable_swarm_is_not_a_performance_win(self):
        self.assertEqual(self.summary([None], [None])["diagnostic_result"], "neither_started")


if __name__ == "__main__":
    unittest.main()
