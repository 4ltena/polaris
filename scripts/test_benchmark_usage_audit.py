"""Offline regressions for contaminated and incomplete benchmark evidence."""
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import benchmark_usage_audit as audit


def usage(tokens=100, output=5):
    return dict(zip(audit.POLARIS_FIELDS, (tokens, 0, 0, output, 1)))


def metric(tokens=100, timestamp=1500, **extra):
    return {'model': 'gpt-6-astra', 'effort': 'xhigh', 'started_unix_ms': timestamp,
            'usage': usage(tokens), **extra}


def context(model='gpt-6-astra', effort='medium'):
    return {'type': 'turn_context', 'payload': {'model': model, 'effort': effort}}


def notification(tokens=100, cumulative=100):
    last = dict(zip(audit.CODEX_FIELDS, (tokens, 0, 0, 5, 1)))
    total = {k: v * (cumulative // tokens) for k, v in last.items()}
    return {'type': 'event_msg', 'payload': {'type': 'token_count', 'info': {
        'last_token_usage': last, 'total_token_usage': total}}}


class AuditTests(unittest.TestCase):
    def write(self, path, rows):
        path.write_text('\n'.join(json.dumps(r) for r in rows)+'\n')

    def test_prior_attempt_is_excluded_from_comparison_but_kept_as_consumption(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'metrics.jsonl'
            self.write(path, [metric(100, 900), metric(200, 1500), metric(300, 2100)])
            rows, excluded, unscoped, errors = audit.polaris_requests(path, started=1, finished=2)
            self.assertEqual(unscoped, [])
            self.assertEqual(audit.summarize(rows, errors)['tokens']['input'], 200)
            self.assertEqual(audit.summarize(excluded)['tokens']['input'], 400)
            self.assertEqual(audit.summarize(rows)['requests'], 1)

    def test_identical_cumulative_notifications_count_once_per_rollout(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'rollout.jsonl'
            self.write(path, [context(), notification(), notification(),
                              notification(cumulative=200)])
            rows, duplicates, errors = audit.codex_requests([path])
            self.assertEqual(len(duplicates), 1)
            self.assertEqual(audit.summarize(rows, errors)['tokens']['input'], 200)
            self.assertEqual(len(rows), 2)
            child = Path(directory)/'child.jsonl'
            self.write(child, [context(), notification()])
            rows, duplicates, errors = audit.codex_requests([path, child])
            self.assertEqual(audit.summarize(rows, errors)['tokens']['input'], 300)

    def test_repeated_snapshot_after_model_switch_is_not_assigned_to_new_model(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'rollout.jsonl'
            self.write(path, [context(), notification(), context('gpt-5.6-luna'),
                              notification(), notification(cumulative=200)])
            rows, duplicates, errors = audit.codex_requests([path])
            result = audit.summarize(rows, errors)
            self.assertEqual(len(duplicates), 1)
            self.assertEqual(result['models']['gpt-5.6-luna']['requests'], 1)
            self.assertEqual(result['tokens']['input'], 200)

    def test_gaps_initial_partial_and_resets_are_not_exact_totals(self):
        for events in ([notification(cumulative=200)],
                       [notification(), notification(cumulative=300)],
                       [notification(cumulative=200), notification()]):
            with self.subTest(events=events), tempfile.TemporaryDirectory() as directory:
                path = Path(directory)/'rollout.jsonl'
                self.write(path, [context(), *events])
                rows, _, errors = audit.codex_requests([path])
                result = audit.summarize(rows, errors)
                self.assertTrue(any(e.startswith('cumulative_mismatch') for e in errors))
                self.assertIsNone(result['usd'])
                self.assertIsNone(result['tokens']['input'])

    def test_unknown_cumulative_is_not_deduplicated(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'rollout.jsonl'
            item = notification()
            del item['payload']['info']['total_token_usage']
            self.write(path, [context(), item, item])
            rows, duplicates, errors = audit.codex_requests([path])
            self.assertEqual(len(rows), 2)
            self.assertEqual(duplicates, [])
            self.assertIsNone(audit.summarize(rows, errors)['usd'])

    def test_long_context_surcharge_is_per_request_not_sum(self):
        rows = [audit.request('gpt-6-astra', 'medium', usage(200000, 10),
                              audit.POLARIS_FIELDS, f'row:{i}') for i in (1, 2)]
        self.assertEqual(audit.summarize(rows)['usd'], '4.001000')
        long = audit.request('gpt-6-astra', 'medium', usage(400000, 20),
                             audit.POLARIS_FIELDS, 'row:1')
        self.assertEqual(audit.summarize([long])['usd'], '8.001500')

    def test_missing_cache_is_unknown_even_when_input_is_present(self):
        value = usage()
        del value['cache_read_tokens']
        row = audit.request('gpt-6-astra', None, value, audit.POLARIS_FIELDS, 'row:1')
        result = audit.summarize([row])
        self.assertIsNone(result['usd'])
        self.assertIsNone(result['tokens']['cache_read'])
        self.assertEqual(result['tokens']['input'], 100)
        self.assertEqual(result['missing_fields'], {'cache_read': 1})
        self.assertEqual(result['known_usd'], '0.000250')

    def test_known_output_keeps_its_long_context_multiplier(self):
        value = usage(400000, 10)
        value['cache_read_tokens'] = None
        row = audit.request('gpt-6-astra', None, value, audit.POLARIS_FIELDS, '1')
        self.assertEqual(audit.summarize([row])['known_usd'], '0.000750')
        value['cache_read_tokens'] = 400001
        row = audit.request('gpt-6-astra', None, value, audit.POLARIS_FIELDS, '1')
        self.assertEqual(audit.summarize([row])['known_usd'], '0.000750')

    def test_partial_and_unknown_model_prices_are_never_complete(self):
        rows = [audit.request('gpt-6-astra', 'medium', usage(), audit.POLARIS_FIELDS, '1'),
                audit.request('unlisted-test-model', 'high', usage(), audit.POLARIS_FIELDS, '2')]
        result = audit.summarize(rows)
        self.assertIsNone(result['usd'])
        self.assertEqual(result['unpriced_models'], {'unlisted-test-model': 1})
        self.assertEqual(result['known_usd'], '0.001250')
        self.assertEqual(result['priced_requests'], 1)

    def test_model_and_effort_follow_context_not_notification(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'rollout.jsonl'
            self.write(path, [context(), context('gpt-5.6-luna', 'high'),
                              notification(), context('gpt-6-astra', None),
                              notification(cumulative=200)])
            rows, _, errors = audit.codex_requests([path])
            result = audit.summarize(rows, errors)
            self.assertEqual(result['models']['gpt-5.6-luna']['efforts'], ['high'])
            self.assertEqual(result['models']['gpt-6-astra']['efforts'], [None])
            self.assertEqual(result['usd'], '0.001276')

    def test_invalid_counts_cache_and_reasoning_do_not_get_a_price(self):
        for changed in ({'input_tokens': True}, {'output_tokens': -1},
                        {'cache_read_tokens': 101}, {'reasoning_tokens': 6}):
            with self.subTest(changed=changed):
                row = audit.request('gpt-6-astra', None, usage() | changed,
                                    audit.POLARIS_FIELDS, '1')
                self.assertIsNone(audit.summarize([row])['usd'])

    def test_malformed_or_unscopable_rows_invalidate_total(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'metrics.jsonl'
            self.write(path, [metric(), metric(started_unix_ms=None)])
            with path.open('a') as output:
                output.write('broken\n')
            rows, _, unscoped, errors = audit.polaris_requests(path, started=1, finished=2)
            result = audit.summarize(rows, errors)
            self.assertEqual(len(errors), 2)
            self.assertIsNone(result['usd'])
            self.assertIsNone(result['tokens']['input'])
            self.assertEqual(result['known_tokens']['input'], 100)
            self.assertEqual(audit.summarize(unscoped)['known_tokens']['input'], 100)

    def test_empty_evidence_has_no_measured_total(self):
        result = audit.summarize([])
        self.assertIsNone(result['usd'])
        self.assertIsNone(result['tokens']['input'])


if __name__ == '__main__':
    unittest.main()
