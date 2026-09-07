"""送信間隔の違反や欠測を合格にしないための検査。"""
import copy
import unittest
from benchmark_cache_pacing import summarize_pacing


def rows(mode='on', count=14):
    interval = 5000 if mode == 'on' else 0
    return [{'started_unix_ms': 100000 + n * 5000,
             'usage': {'input_tokens': 2048},
             'cache_pacing': {'mode': mode, 'interval_ms': interval,
                              'wait_ms': 100 if mode == 'on' and n else 0,
                              'dispatch_offset_ms': n * 5000}}
            for n in range(count)]


class PacingTests(unittest.TestCase):
    def test_half_open_window_and_spacing(self):
        report = summarize_pacing(rows(), 'on')
        self.assertEqual(report['max_starts_in_60s'], 12)
        self.assertEqual(report['min_dispatch_gap_ms'], 5000)
        self.assertEqual(report['total_wait_ms'], 1300)

    def test_first_request_must_be_immediate(self):
        traces = rows()
        traces[0]['cache_pacing']['wait_ms'] = 5000
        with self.assertRaises(ValueError):
            summarize_pacing(traces, 'on')

    def test_short_gap_is_not_accepted(self):
        traces = rows()
        traces[1]['cache_pacing']['dispatch_offset_ms'] = 4999
        with self.assertRaises(ValueError):
            summarize_pacing(traces, 'on')

    def test_missing_wrong_mode_and_bad_types_fail(self):
        for replacement in ({}, {'mode': 'off'}, {'mode': 'on', 'interval_ms': True},
                            {'mode': 'on', 'interval_ms': 5000, 'wait_ms': -1}):
            traces = rows()
            traces[0]['cache_pacing'] = replacement
            with self.assertRaises(ValueError):
                summarize_pacing(traces, 'on')

    def test_off_does_not_allow_recorded_wait(self):
        traces = rows('off')
        self.assertEqual(summarize_pacing(traces, 'off')['interval_ms'], 0)
        traces[1]['cache_pacing']['wait_ms'] = 1
        with self.assertRaises(ValueError):
            summarize_pacing(traces, 'off')

    def test_clock_adjustment_does_not_change_monotonic_gate(self):
        traces = rows()
        traces[1]['started_unix_ms'] = 1
        self.assertLess(summarize_pacing(traces, 'on')['wall_clock_min_gap_ms'], 0)

    def test_empty_and_reordered_evidence_fail(self):
        for traces in ([], list(reversed(copy.deepcopy(rows())))):
            with self.assertRaises(ValueError):
                summarize_pacing(traces, 'on')


if __name__ == '__main__':
    unittest.main()
