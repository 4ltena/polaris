"""Offline tests for the approved cache-cost comparison calculator."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from benchmark_costs import classify_row, price_usage, summarize_rows


SCRIPT = Path(__file__).with_name('benchmark_costs.py')


def metric(**overrides):
    row = {
        'model': 'gpt-6-astra', 'effort': 'medium', 'outcome': 'completed',
        'usage': {
            'input_tokens': 100, 'cache_read_tokens': 20,
            'cache_write_tokens': 10, 'output_tokens': 5,
        },
    }
    row.update(overrides)
    return row


class BenchmarkCostsTests(unittest.TestCase):
    def test_explicit_parent_child_contract_uses_each_models_rates(self):
        models = {'gpt-6-astra': 'xhigh', 'gpt-5.6-luna': 'xhigh'}
        child = metric(model='gpt-5.6-luna', effort='xhigh', usage={
            'input_tokens': 1000, 'cache_read_tokens': 800,
            'cache_write_tokens': 50, 'output_tokens': 200,
            'reasoning_tokens': 100,
        })
        summary = summarize_rows([metric(effort='xhigh'), child], 'parent-child',
                                 expected_models=models)
        self.assertTrue(summary['measurement_complete'])
        # Rust price/tests.rs uses the same Luna vector: round 298.5 once.
        self.assertEqual(summary['standard_api_hypothetical_usd'], '0.001394')
        self.assertEqual(summary['by_model']['gpt-5.6-luna']
                         ['known_complete_standard_api_hypothetical_usd'], '0.000299')
        self.assertEqual(summary['reasoning']['known_tokens'], 100)
        self.assertEqual(summary['reasoning']['missing_requests'], 1)
        self.assertFalse(summary['codex_credit_scenario_complete'])
        self.assertIsNone(summary['codex_credit_scenario'])
        self.assertEqual(summary['contract']['models']['gpt-5.6-luna']['effort'], 'xhigh')

    def test_luna_boundary_and_missing_credit_do_not_use_astra_prices(self):
        models = {'gpt-5.6-luna': 'xhigh'}
        for count, cost in ((272000, '0.0556'), (272001, '0.1106')):
            with self.subTest(count=count):
                row = classify_row(metric(model='gpt-5.6-luna', effort='xhigh', usage={
                    'input_tokens': count, 'cache_read_tokens': 0,
                    'cache_write_tokens': 0, 'output_tokens': 1000,
                }), expected_models=models)
                self.assertEqual(row['standard_api_hypothetical_usd'], cost)
                self.assertIsNone(row['codex_credit_scenario'])

    def test_explicit_contract_does_not_accept_undeclared_or_unpriced_models(self):
        models = {'gpt-5.6-luna': 'xhigh', 'unknown': 'medium'}
        for row, reason in ((metric(), 'wrong_model'),
                            (metric(model='unknown'), 'unknown_model_rates'),
                            (metric(model='gpt-5.6-luna'), 'wrong_effort')):
            with self.subTest(reason=reason):
                classified = classify_row(row, expected_models=models)
                self.assertFalse(classified['complete'])
                self.assertIn(reason, classified['errors'])
        with self.assertRaises(ValueError):
            summarize_rows([metric()], 'empty-contract', expected_models={})

    def test_reasoning_is_a_diagnostic_subset_and_never_an_extra_price(self):
        valid = dict(metric()['usage'], reasoning_tokens=5)
        row = classify_row(metric(usage=valid))
        self.assertEqual(row['standard_api_hypothetical_usd'], '0.001095')
        for count in (6, -1, True, '5'):
            with self.subTest(count=count):
                invalid = dict(valid, reasoning_tokens=count)
                summary = summarize_rows([metric(usage=invalid)], 'invalid-reasoning')
                self.assertFalse(summary['measurement_complete'])
                self.assertEqual(summary['reasoning']['invalid_requests'], 1)

    def test_cli_explicit_contract_keeps_default_strict_and_rejects_duplicates(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'child.metrics.jsonl'
            path.write_text(json.dumps(metric(model='gpt-5.6-luna', effort='xhigh')) + '\n')
            base = [sys.executable, str(SCRIPT), '--label', 'child', '--metrics', str(path)]
            default = subprocess.run(base, capture_output=True, text=True)
            self.assertEqual(default.returncode, 1)
            contract = ['--expect-model', 'gpt-5.6-luna=xhigh']
            result = subprocess.run(base + contract, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(json.loads(result.stdout)['measurement_complete'])
            duplicate = subprocess.run(base + contract + contract, capture_output=True, text=True)
            self.assertEqual(duplicate.returncode, 2)

    def test_cache_components_are_in_input_and_not_double_counted(self):
        row = classify_row(metric())
        self.assertTrue(row['complete'])
        self.assertEqual(row['standard_api_hypothetical_usd'], '0.001095')
        self.assertIsNone(row['codex_credit_scenario'])
        self.assertFalse(row['codex_credit_scenario_complete'])

    def test_zero_is_complete_but_missing_write_is_incomplete(self):
        zero = classify_row(metric(usage={
            'input_tokens': 0, 'cache_read_tokens': 0,
            'cache_write_tokens': 0, 'output_tokens': 0,
        }))
        self.assertTrue(zero['complete'])
        self.assertEqual(zero['codex_credit_scenario'], '0')
        missing = classify_row(metric(usage={
            'input_tokens': 1, 'cache_read_tokens': 0, 'output_tokens': 0,
        }))
        self.assertFalse(missing['complete'])
        self.assertIn('missing_cache_write_tokens', missing['errors'])

    def test_rejects_floats_bools_negatives_and_invalid_totals(self):
        for field, value in (
            ('input_tokens', 1.0), ('output_tokens', True),
            ('cache_read_tokens', -1),
        ):
            with self.subTest(field=field):
                usage = dict(metric()['usage']); usage[field] = value
                self.assertFalse(classify_row(metric(usage=usage))['complete'])
        total = dict(metric()['usage'], total_tokens=999)
        self.assertIn('total_mismatch', classify_row(metric(usage=total))['errors'])

    def test_cache_write_and_output_are_priced(self):
        row = classify_row(metric(usage={
            'input_tokens': 100, 'cache_read_tokens': 0,
            'cache_write_tokens': 100, 'output_tokens': 10,
        }))
        self.assertEqual(row['standard_api_hypothetical_usd'], '0.00175')

    def test_long_context_multiplier_starts_above_boundary(self):
        base = {'cache_read_tokens': 0, 'cache_write_tokens': 0, 'output_tokens': 1}
        at_limit = classify_row(metric(usage=dict(base, input_tokens=272000)))
        above = classify_row(metric(usage=dict(base, input_tokens=272001)))
        self.assertEqual(at_limit['standard_api_hypothetical_usd'], '2.72005')
        self.assertEqual(above['standard_api_hypothetical_usd'], '5.440095')

    def test_wrong_model_or_effort_is_not_comparison_complete(self):
        self.assertIn('wrong_model', classify_row(metric(model='gpt-6-other'))['errors'])
        self.assertIn('wrong_effort', classify_row(metric(effort='high'))['errors'])

    def test_failed_usage_is_reported_separately(self):
        failed = metric(outcome='http_error', usage={
            'input_tokens': 9, 'cache_read_tokens': None,
            'cache_write_tokens': None, 'output_tokens': 3,
        })
        summary = summarize_rows([metric(), failed], 'approved')
        self.assertEqual(summary['requests'], {'seen': 2, 'complete': 1, 'incomplete': 1, 'failed': 1})
        self.assertEqual(summary['failed_partial_usage']['input_tokens'], 9)
        self.assertEqual(summary['failed_partial_usage']['output_tokens'], 3)
        self.assertFalse(summary['measurement_complete'])
        self.assertIsNone(summary['standard_api_hypothetical_usd'])
        self.assertEqual(summary['known_complete_standard_api_hypothetical_usd'], '0.001095')

    def test_credit_scenario_is_complete_only_when_every_write_is_zero(self):
        zero_write = metric(usage={
            'input_tokens': 100, 'cache_read_tokens': 20,
            'cache_write_tokens': 0, 'output_tokens': 5,
        })
        complete = summarize_rows([zero_write], 'approved')
        self.assertTrue(complete['measurement_complete'])
        self.assertTrue(complete['codex_credit_scenario_complete'])
        self.assertEqual(complete['codex_credit_scenario'], '0.02675')
        with_write = summarize_rows([metric()], 'approved')
        self.assertTrue(with_write['measurement_complete'])
        self.assertFalse(with_write['codex_credit_scenario_complete'])
        self.assertIsNone(with_write['codex_credit_scenario'])

    def test_a_model_the_table_says_has_no_cache_is_priced_without_one(self):
        import benchmark_costs
        original = benchmark_costs.CACHE_APPLIES
        benchmark_costs.CACHE_APPLIES = False
        try:
            # Nothing is reported for either share because there is no cache.
            absent = classify_row(metric(usage={
                'input_tokens': 1000, 'output_tokens': 200,
            }))
            self.assertTrue(absent['complete'], absent['errors'])
            # 1,000 at 10 plus 200 at 50, per million: the same figure the
            # Rust module produces for the same rule.
            self.assertEqual(absent['standard_api_hypothetical_usd'], '0.02')
            # Zero is what such a model consistently reports.
            zero = classify_row(metric(usage={
                'input_tokens': 1000, 'cache_read_tokens': 0,
                'cache_write_tokens': 0, 'output_tokens': 200,
            }))
            self.assertTrue(zero['complete'], zero['errors'])
            self.assertEqual(zero['standard_api_hypothetical_usd'], '0.02')
            # A figure above zero contradicts the table and is left showing.
            for field in ('cache_read_tokens', 'cache_write_tokens'):
                contradicted = classify_row(metric(usage={
                    'input_tokens': 1000, 'output_tokens': 200, field: 1,
                }))
                self.assertFalse(contradicted['complete'])
                self.assertIn('cache_declared_absent', contradicted['errors'])
        finally:
            benchmark_costs.CACHE_APPLIES = original

    def test_an_unreadable_cache_declaration_is_refused(self):
        import benchmark_costs
        self.assertTrue(benchmark_costs._cache_applies({}))
        self.assertFalse(benchmark_costs._cache_applies({'cache': 'not_applicable'}))
        with self.assertRaises(ValueError):
            benchmark_costs._cache_applies({'cache': 'maybe'})

    def test_cli_reads_explicit_folder_and_emits_only_machine_json(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'one.metrics.jsonl').write_text(json.dumps(metric()) + '\n')
            (root / 'ignored.jsonl').write_text(json.dumps(metric()) + '\n')
            result = subprocess.run(
                [sys.executable, str(SCRIPT), '--label', 'candidate', '--metrics-folder', str(root)],
                capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            payload = json.loads(result.stdout)
            self.assertEqual(payload['sources'], {'metric_files': 1, 'invalid_json_lines': 0, 'empty_files': 0})
            self.assertTrue(payload['contract']['codex_credit_scenario_is_not_invoice_or_rate_limit'])
            self.assertEqual(payload['contract']['rates_as_of'], '2026-09-07')
            self.assertIn('https://developers.openai.com/api/docs/models/gpt-6-astra',
                          payload['contract']['pricing_sources'])

    def test_cli_malformed_or_empty_inputs_are_incomplete_and_nonzero(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            malformed = root / 'malformed.metrics.jsonl'
            malformed.write_text(json.dumps(metric()) + '\nnot-json\n')
            result = subprocess.run(
                [sys.executable, str(SCRIPT), '--label', 'candidate', '--metrics', str(malformed)],
                capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 1)
            payload = json.loads(result.stdout)
            self.assertFalse(payload['measurement_complete'])
            self.assertIsNone(payload['standard_api_hypothetical_usd'])
            self.assertEqual(payload['sources']['invalid_json_lines'], 1)
            empty = root / 'empty.metrics.jsonl'
            empty.write_text('')
            result = subprocess.run(
                [sys.executable, str(SCRIPT), '--label', 'empty', '--metrics', str(empty)],
                capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 1)
            self.assertFalse(json.loads(result.stdout)['measurement_complete'])

    def test_cli_rejects_empty_source_mixed_with_valid_source(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'valid.metrics.jsonl').write_text(json.dumps(metric()) + '\n')
            (root / 'empty.metrics.jsonl').write_text(' \n')
            result = subprocess.run(
                [sys.executable, str(SCRIPT), '--label', 'mixed', '--metrics-folder', str(root)],
                capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 1)
            payload = json.loads(result.stdout)
            self.assertFalse(payload['measurement_complete'])
            self.assertIsNone(payload['standard_api_hypothetical_usd'])
            self.assertEqual(payload['sources']['empty_files'], 1)

    def test_cli_rejects_duplicate_explicit_or_folder_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            valid = root / 'valid.metrics.jsonl'
            valid.write_text(json.dumps(metric()) + '\n')
            for extra in (['--metrics', str(valid)], ['--metrics-folder', str(root)]):
                result = subprocess.run(
                    [sys.executable, str(SCRIPT), '--label', 'duplicate', '--metrics', str(valid)] + extra,
                    capture_output=True, text=True,
                )
                self.assertEqual(result.returncode, 2)
                self.assertIn('duplicate metric paths', result.stderr)


    def test_price_usage_prices_both_arms_from_one_table(self):
        from decimal import Decimal
        price = price_usage
        # Under the long-context threshold the published per-million rates
        # apply directly: 10 billed input, 1 cache read, 50 output.
        self.assertEqual(price('gpt-6-astra', 1000, 0, 0, 0), Decimal('0.010000'))
        self.assertEqual(price('gpt-6-astra', 1000, 1000, 0, 0), Decimal('0.001000'))
        self.assertEqual(price('gpt-6-astra', 0, 0, 0, 1000), Decimal('0.050000'))
        # Cache read is only the cached share; the rest stays billed input.
        self.assertEqual(price('gpt-6-astra', 1000, 400, 0, 0), Decimal('0.006400'))
        # Above 272,000 input tokens the table's own multipliers apply.
        self.assertEqual(price('gpt-6-astra', 300_000, 0, 0, 0), Decimal('6.000000'))
        self.assertEqual(price('gpt-6-astra', 300_000, 0, 0, 0,
                               long_context_multipliers=False), Decimal('3.000000'))
        # A model the table does not name has no price. Borrowing another
        # model's figures would look like a measurement and be none.
        with self.assertRaises(KeyError):
            price('no-such-model', 1, 0, 0, 0)
        # Counts that cannot be true are refused rather than silently clamped.
        with self.assertRaises(ValueError):
            price('gpt-6-astra', 10, 20, 0, 0)
        with self.assertRaises(ValueError):
            price('gpt-6-astra', -1, 0, 0, 0)
        with self.assertRaises(ValueError):
            price('gpt-6-astra', 1.5, 0, 0, 0)


if __name__ == '__main__':
    unittest.main()
