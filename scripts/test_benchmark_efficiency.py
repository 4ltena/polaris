"""Offline checks for benchmark isolation and incomplete measurements."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from benchmark_efficiency import PROMPTS, task_prompt, embedding_usage

RUNNER = Path(__file__).with_name('benchmark_efficiency.py')
FAKE = '''#!/usr/bin/env python3
import json, os
from pathlib import Path
argv = __import__('sys').argv
expected = os.environ.get('BENCHMARK_TOOL_MEMORY', 'off')
if expected == 'off':
    assert '--tool-memory' not in argv
else:
    assert argv[argv.index('--tool-memory') + 1] == expected
assert 'POLARIS_CACHE_MODE' not in os.environ
assert 'POLARIS_READ_OUTPUT_BYTES' not in os.environ
root = Path.cwd()
assert (root / '.polaris').is_dir()
assert not (root / 'extra.txt').exists(), 'previous run leaked'
rates = root / 'rates.py'
rates.write_text(rates.read_text().replace('extra // 1000', '(extra + 999) // 1000'))
mode = os.environ.get('BENCHMARK_TEST_MODE', '')
if mode == 'embedding-config':
    assert argv[argv.index('--tool-memory-embedding-url') + 1] == 'http://localhost:8080/v1'
    assert argv[argv.index('--tool-memory-embedding-model') + 1] == 'local-model'
else:
    assert '--tool-memory-embedding-url' not in argv
    assert '--tool-memory-embedding-model' not in argv
if mode.startswith('embedding-'):
    record = {'input_tokens': None if mode == 'embedding-missing' else 7,
              'ok': mode != 'embedding-failed'}
    print('tool-memory-embedding: ' + json.dumps(record), file=__import__('sys').stderr)
if mode == 'scope':
    (root / 'extra.txt').write_text('unexpected')
if mode != 'missing':
    record = {'model': 'gpt-6-astra', 'effort': 'medium', 'outcome': 'completed',
              'usage_missing': False, 'usage': {'input_tokens': 90 if mode == 'mismatch' else 100,
              'output_tokens': 10, 'cache_read_tokens': 20}}
    Path(os.environ['POLARIS_METRICS_PATH']).write_text(json.dumps(record) + '\\n')
print('tokens: in 100 / out 10 / cache 20 / total 110', file=__import__('sys').stderr)
print('使用量の計測: 応答あり 1 / 欠測 0 / 失敗 ' + ('1' if mode == 'lost-failure' else '0'), file=__import__('sys').stderr)
if mode == 'write-warning':
    print('Polaris: 診断ログの保存に失敗しました。', file=__import__('sys').stderr)
'''


class BenchmarkTests(unittest.TestCase):
    def test_embedding_settings_passthrough(self):
        result, rows = self.run_case('embedding-config', tool_memory='retrieval')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(all(row['combined_known_tokens'] == 117 for row in rows))

    def test_embedding_settings_invalid_before_measurement(self):
        for extra in (['--tool-memory-embedding-url', 'http://localhost'],
                      ['--tool-memory-embedding-model', 'model'],
                      ['--tool-memory-embedding-url', 'http://localhost', '--tool-memory-embedding-model', 'model']):
            with tempfile.TemporaryDirectory() as directory:
                output = Path(directory) / 'not-created'
                result = subprocess.run([sys.executable, str(RUNNER), '--binary', str(RUNNER),
                    '--output', str(output), '--label', 'invalid', *extra], capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(output.exists())

    def test_embedding_accounting(self):
        self.assertEqual(embedding_usage(''), {'requests': 0, 'known_input_tokens': 0, 'missing': 0, 'failed': 0})
        records = '\n'.join('tool-memory-embedding: ' + line for line in (
            '{"input_tokens":9,"ok":true}', '{"input_tokens":null,"ok":false}',
            '{"input_tokens":true,"ok":true}', 'invalid'))
        self.assertEqual(embedding_usage(records),
                         {'requests': 4, 'known_input_tokens': 9, 'missing': 3, 'failed': 2})
        for mode in ('embedding-known', 'embedding-missing', 'embedding-failed'):
            result, rows = self.run_case(mode)
            row = rows[0]
            self.assertEqual(row['embedding_usage']['requests'], 1)
            self.assertEqual(row['combined_known_tokens'], 110 if mode == 'embedding-missing' else 117)
            self.assertEqual(row['measurement_complete'], mode == 'embedding-known')
            self.assertEqual(result.returncode == 0, mode == 'embedding-known')

    def test_strategies_preserve_task_and_isolate_search_guidance(self):
        for task in PROMPTS:
            self.assertEqual(task_prompt(task, 'default'), PROMPTS[task])
            self.assertTrue(task_prompt(task, 'scoped').startswith(PROMPTS[task]))
        for task in ('overview', 'repair'):
            self.assertEqual(task_prompt(task, 'scoped'), task_prompt(task, 'search-first'))
        self.assertTrue(task_prompt('investigate', 'search-first').startswith(task_prompt('investigate', 'scoped')))
        self.assertNotIn('D002', task_prompt('investigate', 'search-first'))

    def run_case(self, mode='', legacy=False, tool_memory='off'):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / '.git').mkdir()
            binary = root / 'fake'
            binary.write_text(FAKE)
            binary.chmod(0o700)
            output = root / 'results'
            command = [sys.executable, str(RUNNER), '--binary', str(binary),
                       '--output', str(output), '--label', 'offline-test',
                       '--tasks', 'repair', '--repeats', '2']
            command.extend(['--tool-memory', tool_memory])
            if mode == 'embedding-config':
                command.extend(['--tool-memory-embedding-url', 'http://localhost:8080/v1',
                                '--tool-memory-embedding-model', 'local-model'])
            if legacy:
                command.append('--allow-legacy-metrics')
            result = subprocess.run(command, capture_output=True, text=True,
                                    env=dict(os.environ, BENCHMARK_TEST_MODE=mode,
                                             BENCHMARK_TOOL_MEMORY=tool_memory,
                                             POLARIS_CACHE_MODE='explicit', POLARIS_READ_OUTPUT_BYTES='8192'))
            rows = [json.loads(line) for line in (output / 'results.jsonl').read_text().splitlines()]
            self.assertTrue((output / 'repair-1.fixture' / '.polaris').is_dir())
            return result, rows

    def test_tool_memory_modes_reach_command_and_preserve_environment_cleanup(self):
        for mode in ('off', 'history', 'retrieval'):
            with self.subTest(mode=mode):
                result, rows = self.run_case(tool_memory=mode)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertTrue(all(row['measurement_complete'] for row in rows))

    def test_complete_measurement_and_pristine_repeats(self):
        result, rows = self.run_case()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(rows), 2)
        self.assertTrue(all(r['quality_pass'] and r['measurement_complete'] for r in rows))
        self.assertTrue(all(not r['cost_comparison_ready'] for r in rows))

    def test_scope_violation_fails_quality_and_does_not_leak(self):
        result, rows = self.run_case('scope')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(rows), 2)
        self.assertTrue(all(not r['quality_pass'] and not r['scope_pass'] for r in rows))

    def test_missing_and_mismatched_metrics_stop(self):
        for mode in ('missing', 'mismatch', 'lost-failure', 'write-warning'):
            with self.subTest(mode=mode):
                result, rows = self.run_case(mode)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(len(rows), 1)
                self.assertFalse(rows[0]['measurement_complete'])

    def test_legacy_is_explicit_and_remains_incomplete(self):
        result, rows = self.run_case('missing', legacy=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(rows), 2)
        self.assertTrue(all(not r['measurement_complete'] for r in rows))


if __name__ == '__main__':
    unittest.main()
