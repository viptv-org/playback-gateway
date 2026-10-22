import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("pipeline", Path(__file__).with_name("benchmark-pipeline.py"))
pipeline = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pipeline)


class PipelineScreenTest(unittest.TestCase):
    def rows(self, candidate):
        return [dict(engine=name, latency_ms=120, passed=True, elapsed_ms=value)
                for name, values in (("baseline", [2000, 2100, 1900]), ("candidate", candidate))
                for value in values]

    def test_rejects_measured_mixed_peer_regression(self):
        self.assertFalse(pipeline.compare_screen(self.rows([2194, 2190, 2200]), 120, 5)["passed"])

    def test_accepts_narrow_queue_fix(self):
        self.assertTrue(pipeline.compare_screen(self.rows([1591, 1722, 1719]), 120, 5)["passed"])

    def test_retains_small_control_jitter(self):
        self.assertTrue(pipeline.compare_screen(self.rows([2001, 2002, 2000]), 120, 5)["passed"])

    def test_failed_fast_measurement_cannot_help(self):
        rows = self.rows([1, 1, 1])
        rows[-1]["passed"] = False
        self.assertFalse(pipeline.compare_screen(rows, 120, 5)["passed"])

    def test_missing_and_nonfinite_cannot_pass(self):
        for value in (None, float("nan"), float("inf"), 0):
            with self.subTest(value=value):
                self.assertFalse(pipeline.compare_screen(self.rows([1700, 1700, value]), 120, 5)["passed"])
        self.assertFalse(pipeline.compare_screen([], 120, 5)["passed"])


if __name__ == "__main__":
    unittest.main()
