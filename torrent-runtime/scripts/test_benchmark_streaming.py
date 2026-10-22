"""Acceptance must fail closed when measurements are missing or regress."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('benchmark', Path(__file__).with_name('benchmark-streaming.py'))
benchmark = importlib.util.module_from_spec(spec); spec.loader.exec_module(benchmark)


class AcceptanceTest(unittest.TestCase):
    def rows(self):
        return [dict(case=case, state='cold', engine=engine, trial=trial, passed=True,
                     first_frame_seconds=1., forward_seconds=1., backward_seconds=1.,
                     sustained_seconds=600, seconds_without_new_frames=0)
                for case in range(2) for engine in ['candidate', 'stremio'] for trial in range(20)]

    def test_equal_is_accepted_only_with_the_complete_measurements(self):
        rows = self.rows()
        self.assertTrue(benchmark.performance_gate(rows)['passed'])
        self.assertFalse(benchmark.performance_gate(rows[1:])['passed'])
        for row in rows: row.pop('sustained_seconds')
        self.assertFalse(benchmark.performance_gate(rows)['passed'])

    def test_fast_case_does_not_hide_another_cases_tail_regression(self):
        rows = self.rows()
        for row in rows:
            if row['engine']=='candidate':
                if row['case']==0: row['forward_seconds']=.01
                elif row['trial']<3: row['forward_seconds']=2.
        self.assertFalse(benchmark.performance_gate(rows)['passed'])

    def test_failures_and_missing_metrics_remain_in_the_statistics(self):
        rows = self.rows()
        for row in rows:
            if row['engine']=='candidate' and row['trial']<3:
                row['passed']=False; row.pop('forward_seconds')
        self.assertFalse(benchmark.performance_gate(rows)['passed'])
        report=benchmark.summary(rows,20)
        cell=next(c for c in report['cells'] if c['engine']=='candidate')
        self.assertEqual(cell['failures'],3)
        self.assertIsNone(cell['forward_seconds']['p90'])

    def test_all_failed_cannot_qualify_even_with_equal_reported_timings(self):
        rows=self.rows()
        for row in rows: row['passed']=False
        self.assertFalse(benchmark.performance_gate(rows)['passed'])

    def test_cold_gate_never_uses_warm_or_seek_improvements(self):
        rows=self.rows()
        for row in rows:
            row.pop('forward_seconds'); row.pop('backward_seconds'); row.pop('sustained_seconds')
        self.assertTrue(benchmark.cold_start_gate(rows)['passed'])
        for row in rows:
            if row['engine']=='candidate' and row['case']==1: row['first_frame_seconds']=1.01
        self.assertFalse(benchmark.cold_start_gate(rows)['passed'])
        for row in rows: row['state']='retained'
        self.assertFalse(benchmark.cold_start_gate(rows)['passed'])

    def test_cold_gate_requires_matching_unique_trial_ids(self):
        rows=self.rows()
        rows[0]['trial']=20
        self.assertFalse(benchmark.cold_start_gate(rows)['passed'])
        rows[0]['trial']=1
        self.assertFalse(benchmark.cold_start_gate(rows)['passed'])

    def test_finite_candidate_beats_failure_censored_reference(self):
        rows = self.rows()
        for row in rows:
            if row['engine'] == 'stremio' and row['trial'] < 3:
                row['passed'] = False
        self.assertTrue(benchmark.cold_start_gate(rows)['passed'])
        cell = next(c for c in benchmark.summary(rows, 20)['cells'] if c['engine'] == 'stremio')
        self.assertIsNone(cell['first_frame_seconds']['p90'])
        self.assertEqual(cell['first_frame_seconds']['p90_status'], 'censored_failure')

    def test_missing_successful_reference_measurement_cannot_pass(self):
        rows = self.rows()
        next(row for row in rows if row['engine'] == 'stremio').pop('first_frame_seconds')
        self.assertFalse(benchmark.cold_start_gate(rows)['passed'])

    def test_failed_candidate_timings_are_censored_even_if_recorded(self):
        rows = self.rows()
        for row in rows:
            if row['engine'] == 'candidate' and row['trial'] < 3:
                row['passed'] = False
                row['first_frame_seconds'] = .001
        cell = next(c for c in benchmark.summary(rows, 20)['cells'] if c['engine'] == 'candidate')
        self.assertIsNone(cell['first_frame_seconds']['p90'])
        self.assertFalse(benchmark.cold_start_gate(rows)['passed'])


if __name__=='__main__':unittest.main()
