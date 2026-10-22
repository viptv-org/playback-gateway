import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("merge", Path(__file__).with_name("merge-cold-results.py"))
merge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(merge)


class ResumeTest(unittest.TestCase):
    def batch(self, offset, count):
        run = dict(seed=20261022, states=["cold"], startup_only=True,
                   metadata_timeout_ms=30000, render_timeout_ms=30000,
                   candidate_grace="0", candidate_profile=None,
                   sha256=dict(candidate="same-worker", stremio="same-reference", launcher="same-player"),
                   trials=count, trial_offset=offset)
        rows = [dict(trial=trial, case=case, engine=engine, state="cold", passed=True,
                     hash=str(case), first_frame_seconds=5)
                for trial, case, order in merge.desktop.trial_schedule(2, ["candidate", "stremio"], count, offset)
                for engine in order]
        return run, rows

    def test_three_screen_pairs_count_toward_twenty(self):
        rows, count = merge.merge_batches([self.batch(0, 3), self.batch(3, 17)], ["candidate", "stremio"])
        self.assertEqual((len(rows), count), (80, 20))
        self.assertTrue(merge.desktop.screen.cold_start_gate(rows)["passed"])

    def test_failures_are_preserved(self):
        first, rest = self.batch(0, 3), self.batch(3, 17)
        next(row for row in first[1] if row["engine"] == "candidate")["passed"] = False
        rows, _ = merge.merge_batches([first, rest], ["candidate", "stremio"])
        self.assertEqual(sum(not row["passed"] for row in rows), 1)
        self.assertFalse(merge.desktop.screen.cold_start_gate(rows)["passed"])

    def test_changed_worker_or_source_cannot_be_pooled(self):
        for kind in ("worker", "source"):
            with self.subTest(kind=kind):
                rest = copy.deepcopy(self.batch(3, 17))
                if kind == "worker":
                    rest[0]["sha256"]["candidate"] = "different-worker"
                else:
                    for row in rest[1]:
                        row["hash"] = "different-source"
                with self.assertRaises(ValueError):
                    merge.merge_batches([self.batch(0, 3), rest], ["candidate", "stremio"])

    def test_gaps_duplicates_and_reordering_are_rejected(self):
        for offset in (2, 4):
            with self.assertRaises(ValueError):
                merge.merge_batches([self.batch(0, 3), self.batch(offset, 17)], ["candidate", "stremio"])
        for mutation in (lambda rows: rows.append(rows[-1]), lambda rows: rows.reverse(), lambda rows: rows.pop()):
            batch = self.batch(0, 3)
            mutation(batch[1])
            with self.assertRaises(ValueError):
                merge.merge_batches([batch], ["candidate", "stremio"])


if __name__ == "__main__":
    unittest.main()
