"""Offline physical-attempt accounting regressions; no provider calls."""
import copy
from decimal import Decimal
import json
from pathlib import Path
import tempfile
import unittest

import benchmark_jev as jev
from benchmark_costs import price_usage


def pair(identity='1', *, outcome='completed', input_tokens=120, output_tokens=0):
    start = dict(schema_version=1, provider='jev', model='jev-latest', event='started',
                 logical_id=identity, attempt_id=identity + '-1', attempt_number=1,
                 started_unix_ms=1, finished_unix_ms=None, http_status=None, outcome='running',
                 usage=dict(input_tokens=None, output_tokens=None))
    end = copy.deepcopy(start)
    end.update(event='finished', finished_unix_ms=2, http_status=200, outcome=outcome,
               usage=dict(input_tokens=input_tokens, output_tokens=output_tokens))
    return [start, end]


class JevTests(unittest.TestCase):
    def audit(self, rows, calls=1):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'jev.jsonl'
            path.write_text(''.join(json.dumps(row) + '\n' for row in rows))
            return jev.audit(path, expected_logical_calls=calls)

    def test_physical_pairs_are_counted_once_and_missing_output_is_not_zero(self):
        got = self.audit(pair())
        self.assertTrue(got['measurement_complete'])
        self.assertEqual(got['physical_attempts'], 1)
        self.assertEqual(got['tokens'], {'input': 120, 'output': 0})
        self.assertEqual(Decimal(got['usd']), Decimal('0.00000504'))
        got = self.audit(pair(output_tokens=None))
        self.assertFalse(got['measurement_complete'])
        self.assertIsNone(got['tokens']['output'])
        self.assertEqual(Decimal(got['usd']), Decimal('0.00000504'))

    def test_invalid_answer_keeps_known_cost_but_invalidates_comparison(self):
        got = self.audit(pair(outcome='invalid_answer'))
        self.assertFalse(got['measurement_complete'])
        self.assertEqual(got['tokens']['input'], 120)
        self.assertEqual(Decimal(got['usd']), Decimal('0.00000504'))

    def test_failed_and_unfinished_requests_never_become_free(self):
        rows = pair(outcome='unavailable', input_tokens=None, output_tokens=None)
        rows[1]['http_status'] = 429
        for values in (rows, rows[:1]):
            got = self.audit(values)
            self.assertEqual(got['physical_attempts'], 1)
            self.assertFalse(got['measurement_complete'])
            self.assertIsNone(got['usd'])
            self.assertIsNone(got['tokens']['input'])

    def test_duplicate_or_mismatched_terminals_are_not_trusted(self):
        rows = pair()
        changed = copy.deepcopy(rows)
        changed[1]['logical_id'] = 'wrong'
        for values in (rows + rows[1:], changed, rows[::-1], [False], [dict(rows[0], payload='secret')]):
            got = self.audit(values)
            self.assertFalse(got['measurement_complete'])
            self.assertIsNone(got['usd'])

    def test_retries_keep_the_failed_physical_attempt(self):
        first = pair(outcome='unavailable', input_tokens=None, output_tokens=None)
        first[1]['http_status'] = 503
        second = pair()
        for row in second:
            row.update(attempt_id='1-2', attempt_number=2)
        got = self.audit(first + second)
        self.assertEqual(got['physical_attempts'], 2)
        self.assertEqual(got['logical_calls'], 1)
        self.assertIsNone(got['usd'])
        self.assertEqual(Decimal(got['known_usd']), Decimal('0.00000504'))

    def test_zero_calls_are_distinct_from_missing_journal(self):
        got = self.audit([], calls=0)
        self.assertTrue(got['measurement_complete'])
        self.assertEqual(got['status'], 'not_exercised')
        self.assertEqual(got['usd'], '0')
        self.assertFalse(self.audit([], calls=1)['measurement_complete'])
        with tempfile.TemporaryDirectory() as directory:
            got = jev.audit(Path(directory) / 'missing', expected_logical_calls=0)
            self.assertFalse(got['measurement_complete'])
            self.assertIsNone(got['usd'])

    def test_two_successful_attempts_cannot_satisfy_one_logical_call(self):
        for number in (1, 2):
            second = pair()
            for row in second:
                row.update(attempt_id='1-2', attempt_number=number)
            got = self.audit(pair() + second)
            self.assertFalse(got['measurement_complete'])
            self.assertIsNone(got['usd'])
            self.assertEqual(got['known_tokens']['input'], 240)
            self.assertIn('unexpected_physical_attempt_count', got['issues'])

    def test_new_openai_rates_retain_cache_and_request_context_rules(self):
        self.assertEqual(price_usage('gpt-5.6-terra', 1000000, 0, 0, 1000000), Decimal('22'))
        self.assertEqual(price_usage('gpt-5.6-sol', 1000000, 0, 0, 1000000), Decimal('38'))
        self.assertEqual(price_usage('gpt-5.6-terra', 1000, 500, 200, 100), Decimal('0.002400'))


if __name__ == '__main__':
    unittest.main()
