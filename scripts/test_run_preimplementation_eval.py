"""Offline regression checks for the bounded planning-evaluation launcher."""
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import run_preimplementation_eval as runner


def ledger(records):
    return {'budget': {'max_physical_attempts': 96}, 'halted': False, 'records': records}


def record(output=2):
    return {'status': 'succeeded', 'model': 'gpt-6-astra', 'retry_reason': None,
            'usage': {'state': 'known', 'input_tokens': 10, 'output_tokens': output,
                      'total_tokens': 10 + output, 'cached_tokens': 0}}


class LauncherTests(unittest.TestCase):
    def test_plan_balances_48_trials_and_preserves_96_requests(self):
        trials = runner.trial_plan()
        self.assertEqual(len(trials), 48)
        self.assertEqual(len({t['id'] for t in trials}), 48)
        for case in {'PRE%02d' % n for n in range(1, 9)}:
            for arm in runner.ARMS:
                self.assertEqual(sum(t['case'] == case and t['arm'] == arm for t in trials), 3)
        self.assertNotEqual(trials[0]['arm'], trials[16]['arm'])

    def test_ledger_missing_usage_output_excess_and_failed_attempt_stop(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.assertEqual(runner.ledger_status(root), (0, 0.0))
            for bad in (record(4097), {**record(), 'status': 'running'},
                        {**record(), 'usage': {'state': 'missing'}},
                        {**record(), 'retry_reason': '401'}):
                (root / 'attempts.json').write_text(json.dumps(ledger([bad])))
                with self.assertRaises(ValueError):
                    runner.ledger_status(root)
            (root / 'attempts.json').write_text(json.dumps(ledger([record()])))
            self.assertEqual(runner.ledger_status(root)[0], 1)

    def test_cumulative_limits_and_claim_are_not_reset(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'attempts.json').write_text(json.dumps(ledger([record()] * 97)))
            with self.assertRaises(ValueError):
                runner.ledger_status(root)
            expensive = record()
            expensive['usage'].update(input_tokens=4000000, total_tokens=4000002)
            (root / 'attempts.json').write_text(json.dumps(ledger([expensive])))
            with self.assertRaises(ValueError):
                runner.ledger_status(root)
            runner.write_new(root / 'claim.json', {'claimed': True})
            with self.assertRaises(FileExistsError):
                runner.write_new(root / 'claim.json', {'claimed': False})

    def test_changed_input_and_binary_fail_before_send(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            binary = root / 'binary'
            binary.write_bytes(b'not executed')
            frozen = root / 'input.json'
            frozen.write_text('{}')
            contract = {'status': 'approved', 'model': 'gpt-6-astra', 'effort': 'medium',
                        'max_physical_requests': 96, 'trials': runner.trial_plan()}
            (root / 'execution-contract.json').write_text(json.dumps(contract))
            for name in runner.REQUIRED_FILES - {'execution-contract.json'}:
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('{}')
            (root / 'source').mkdir()
            (root / 'source/example.rs').write_text('// source')
            (root / 'source-receipt.json').write_text(json.dumps({'example.rs': runner.digest(root / 'source/example.rs')}))
            frozen_files = {name: runner.digest(root / name) for name in runner.REQUIRED_FILES}
            frozen_files.update({'input.json': runner.digest(frozen),
                                 'source/example.rs': runner.digest(root / 'source/example.rs')})
            (root / 'freeze.json').write_text(json.dumps({'binary_path': str(binary),
                'binary_sha256': runner.digest(binary), 'files': frozen_files}))
            with patch.dict(os.environ, {'POLARIS_AUTH_READ_ONLY': '1'}):
                self.assertEqual(runner.verify(root, binary), contract)
                for required in runner.REQUIRED_FILES:
                    incomplete = dict(frozen_files)
                    del incomplete[required]
                    (root / 'freeze.json').write_text(json.dumps({'binary_path': str(binary),
                        'binary_sha256': runner.digest(binary), 'files': incomplete}))
                    with self.assertRaises(ValueError):
                        runner.verify(root, binary)
                (root / 'freeze.json').write_text(json.dumps({'binary_path': str(binary),
                    'binary_sha256': runner.digest(binary), 'files': frozen_files}))
                frozen.write_text('{"changed":true}')
                with self.assertRaises(ValueError):
                    runner.verify(root, binary)
                frozen.write_text('{}')
                binary.write_bytes(b'changed')
                with self.assertRaises(ValueError):
                    runner.verify(root, binary)

    def test_cannot_skip_incomplete_previous_trial(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with patch.object(runner, 'verify', return_value={'trials': runner.trial_plan()}):
                with patch.object(runner.subprocess, 'run') as child:
                    with self.assertRaises(FileNotFoundError):
                        runner.run_trial(root / 'binary', root, runner.trial_plan()[1]['id'])
                    child.assert_not_called()

    def test_continuation_excludes_prior_trials_and_preserves_known_cost(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            binary = root / 'binary'
            binary.write_bytes(b'never run')
            for name in runner.REQUIRED_FILES:
                p = root / name
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text('{}')
            (root / 'source').mkdir()
            (root / 'source/example.rs').write_text('// fixture')
            (root / 'source-receipt.json').write_text(json.dumps({'example.rs': runner.digest(root / 'source/example.rs')}))
            (root / 'resume-evidence').mkdir()
            known = {**record(), 'parent_id': runner.trial_plan()[0]['id']}
            failed = {**record(), 'parent_id': runner.trial_plan()[1]['id'],
                      'status': 'cancelled', 'usage': {'state': 'missing'}}
            (root / 'resume-evidence/attempts.json').write_text(json.dumps(ledger([known, known, failed])))
            (root / 'resume-evidence/evaluation.stopped.json').write_text(json.dumps({'trial': runner.trial_plan()[1]['id']}))
            for n in (1, 2):
                (root / ('prior.turn-%d.result.json' % n)).write_text(json.dumps({
                    'measurement_complete': True, 'observed_cost_upper_usd': 0.000225,
                    'usage': {'input_tokens':10,'output_tokens':2,'total_tokens':12,'cache_read_tokens':0}}))
            contract = {'status': 'approved', 'model':'gpt-6-astra','effort':'medium',
                        'continuation':'remaining_after_pre01_timeout','max_physical_requests':92,
                        'prior_known_cost_upper_usd':0.00045,'trials':runner.trial_plan()[2:]}
            def freeze():
                (root / 'execution-contract.json').write_text(json.dumps(contract))
                files = {str(p.relative_to(root)):runner.digest(p) for p in root.rglob('*')
                         if p.is_file() and p.name != 'freeze.json'}
                (root / 'freeze.json').write_text(json.dumps({'binary_path':str(binary),
                    'binary_sha256':runner.digest(binary),'files':files}))
            with patch.dict(os.environ, {'POLARIS_AUTH_READ_ONLY':'1'}):
                freeze()
                self.assertEqual(len(runner.verify(root,binary)['trials']),46)
                self.assertEqual(runner.ledger_status(root,92,0.00045),(0,0.00045))
                contract['trials'] = runner.trial_plan()
                freeze()
                with self.assertRaises(ValueError):runner.verify(root,binary)
                contract['trials'] = runner.trial_plan()[2:]
                contract['prior_known_cost_upper_usd'] = 0
                freeze()
                with self.assertRaises(ValueError):runner.verify(root,binary)
                contract['prior_known_cost_upper_usd'] = 0.00045
                failed['usage'] = known['usage']
                (root / 'resume-evidence/attempts.json').write_text(json.dumps(ledger([known,known,failed])))
                freeze()
                with self.assertRaises(ValueError):runner.verify(root,binary)


if __name__ == '__main__':
    unittest.main()
