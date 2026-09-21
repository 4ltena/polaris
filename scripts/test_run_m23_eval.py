"""No-network tests of M23 campaign bounds, frozen inputs, and crash handling."""
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import run_m23_eval as runner


class CampaignTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.work = Path(self.directory.name).resolve()
        self.binary = self.work / 'driver'
        self.binary.write_text('synthetic, never execute')
        # The driver resolves each fixture's project from the directory holding
        # its source and refuses one without this marker, because `resolve_root`
        # would otherwise walk up and adopt the real repository.
        (self.work / '.polaris').mkdir()
        cases = []
        for number in range(1, 7):
            source = self.work / f'R{number:02}.rs'
            source.write_text(f'const X: usize = {number};')
            cases.append({'id': f'R{number:02}', 'path': source.name,
                          'sha256': runner.digest(source)})
        (self.work / 'cases.json').write_text(json.dumps({'cases': cases}))
        for name in ('rates.json', 'run_m23_eval.py'):
            (self.work / name).write_bytes(Path(__file__).with_name(name).read_bytes())
        self.env = patch.dict(os.environ, {'POLARIS_AUTH_READ_ONLY': '1'})
        self.env.start()
        self.addCleanup(self.env.stop)

    def prepare(self, schedule='full', fee=1000):
        prepared = runner.prepare(self.work, self.binary, self.work / 'unread-auth.json', schedule, fee)
        self.assertFalse((self.work / 'unread-auth.json').exists())
        runner.write_new(self.work / 'm23-authorization.json', {
            'status': 'approved', 'freeze_sha256': prepared['freeze_sha256'],
        })
        return prepared['contract']

    def emit_pair(self, pair, costs=(100, 50)):
        output = self.work / f"pair-{pair['index']:02}"
        output.mkdir()
        for name in ('plan.json', 'started.json', 'attempts.json'):
            runner.write_new(output / name, {})
        (output / 'metrics.jsonl').write_text('{}\n')
        sends = 0
        for arm, fee in zip(pair['arms'], costs):
            count = 2 if arm == 'direct' else 4
            sends += count
            runner.write_new(output / f"{pair['case']}-r{pair['repeat']}-{arm}.result.json", {
                'observation_complete': True, 'measurement_complete': True, 'physical_sends': count,
                'accounting': {'upper_cost_microusd': fee}, 'score': {'correct': True},
            })
        runner.write_new(output / 'complete.json', {
            'state': 'complete', 'trials': 2, 'physical_sends': sends,
            'observed_cost_upper_microusd': sum(costs),
            'price_table_sha256': runner.digest(self.work / 'rates.json'),
        })
        return output

    def test_schedule_is_balanced_and_fits_runner_limits(self):
        full = runner.pairs('full')
        self.assertEqual(len(full), 18)
        self.assertEqual(len(full) * runner.PAIR_SENDS, 270)
        self.assertLess(runner.PAIR_TIMEOUT, 600)
        for number in range(1, 7):
            self.assertEqual([p['repeat'] for p in full if p['case'] == f'R{number:02}'], [1, 2, 3])
        self.assertEqual(full[0]['arms'], full[1]['arms'][::-1])
        self.assertEqual(runner.pairs('pilot')[0]['case'], 'R03')

    def test_preparation_does_not_authorize_or_read_authentication(self):
        runner.prepare(self.work, self.binary, self.work / 'absent-auth.json', 'pilot', 500)
        with self.assertRaises(FileNotFoundError):
            runner.verify(self.work)

    def test_changed_binary_source_rates_and_missing_frozen_entry_are_rejected(self):
        self.prepare()
        runner.verify(self.work)
        for name in ('driver', 'R01.rs', 'rates.json', 'run_m23_eval.py', 'cases.json'):
            path = self.work / name
            original = path.read_bytes()
            path.write_bytes(original + b' ')
            with self.subTest(name=name), self.assertRaises(ValueError):
                runner.verify(self.work)
            path.write_bytes(original)

    def test_executing_launcher_must_match_frozen_work_copy(self):
        self.prepare()
        other = self.work / 'unfrozen-launcher.py'
        other.write_text('changed code')
        with patch.object(runner, '__file__', str(other)), self.assertRaises(ValueError):
            runner.verify(self.work)

    def test_preparation_refuses_a_fixture_without_its_project_marker(self):
        (self.work / '.polaris').rmdir()
        with self.assertRaises(ValueError):
            runner.prepare(self.work, self.binary, self.work / 'auth.json', 'pilot', 500)
        self.assertFalse((self.work / 'm23-contract.json').exists())

    def test_missing_fixture_project_marker_stops_before_the_claim(self):
        self.prepare()
        (self.work / '.polaris').rmdir()
        with patch.object(runner.subprocess, 'run') as child:
            with self.assertRaises(ValueError):
                runner.run_pair(self.work, 0)
            child.assert_not_called()
        # The driver refuses this fixture as well, but only after run_pair has
        # written the one-shot claim, which ends the campaign for a condition
        # that is visible beforehand. Stopping in verify leaves it runnable.
        self.assertFalse((self.work / 'pair-00.claim.json').exists())
        self.assertFalse((self.work / 'm23-stopped.json').exists())
        (self.work / '.polaris').mkdir()
        with patch.object(runner.subprocess, 'run',
                          return_value=type('Result', (), {'returncode': 1})()) as child:
            with self.assertRaises(ValueError):
                runner.run_pair(self.work, 0)
            child.assert_called_once()

    def test_previous_fee_and_count_are_recomputed_and_cannot_reset(self):
        contract = self.prepare()
        output = self.emit_pair(contract['pairs'][0])
        checked = runner.check_pair(self.work, contract['pairs'][0], output)
        runner.write_new(self.work / 'pair-00.checked.json', checked)
        self.assertEqual(runner.checked_totals(self.work, contract, 1), (6, 150))
        changed = dict(checked, observed_cost_upper_microusd=0)
        (self.work / 'pair-00.checked.json').write_text(json.dumps(changed))
        with self.assertRaises(ValueError):
            runner.checked_totals(self.work, contract, 1)
        (self.work / 'pair-00.checked.json').write_text(json.dumps(checked))
        (output / 'metrics.jsonl').write_text('changed\n')
        with self.assertRaises(ValueError):
            runner.checked_totals(self.work, contract, 1)

    def test_incomplete_predecessor_and_future_claim_block_before_subprocess(self):
        self.prepare()
        with patch.object(runner.subprocess, 'run') as child:
            with self.assertRaises(FileNotFoundError):
                runner.run_pair(self.work, 1)
            (self.work / 'pair-02.claim.json').write_text('{}')
            with self.assertRaises(ValueError):
                runner.run_pair(self.work, 0)
            child.assert_not_called()

    def test_success_uses_remaining_fee_then_forbids_replay(self):
        contract = self.prepare()
        first = self.emit_pair(contract['pairs'][0])
        runner.write_new(self.work / 'pair-00.checked.json',
                         runner.check_pair(self.work, contract['pairs'][0], first))

        def finish(command, **kwargs):
            self.assertEqual(command[-2:], ['--fee-cap-microusd', '850'])
            self.assertEqual(kwargs['env']['POLARIS_WS'], 'off')
            self.assertEqual(kwargs['env']['POLARIS_READ_OUTPUT_BYTES'], '24576')
            self.emit_pair(contract['pairs'][1])
            return type('Result', (), {'returncode': 0})()

        with patch.object(runner.subprocess, 'run', side_effect=finish) as child:
            checked = runner.run_pair(self.work, 1)
            self.assertEqual(checked['physical_sends'], 6)
            with self.assertRaises(ValueError):
                runner.run_pair(self.work, 1)
            self.assertEqual(child.call_count, 1)

    def test_failure_and_cumulative_limit_stop_next_pair(self):
        contract = self.prepare(fee=150)
        output = self.emit_pair(contract['pairs'][0])
        runner.write_new(self.work / 'pair-00.checked.json',
                         runner.check_pair(self.work, contract['pairs'][0], output))
        with patch.object(runner.subprocess, 'run') as child:
            with self.assertRaises(ValueError):
                runner.run_pair(self.work, 1)
            child.assert_not_called()

    def test_driver_failure_keeps_claim_and_marks_campaign_stopped(self):
        self.prepare()
        with patch.object(runner.subprocess, 'run', return_value=type('Result', (), {'returncode': 1})()):
            with self.assertRaises(ValueError):
                runner.run_pair(self.work, 0)
        self.assertTrue((self.work / 'pair-00.claim.json').exists())
        self.assertTrue((self.work / 'm23-stopped.json').exists())
        with patch.object(runner.subprocess, 'run') as child:
            with self.assertRaises(ValueError):
                runner.run_pair(self.work, 0)
            child.assert_not_called()

    def test_report_total_and_unknown_or_negative_cost_are_refused(self):
        contract = self.prepare()
        pair = contract['pairs'][0]
        output = self.emit_pair(pair)
        path = output / 'R01-r1-direct.result.json'
        original = json.loads(path.read_text())
        for cost in (None, -1, True, 101):
            record = dict(original, accounting={'upper_cost_microusd': cost})
            path.write_text(json.dumps(record))
            with self.subTest(cost=cost), self.assertRaises(ValueError):
                runner.check_pair(self.work, pair, output)

    def test_missing_exact_fee_keeps_complete_observations_separate(self):
        contract = self.prepare()
        pair = contract['pairs'][0]
        output = self.emit_pair(pair)
        path = output / 'R01-r1-direct.result.json'
        report = json.loads(path.read_text())
        report['measurement_complete'] = False
        path.write_text(json.dumps(report))
        checked = runner.check_pair(self.work, pair, output)
        self.assertFalse(checked['measurement_complete'])
        runner.write_new(self.work / 'pair-00.checked.json', dict(checked, measurement_complete=True))
        with self.assertRaises(ValueError):
            runner.checked_totals(self.work, contract, 1)
        report['observation_complete'] = False
        path.write_text(json.dumps(report))
        with self.assertRaises(ValueError):
            runner.check_pair(self.work, pair, output)


if __name__ == '__main__':
    unittest.main()
