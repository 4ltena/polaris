"""Algedi benchmark のオフライン安全性と課題契約。"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from benchmark_algedi import PROMPTS, TASKS, SafeEvaluationError, SafeProgram, quality, quality_result, task_files

RUNNER = Path(__file__).with_name('benchmark_algedi.py')
FAKE = '''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
argv = __import__('sys').argv
assert argv[argv.index('--model') + 1] == 'gpt-6-astra'
assert argv[argv.index('--effort') + 1] == 'medium'
assert os.environ.get('POLARIS_AUTH_READ_ONLY') == '1'
assert os.environ.get('POLARIS_CACHE_PREFIX') == os.environ.get('ALGEDI_EXPECT_PREFIX', 'compact')
assert os.environ.get('POLARIS_CACHE_PACING') == os.environ.get('ALGEDI_EXPECT_PACING', 'off')
assert os.environ.get('POLARIS_TURN_AFFINITY') == os.environ.get('ALGEDI_EXPECT_AFFINITY', 'off')
root = Path.cwd(); prompt = argv[argv.index('--prompt') + 1]
if '送料' in prompt:
    path = root / 'rates.py'; path.write_text("def shipping_fee(weight_g):\\n    if weight_g < 0: raise ValueError('negative')\\n    extra = max(0, weight_g - 1000)\\n    return 500 + (extra + 999) // 1000 * 200\\n")
elif '税込価格' in prompt:
    (root / 'pricing.py').write_text('def total(subtotal):\\n    return subtotal * 110 // 100\\n')
    (root / 'receipt.py').write_text('from pricing import total\\ndef render(subtotal):\\n    return f"total={total(subtotal)}"\\n')
elif '原因となった ERROR' in prompt: print(json.dumps({'root_cause':'TLS_CERT_EXPIRED','serial':'AL-42'}))
elif '最終承認' in prompt: print(json.dumps({'decision_id':'D011','retention_days':30}))
else:
    assert 'retention_days=45' in (root / 'current_source.md').read_text()
    print(json.dumps({'evidence_id':'EV-7','saved_retention_days':30,'current_retention_days':45,'changed':True}))
record = {'model':'gpt-6-astra','effort':'medium','outcome':'completed','usage_missing':False,'usage':{'input_tokens':100,'output_tokens':10,'cache_read_tokens':20}}
Path(os.environ['POLARIS_METRICS_PATH']).write_text(json.dumps(record)+'\\n')
if os.environ.get('ALGEDI_TEST_MODE') == 'rates-symlink':
    (root / 'rates.py').unlink(); (root / 'rates.py').symlink_to('/nonexistent-algedi-target')
if os.environ.get('ALGEDI_TEST_MODE') == 'rates-deleted':
    (root / 'rates.py').unlink()
if os.environ.get('ALGEDI_TEST_MODE') == 'embedding-missing':
    print('tool-memory-embedding: {"input_tokens":null,"ok":false}', file=sys.stderr)
print('tokens: in 100 / out 10 / cache 20 / total 110', file=sys.stderr)
print('使用量の計測: 応答あり 1 / 欠測 0 / 失敗 0', file=sys.stderr)
'''


class AlgediTests(unittest.TestCase):
    def test_pacing_cannot_inherit_a_different_condition(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / 'fake'
            binary.write_text(FAKE)
            binary.chmod(0o700)
            for mode in ('off', 'on'):
                output = root / mode
                env = {**os.environ, 'POLARIS_AUTH_READ_ONLY': '0', 'POLARIS_CACHE_PACING': 'inherited-invalid',
                       'ALGEDI_EXPECT_PACING': mode}
                env.pop('POLARIS_CACHE_MODE', None)
                command = [sys.executable, str(RUNNER), '--binary', str(binary),
                           '--output', str(output), '--label', mode, '--repeats', '1',
                           '--tasks', 'changed_source_evidence', '--cache-pacing', mode]
                run = subprocess.run(command, env=env, capture_output=True, text=True)
                self.assertEqual(run.returncode, 0, run.stderr)
                self.assertEqual(json.loads((output / 'metadata.json').read_text())['cache_pacing'], mode)

    def test_affinity_cannot_inherit_a_different_condition(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / 'fake'
            binary.write_text(FAKE)
            binary.chmod(0o700)
            for mode in ('off', 'on'):
                output = root / mode
                env = {**os.environ, 'POLARIS_TURN_AFFINITY': 'inherited-invalid',
                       'ALGEDI_EXPECT_AFFINITY': mode}
                env.pop('POLARIS_CACHE_MODE', None)
                command = [sys.executable, str(RUNNER), '--binary', str(binary),
                           '--output', str(output), '--label', mode, '--repeats', '1',
                           '--tasks', 'changed_source_evidence', '--turn-affinity', mode]
                run = subprocess.run(command, env=env, capture_output=True, text=True)
                self.assertEqual(run.returncode, 0, run.stderr)
                self.assertEqual(json.loads((output / 'metadata.json').read_text())['turn_affinity'], mode)

    def test_numeric_remainder_repair_and_wrong_divisor(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = ('def shipping_fee(weight_g):\n'
                      '    if weight_g < 0: raise ValueError("negative")\n'
                      '    excess_g = max(0, weight_g - 1000)\n'
                      '    units = excess_g // 1000 + (excess_g % 1000 > 0)\n'
                      '    return 500 + units * 200\n')
            for candidate, expected in ((source, 'pass'),
                                        (source.replace('% 1000', '% 2000'), 'incorrect'),
                                        (source.replace('% 1000', '% 0'), 'unassessed')):
                (root / 'rates.py').write_text(candidate)
                self.assertEqual(quality_result('singlefile_repair', '', root)[0], expected)

    def test_profile_is_explicit_and_cannot_inherit_a_different_condition(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / 'fake'
            binary.write_text(FAKE)
            binary.chmod(0o700)
            for profile in ('compact', 'stable'):
                output = root / profile
                env = {**os.environ, 'POLARIS_CACHE_PREFIX': 'inherited-invalid',
                       'ALGEDI_EXPECT_PREFIX': profile}
                env.pop('POLARIS_CACHE_MODE', None)
                command = [sys.executable, str(RUNNER), '--binary', str(binary),
                           '--output', str(output), '--label', profile, '--repeats', '1',
                           '--tasks', 'changed_source_evidence', '--cache-prefix', profile]
                run = subprocess.run(command, env=env, capture_output=True, text=True)
                self.assertEqual(run.returncode, 0, run.stderr)
                self.assertEqual(json.loads((output / 'metadata.json').read_text())['cache_prefix'], profile)
            env['POLARIS_CACHE_MODE'] = 'implicit'
            command[command.index('--output') + 1] = str(root / 'rejected')
            run = subprocess.run(command, env=env, capture_output=True, text=True)
            self.assertNotEqual(run.returncode, 0)
            self.assertIn('POLARIS_CACHE_MODE', run.stderr)
            self.assertFalse((root / 'rejected').exists())

    def test_observed_math_repairs_and_math_module_are_assessed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for imported, expression in (
                ('from math import ceil', 'ceil'), ('import math', 'math.ceil'),
            ):
                source = (imported + '\n\ndef shipping_fee(weight_g):\n'
                          '    if weight_g < 0:\n        raise ValueError("negative")\n'
                          '    return 500 + ' + expression + '(max(0, weight_g - 1000) / 1000) * 200\n')
                (root / 'rates.py').write_text(source)
                self.assertEqual(quality_result('singlefile_repair', '', root), ('pass', None))
                (root / 'rates.py').write_text(source.replace(' / 1000', ' / 2000'))
                self.assertEqual(quality_result('singlefile_repair', '', root), ('incorrect', None))
            (root / 'rates.py').write_text('from os import system\ndef shipping_fee(weight_g):\n    return system("false")\n')
            self.assertEqual(quality_result('singlefile_repair', '', root)[0], 'unassessed')
            (root / 'pricing.py').write_text('from math import floor\ndef total(subtotal):\n    return floor(subtotal * 110 / 100)\n')
            (root / 'receipt.py').write_text('from pricing import total\ndef render(subtotal):\n    return f"total={total(subtotal)}"\n')
            self.assertEqual(quality_result('multifile_repair', '', root), ('pass', None))
            (root / 'pricing.py').write_text('from os import system\ndef total(subtotal):\n    return system("false")\n')
            self.assertEqual(quality_result('multifile_repair', '', root)[0], 'unassessed')
            (root / 'pricing.py').write_text('from decimal import Decimal, ROUND_FLOOR\ndef total(subtotal):\n    taxed = Decimal(str(subtotal)) * Decimal("1.10")\n    return int(taxed.to_integral_value(rounding=ROUND_FLOOR))\n')
            self.assertEqual(quality_result('multifile_repair', '', root), ('pass', None))
            (root / 'pricing.py').write_text('from decimal import Decimal, ROUND_FLOOR\ndef total(subtotal):\n    return int(Decimal(str(subtotal)).to_integral_value(rounding=ROUND_FLOOR, context=None))\n')
            self.assertEqual(quality_result('multifile_repair', '', root)[0], 'unassessed')
            (root / 'pricing.py').write_text('from decimal import Decimal, ROUND_FLOOR\ndef total(str):\n    return int(Decimal(str(str)).to_integral_value(rounding=ROUND_FLOOR))\n')
            self.assertEqual(quality_result('multifile_repair', '', root)[0], 'unassessed')
            (root / 'pricing.py').write_text('from decimal import Decimal, ROUND_FLOOR\ndef total(subtotal):\n    return int(Decimal("not-a-number").to_integral_value(rounding=ROUND_FLOOR))\n')
            self.assertEqual(quality_result('multifile_repair', '', root)[0], 'unassessed')

    def test_alternating_runner_aggregates_all_tasks_and_embedding_cost(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / 'fake'
            binary.write_text(FAKE + '\nprint(\'tool-memory-embedding: {"input_tokens":5,"ok":true}\', file=sys.stderr)\n')
            binary.chmod(0o700)
            output = root / 'results'
            run = subprocess.run([
                sys.executable, str(RUNNER.with_name('run_algedi_benchmark.py')),
                '--baseline', str(binary), '--candidate', str(binary),
                '--output', str(output), '--repeats', '3',
            ], capture_output=True, text=True)
            self.assertEqual(run.returncode, 0, run.stderr)
            summary = json.loads((output / 'summary.json').read_text())
            self.assertEqual(len(summary), 10)
            for row in summary:
                self.assertEqual(row['runs'], 3)
                self.assertEqual(row['complete_runs'], 3)
                self.assertEqual(row['quality_passes'], 3)
                self.assertEqual(row['known_total_tokens'], 330)
                self.assertEqual(row['combined_known_tokens'], 345)
                self.assertEqual(row['embedding_missing'], 0)

    def test_five_representative_tasks_have_distinct_contracts(self):
        self.assertEqual(len(TASKS), 5)
        self.assertEqual(set(TASKS), set(PROMPTS))
        self.assertIn('saved_evidence.md', task_files('changed_source_evidence'))

    def test_quality_rejects_wrong_functions_and_comments(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'rates.py').write_text('''# (extra + 999) // 1000
def shipping_fee(weight_g):
    return 500
''')
            self.assertFalse(quality('singlefile_repair', '', root))
            (root / 'rates.py').write_text('''def wrong_name(weight_g):
    return 500
''')
            self.assertFalse(quality('singlefile_repair', '', root))

    def test_quality_checks_numeric_results_instead_of_rejecting_float_syntax(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'rates.py').write_text('''def shipping_fee(weight_g):
    if weight_g < 0: raise ValueError('negative')
    return 500 + ((max(0, weight_g - 1000) + 999) // 1000) * 200
''')
            self.assertTrue(quality('singlefile_repair', '', root))
            (root / 'pricing.py').write_text('''def total(subtotal):
    return subtotal * 110 // 100
''')
            (root / 'receipt.py').write_text('''from pricing import total
def render(subtotal):
    return f"total={total(subtotal)}"
''')
            self.assertTrue(quality('multifile_repair', '', root))
            (root / 'pricing.py').write_text('''def total(subtotal):
    return int(subtotal * 1.1)
''')
            self.assertTrue(quality('multifile_repair', '', root))
            (root / 'pricing.py').write_text('''def total(subtotal):
    return int(subtotal * 1.01)
''')
            self.assertFalse(quality('multifile_repair', '', root))

    def test_unsupported_programs_are_unassessed_without_evaluating_large_values(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cases = (
                "def shipping_fee(weight_g):\n    return 'x' * 999999999999999999999999\n",
                "def shipping_fee(weight_g):\n    return shipping_fee(weight_g)\n",
                "@decorator\ndef shipping_fee(weight_g):\n    return 500\n",
                "def shipping_fee(weight_g=0):\n    return 500\n",
            )
            for source in cases:
                (root / 'rates.py').write_text(source)
                self.assertEqual(quality_result('singlefile_repair', '', root)[0], 'unassessed')
            (root / 'pricing.py').write_text('def total(subtotal):\n    return subtotal * 110 // 100\n')
            for receipt in ('from pricing import total as price\ndef render(subtotal):\n    return f"total={price(subtotal)}"\n',
                            'from .pricing import total\ndef render(subtotal):\n    return f"total={total(subtotal)}"\n'):
                (root / 'receipt.py').write_text(receipt)
                self.assertEqual(quality_result('multifile_repair', '', root)[0], 'unassessed')
            (root / 'receipt.py').write_text('''def render(subtotal):
    return f"total={total(subtotal)}"
''')
            self.assertFalse(quality('multifile_repair', '', root))

    def test_static_singlefile_accepts_observed_divmod_repair(self):
        source = '''def shipping_fee(weight_g):
    if weight_g < 0:
        raise ValueError('negative')
    units, remainder = divmod(max(0, weight_g - 1000), 1000)
    return 500 + (units + (remainder > 0)) * 200
'''
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'rates.py').write_text(source)
            self.assertEqual(quality_result('singlefile_repair', '', root), ('pass', None))
            (root / 'rates.py').write_text(source.replace(', 1000)', ', 2000)'))
            self.assertEqual(quality_result('singlefile_repair', '', root), ('incorrect', None))

    def test_divmod_grammar_rejects_unbounded_forms_and_numeric_errors(self):
        observed = '''def shipping_fee(weight_g):
    units, remainder = divmod(max(0, weight_g - 1000), 1000)
    return 500 + (units + (remainder > 0)) * 200
'''
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for source in (
                'import os\n' + observed,
                observed.replace('units, remainder', 'units, (remainder, extra)'),
                observed.replace('units, remainder =', 'divmod = 1\n    units, remainder ='),
                observed.replace('units, remainder', 'max, remainder'),
            ):
                (root / 'rates.py').write_text(source)
                self.assertEqual(quality_result('singlefile_repair', '', root)[0], 'unassessed')
            program = SafeProgram({'rates': observed.replace(', 1000)', ', 0)')})
            with self.assertRaises(SafeEvaluationError):
                program.call('rates', 'shipping_fee', [1])
            program = SafeProgram({'rates': observed})
            with self.assertRaises(SafeEvaluationError):
                program.call('rates', 'shipping_fee', [2 ** 200])

    def test_offline_all_tasks_and_timeline_source_change(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); binary = root / 'fake'; binary.write_text(FAKE); binary.chmod(0o700)
            output = root / 'results'
            result = subprocess.run([sys.executable, str(RUNNER), '--binary', str(binary), '--output', str(output), '--label', 'offline', '--repeats', '1'], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            rows = [json.loads(line) for line in (output / 'results.jsonl').read_text().splitlines()]
            self.assertTrue(all(row['quality_pass'] and row['scope_pass'] and row['measurement_complete'] for row in rows))
            source = next(row for row in rows if row['task'] == 'changed_source_evidence')
            self.assertEqual(source['timeline'][1], {'sequence': 2, 'event': 'source_changed', 'path': 'current_source.md'})
            audit = [json.loads(line) for line in (output / 'changed_source_evidence-1.audit.jsonl').read_text().splitlines()]
            self.assertEqual(audit[1]['benchmark_timeline']['event'], 'source_changed')
            self.assertEqual(source['resume_measurement'], 'not_measured')

    def test_symlink_scope_fails_before_quality_and_embedding_missing_is_incomplete(self):
        for mode in ('rates-symlink', 'rates-deleted', 'embedding-missing'):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                root = Path(directory); binary = root / 'fake'; binary.write_text(FAKE); binary.chmod(0o700)
                output = root / 'results'
                result = subprocess.run([sys.executable, str(RUNNER), '--binary', str(binary), '--output', str(output), '--label', 'offline', '--tasks', 'singlefile_repair'], capture_output=True, text=True, env=dict(os.environ, ALGEDI_TEST_MODE=mode))
                row = json.loads((output / 'results.jsonl').read_text())
                self.assertNotEqual(result.returncode, 0)
                if mode in ('rates-symlink', 'rates-deleted'):
                    self.assertFalse(row['scope_pass'])
                    self.assertEqual(row['quality_unassessed_reason'], None)
                else:
                    self.assertTrue(row['quality_pass'])
                    self.assertFalse(row['embedding_measurement_complete'])


if __name__ == '__main__':
    unittest.main()
