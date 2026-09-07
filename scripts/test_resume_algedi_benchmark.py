"""resume_algedi_benchmark の限定再開を偽 runner で検証する。"""
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).parent))
import benchmark_algedi as benchmark

RESUME = Path(__file__).with_name('resume_algedi_benchmark.py')
FAKE_RUNNER = '''#!/usr/bin/env python3
import json, sys
from pathlib import Path
args = sys.argv
output = Path(args[args.index('--output') + 1]); label = args[args.index('--label') + 1]
tasks = args[args.index('--tasks') + 1:]
output.mkdir()
(output / 'metadata.json').write_text('{}')
rows = []
for task in tasks:
    rows.append({'task': task, 'repeat': 1, 'exit_code': 0, 'quality_pass': True,
                 'scope_pass': True, 'measurement_complete': True,
                 'comparison_status': 'complete', 'usage': {'input': 1, 'output': 2, 'cache': 0, 'total': 3}})
(output / 'results.jsonl').write_text(''.join(json.dumps(row) + '\\n' for row in rows))
'''


def digest(value):
    return hashlib.sha256(value.encode()).hexdigest()


class ResumeAlgediTests(unittest.TestCase):
    def make_old(self, root, baseline):
        directory = root / 'round-1-baseline-retrieval'; directory.mkdir()
        fixture = directory / 'singlefile_repair-1.fixture'; fixture.mkdir(); (fixture / '.polaris').mkdir()
        for name, body in benchmark.task_files('singlefile_repair').items():
            (fixture / name).write_text(body)
        source = ('from math import ceil\n\n\ndef shipping_fee(weight_g):\n'
                  "    if weight_g < 0:\n        raise ValueError('negative')\n"
                  '    return 500 + ceil(max(0, weight_g - 1000) / 1000) * 200\n')
        (fixture / 'rates.py').write_text(source)
        stderr = 'tokens: in 6 / out 2 / cache 1 / total 8\n使用量の計測: 応答あり 1 / 欠測 0 / 失敗 0\n'
        metrics = {'model': 'gpt-6-astra', 'effort': 'medium', 'outcome': 'completed', 'usage_missing': False,
                   'usage': {'input_tokens': 6, 'output_tokens': 2, 'cache_read_tokens': 1}}
        (directory / 'singlefile_repair-1.err').write_text(stderr)
        (directory / 'singlefile_repair-1.out').write_text('')
        (directory / 'singlefile_repair-1.metrics.jsonl').write_text(json.dumps(metrics) + '\n')
        (directory / 'singlefile_repair-1.audit.jsonl').write_text('')
        row = {'task': 'singlefile_repair', 'repeat': 1, 'exit_code': 0, 'quality_pass': False,
               'quality_assessment': 'unassessed', 'quality_unassessed_reason': 'unsupported_syntax', 'scope_pass': True, 'changed_paths': ['rates.py'],
               'usage': {'input': 6, 'output': 2, 'cache': 1, 'total': 8},
               'embedding_usage': {'requests': 0, 'known_input_tokens': 0, 'missing': 0, 'failed': 0},
               'recorded_model_match': True, 'cli_coverage': {'reported': 1, 'missing': 0, 'failed': 0},
               'metrics_recorded': True, 'measurement_complete': True}
        (directory / 'results.jsonl').write_text(json.dumps(row) + '\n')
        binary_hash = hashlib.sha256(baseline.read_bytes()).hexdigest()
        (directory / 'metadata.json').write_text(json.dumps({'label': 'baseline-retrieval', 'model': 'gpt-6-astra',
            'effort': 'medium', 'tool_memory': 'retrieval', 'baseline_binary': str(baseline),
            'baseline_sha256': binary_hash, 'binary_sha256': binary_hash}))

    def add_stopped_resume(self, root, baseline, candidate):
        original = root / 'round-1-baseline-retrieval'
        old = json.loads((original / 'results.jsonl').read_text())
        old.update(original_quality_pass=False, original_quality_assessment='unassessed',
                   original_quality_unassessed_reason='unsupported_syntax', original_comparison_status=None,
                   quality_pass=True, quality_assessment='pass', quality_unassessed_reason=None,
                   comparison_status='complete', condition='baseline-retrieval',
                   reassessed_quality='pass', reassessment_reason='current_quality_result')
        continuation = root / 'resume-existing'; continuation.mkdir()
        (continuation / 'reassessment.jsonl').write_text(json.dumps(old) + '\n')
        (continuation / 'resume-metadata.json').write_text(json.dumps({
            'baseline': str(baseline.resolve()), 'baseline_sha256': hashlib.sha256(baseline.read_bytes()).hexdigest(),
            'candidate': str(candidate.resolve()), 'candidate_sha256': hashlib.sha256(candidate.read_bytes()).hexdigest(),
            'source_results': str(root.resolve()), 'repeats': 3, 'pending_task_invocations': 29,
        }))
        directory = continuation / 'round-1-baseline-retrieval'; directory.mkdir()
        binary_hash = hashlib.sha256(baseline.read_bytes()).hexdigest()
        (directory / 'metadata.json').write_text(json.dumps({'label': 'baseline-retrieval', 'model': 'gpt-6-astra',
            'effort': 'medium', 'tool_memory': 'retrieval', 'baseline_binary': str(baseline),
            'baseline_sha256': binary_hash, 'binary_sha256': binary_hash}))
        fixture = directory / 'multifile_repair-1.fixture'; fixture.mkdir(); (fixture / '.polaris').mkdir()
        for name, body in benchmark.task_files('multifile_repair').items():
            (fixture / name).write_text(body)
        (fixture / 'pricing.py').write_text('from math import floor\ndef total(subtotal):\n    return floor(subtotal * 110 / 100)\n')
        (fixture / 'receipt.py').write_text('from pricing import total\ndef render(subtotal):\n    return f"total={total(subtotal)}"\n')
        stderr = 'tokens: in 6 / out 2 / cache 1 / total 8\n使用量の計測: 応答あり 1 / 欠測 0 / 失敗 0\n'
        metrics = {'model': 'gpt-6-astra', 'effort': 'medium', 'outcome': 'completed', 'usage_missing': False,
                   'usage': {'input_tokens': 6, 'output_tokens': 2, 'cache_read_tokens': 1}}
        (directory / 'multifile_repair-1.err').write_text(stderr)
        (directory / 'multifile_repair-1.out').write_text('')
        (directory / 'multifile_repair-1.metrics.jsonl').write_text(json.dumps(metrics) + '\n')
        (directory / 'multifile_repair-1.audit.jsonl').write_text('')
        row = {'task': 'multifile_repair', 'repeat': 1, 'exit_code': 0, 'quality_pass': False,
               'quality_assessment': 'unassessed', 'quality_unassessed_reason': 'unsupported_module_statement',
               'scope_pass': True, 'changed_paths': ['pricing.py', 'receipt.py'],
               'usage': {'input': 6, 'output': 2, 'cache': 1, 'total': 8},
               'embedding_usage': {'requests': 0, 'known_input_tokens': 0, 'missing': 0, 'failed': 0},
               'recorded_model_match': True, 'cli_coverage': {'reported': 1, 'missing': 0, 'failed': 0},
               'metrics_recorded': True, 'measurement_complete': True}
        (directory / 'results.jsonl').write_text(json.dumps(row) + '\n')

    def invoke(self, arguments, runner=None):
        spec = importlib.util.spec_from_file_location('resume_algedi_benchmark_tested', RESUME)
        module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
        argv = ['resume_algedi_benchmark.py', *map(str, arguments)]
        with mock.patch.object(sys, 'argv', argv):
            if runner is None:
                module.main()
            else:
                with mock.patch.object(module, 'BENCHMARK', runner):
                    module.main()

    def test_dry_run_regrades_without_writing_or_inference(self):
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary); root = parent / 'results'; root.mkdir(); baseline = parent / 'baseline'; candidate = parent / 'candidate'
            baseline.write_text('baseline'); candidate.write_text('candidate'); self.make_old(root, baseline)
            before = sorted(path.relative_to(root) for path in root.rglob('*'))
            self.invoke(['--source-results', root, '--baseline', baseline, '--candidate', candidate, '--dry-run'])
            self.assertEqual(before, sorted(path.relative_to(root) for path in root.rglob('*')))

    def test_continuation_combines_exactly_three_rows_per_condition_and_task(self):
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary); root = parent / 'results'; root.mkdir(); baseline = parent / 'baseline'; candidate = parent / 'candidate'; runner = parent / 'fake_runner.py'
            baseline.write_text('baseline'); candidate.write_text('candidate'); runner.write_text(FAKE_RUNNER); runner.chmod(0o700)
            self.make_old(root, baseline)
            self.invoke(['--output', root, '--baseline', baseline, '--candidate', candidate], runner)
            all_rows = [json.loads(line) for line in (root / 'all-results.jsonl').read_text().splitlines()]
            self.assertEqual(len(all_rows), 30)
            continuation = next(path for path in root.iterdir() if path.name.startswith('resume-'))
            generated = [path for path in continuation.iterdir() if path.name.startswith('round-')]
            self.assertEqual(len(generated), 6)
            self.assertEqual(sum(len((path / 'results.jsonl').read_text().splitlines()) for path in generated), 29)
            summary = json.loads((root / 'summary.json').read_text())
            self.assertEqual(len(summary), 10)
            self.assertTrue(all(row['runs'] == 3 for row in summary))
            self.assertTrue(all(row['quality_passes'] == 3 for row in summary))
            self.assertTrue((continuation / 'reassessment.jsonl').is_file())
            self.assertTrue((continuation / 'resume-metadata.json').is_file())

    def test_existing_resume_reuses_two_rows_and_runs_only_28_pending(self):
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary); root = parent / 'results'; root.mkdir(); baseline = parent / 'baseline'; candidate = parent / 'candidate'; runner = parent / 'fake_runner.py'
            baseline.write_text('baseline'); candidate.write_text('candidate'); runner.write_text(FAKE_RUNNER); runner.chmod(0o700)
            self.make_old(root, baseline); self.add_stopped_resume(root, baseline, candidate)
            self.invoke(['--source-results', root, '--baseline', baseline, '--candidate', candidate], runner)
            continuation = root / 'resume-existing'
            generated = [path for path in continuation.iterdir() if path.name.startswith('round-')]
            self.assertEqual(len(generated), 7)
            self.assertEqual(sum(len((path / 'results.jsonl').read_text().splitlines()) for path in generated
                                 if path.name != 'round-1-baseline-retrieval'), 28)
            all_rows = [json.loads(line) for line in (root / 'all-results.jsonl').read_text().splitlines()]
            self.assertEqual(len(all_rows), 30)
            self.assertEqual(sum(row['task'] == 'singlefile_repair' and row['condition'] == 'baseline-retrieval' for row in all_rows), 3)
            self.assertEqual(sum(row['task'] == 'multifile_repair' and row['condition'] == 'baseline-retrieval' for row in all_rows), 3)

    def test_completed_prefix_of_seven_schedules_only_twenty_three(self):
        spec = importlib.util.spec_from_file_location('resume_algedi_benchmark_tested', RESUME)
        module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); baseline = root / 'baseline'; candidate = root / 'candidate'; continuation = root / 'resume'
            baseline.write_text('baseline'); candidate.write_text('candidate'); continuation.mkdir()
            (continuation / 'round-1-candidate-retrieval').mkdir()
            reused = [
                {'round': 1, 'condition': 'baseline-retrieval', 'task': task}
                for task in benchmark.TASKS
            ] + [
                {'round': 1, 'condition': 'candidate-retrieval', 'task': task}
                for task in benchmark.TASKS[:2]
            ]
            commands = module.schedule(baseline, candidate, continuation, 600, reused)
            scheduled = [task for command in commands for task in command[command.index('--tasks') + 1:]]
            self.assertEqual(len(scheduled), 23)
            self.assertEqual(scheduled[:3], list(benchmark.TASKS[2:]))
            self.assertIn('round-1-candidate-retrieval-remaining', commands[0][commands[0].index('--output') + 1])
            (continuation / 'round-1-candidate-retrieval-remaining').mkdir()
            with self.assertRaises(SystemExit):
                module.schedule(baseline, candidate, continuation, 600, reused)


if __name__ == '__main__':
    unittest.main()
