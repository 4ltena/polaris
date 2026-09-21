"""Offline M12 launcher tests; the fake binary is never executed."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import run_m12_eval as runner


class CampaignTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.work = Path(self.directory.name).resolve()
        self.binary = self.work / 'driver'
        self.binary.write_text('synthetic binary; never execute', encoding='utf-8')
        self.binary.chmod(0o700)
        cases = []
        answers = {
            'I01': {'project': 'fixture', 'rename_variable': False, 'timeout': 41, 'unit': 'seconds',
                    'production_write': False, 'local_condition': 'offline=true', 'database_password': None},
            'I02': {'occurrences': 8, 'name': 'fixture', 'identifier': '7319'},
            'I03': {'final_limit': 41}, 'I04': {'last_entry_number': 7},
        }
        for case in runner.CASE_IDS:
            source = f'Original input for {case}. ' * 1000
            cases.append({'id': case, 'input': source, 'input_bytes': len(source.encode()),
                          'sha256': runner.hashlib.sha256(source.encode()).hexdigest(),
                          'expected': answers[case],
                          'expected_evidence': {field: {'anchor': f'Original input for {case}.'}
                                                for field in answers[case]},
                          'expected_projection': {'I01': 'applied', 'I02': 'applied',
                              'I03': 'protected_syntax', 'I04': 'insufficient_savings'}[case]})
        (self.work / 'cases.json').write_text(json.dumps({'cases': cases}), encoding='utf-8')
        (self.work / 'rates.json').write_text('{"fixture": "rates"}', encoding='utf-8')
        (self.work / 'm12-model-harness-contract.md').write_text('Synthetic contract\n', encoding='utf-8')
        (self.work / 'run_m12_eval.py').write_bytes(Path(runner.__file__).read_bytes())
        self.env = patch.dict(os.environ, {'POLARIS_AUTH_READ_ONLY': '1'})
        self.env.start()
        self.addCleanup(self.env.stop)

    def prepare(self, fee=1000):
        prepared = runner.prepare(self.work, self.binary, self.work / 'absent-auth.json', fee)
        self.assertFalse((self.work / 'absent-auth.json').exists())
        runner.write_new(self.work / 'm12-authorization.json', {
            'status': 'approved', 'freeze_sha256': prepared['freeze_sha256'],
        })
        return runner.verify(self.work)[0]

    def emit_pair(self, pair, costs=(100, 50), standalone=True):
        prefix = f"pair-{pair['index']:02}"
        output = self.work / prefix
        output.mkdir()
        if standalone:
            runner.write_new(self.work / (prefix + '.claim.json'), runner.claim_for(self.work, pair))
            for suffix in ('.stdout.log', '.stderr.log'):
                (self.work / (prefix + suffix)).write_bytes(b'')
        for name in ('plan.json', 'started.json', 'attempts.json'):
            runner.write_new(output / name, {})
        (output / 'metrics.jsonl').write_text('{}\n', encoding='utf-8')
        (output / 'session').mkdir()
        (output / 'session' / 'raw.jsonl').write_text('original accepted input\n', encoding='utf-8')
        case = next(case for case in json.loads((self.work / 'cases.json').read_text())['cases']
                    if case['id'] == pair['case'])
        for arm, fee in zip(pair['arms'], costs):
            reason = 'off' if arm == 'off' else case['expected_projection']
            user = f"Prepared projection of {pair['case']}" if reason == 'applied' else case['input']
            user_tokens = 50 if reason == 'applied' else 5000
            metrics = {'reason': reason, 'input_bytes': case['input_bytes'],
                       'input_tokens': None if arm == 'off' else 5000,
                       'output_bytes': len(user.encode()) if reason == 'applied' else None,
                       'output_tokens': user_tokens if reason == 'applied' else None,
                       'elapsed_micros': 1}
            if reason == 'insufficient_savings':
                metrics.update(output_bytes=case['input_bytes'] + 100, output_tokens=5100)
            response_name = f"{pair['case']}-{arm}.response.json"
            response_text = json.dumps({'answer': case['expected'], 'evidence': {
                field: value['anchor'] for field, value in case['expected_evidence'].items()}})
            runner.write_new(output / response_name, {'response': {
                'text': response_text, 'text_bytes': len(response_text.encode()),
                'text_sha256': runner.hashlib.sha256(response_text.encode()).hexdigest(),
                'tool_calls': 0, 'hosted_actions': 0, 'url_citations': 0, 'body_retained': True,
            }})
            runner.write_new(output / f"{pair['case']}-{arm}.result.json", {
                'schema_version': 1, 'case': pair['case'], 'mode': arm,
                'observation_complete': True, 'measurement_complete': True, 'physical_sends': 1,
                'raw_original_verified_after_reopen': True,
                'response_file': response_name, 'response_sha256': runner.digest(output / response_name),
                'error': None, 'outcome_error': None, 'accounting_error': None,
                'known_cost_upper_microusd': fee,
                'preparation': {'source_sha256': case['sha256'], 'preparation': metrics},
                'request': {'source': 'actual_provider_request', 'tool_schemas': 0,
                            'endpoint_output_cap_verified': False,
                            'user_sha256': runner.hashlib.sha256(user.encode()).hexdigest(),
                            'user_bytes': len(user.encode()), 'user_tokens': user_tokens,
                            'serialized_bytes': len(user.encode()) + 1000,
                            'system_sha256': runner.hashlib.sha256(b'fixed system').hexdigest(),
                            'serialized_sha256': runner.hashlib.sha256(user.encode()).hexdigest()},
                'accounting': {'source': 'physical_metrics_only', 'lower_cost_microusd': fee,
                               'upper_cost_microusd': fee, 'exact_cost_microusd': fee,
                               'exact_fee_complete': True, 'by_model': {'gpt-6-astra': {
                                   'physical_sends': 1, 'lower_cost_microusd': fee,
                                   'upper_cost_microusd': fee}}},
                'score': {**dict.fromkeys(runner.SCORE_FLAGS, True),
                          'field_answer_correct': dict.fromkeys(case['expected'], True),
                          'field_evidence_correct': dict.fromkeys(case['expected'], True),
                          'raw_output': 'PRIVATE MODEL ANSWER'},
            })
        runner.write_new(output / 'complete.json', {
            'state': 'complete', 'trials': 2, 'physical_sends': 2,
            'observed_cost_upper_microusd': sum(costs),
            'price_table_sha256': runner.digest(self.work / 'rates.json'),
        })
        return output

    def checked_pair(self, pair, costs=(100, 50)):
        output = self.emit_pair(pair, costs)
        checked = runner.check_pair(self.work, pair, output)
        runner.write_new(self.work / f"pair-{pair['index']:02}.checked.json", checked)
        return checked

    def test_fixed_schedule_caps_and_preparation_do_not_authorize_or_read_auth(self):
        self.assertEqual([pair['case'] for pair in runner.pairs()], runner.CASE_IDS)
        self.assertEqual([pair['arms'][0] for pair in runner.pairs()],
                         ['off', 'repeat_refs', 'off', 'repeat_refs'])
        self.assertEqual(len(runner.pairs()) * runner.PAIR_SENDS, 8)
        self.assertEqual(runner.PAIR_TIMEOUT, 260)
        with patch.object(runner.subprocess, 'run') as child:
            prepared = runner.prepare(self.work, self.binary, self.work / 'absent-auth.json', 500)
            self.assertEqual(prepared['status'], 'prepared_not_authorized')
            self.assertFalse((self.work / 'absent-auth.json').exists())
            with self.assertRaises(FileNotFoundError):
                runner.verify(self.work)
            child.assert_not_called()

    def test_fee_and_pair_boundaries_reject_bool_zero_and_overflow(self):
        for fee in (0, -1, True, runner.MAX_FEE + 1):
            with self.subTest(fee=fee), self.assertRaises(ValueError):
                runner.prepare(self.work, self.binary, self.work / 'auth', fee)
        self.prepare(fee=runner.MAX_FEE)
        with patch.object(runner.subprocess, 'run') as child:
            for index in (-1, True, 4):
                with self.subTest(index=index), self.assertRaises(ValueError):
                    runner.run_pair(self.work, index)
            child.assert_not_called()

    def test_authorization_readonly_and_every_frozen_artifact_are_verified(self):
        self.prepare()
        for name in ('driver', 'cases.json', 'rates.json', 'run_m12_eval.py',
                     'm12-model-harness-contract.md', 'm12-contract.json'):
            path = self.work / name
            before = path.read_bytes()
            path.write_bytes(before + b' ')
            with self.subTest(name=name), self.assertRaises(ValueError):
                runner.verify(self.work)
            path.write_bytes(before)
        with patch.dict(os.environ, {'POLARIS_AUTH_READ_ONLY': '0'}), self.assertRaises(ValueError):
            runner.verify(self.work)
        (self.work / 'm12-authorization.json').write_text(
            json.dumps({'status': 'approved', 'freeze_sha256': 'wrong'}), encoding='utf-8')
        with self.assertRaises(ValueError):
            runner.verify(self.work)

    def test_executing_script_and_missing_frozen_entries_are_rejected(self):
        self.prepare()
        alternate = self.work / 'different.py'
        alternate.write_text('not frozen', encoding='utf-8')
        with patch.object(runner, '__file__', str(alternate)), self.assertRaises(ValueError):
            runner.verify(self.work)
        frozen_path = self.work / 'm12-freeze.json'
        frozen = json.loads(frozen_path.read_text())
        del frozen['files']['rates.json']
        frozen_path.write_text(json.dumps(frozen), encoding='utf-8')
        (self.work / 'm12-authorization.json').write_text(json.dumps({
            'status': 'approved', 'freeze_sha256': runner.digest(frozen_path),
        }), encoding='utf-8')
        with self.assertRaises(ValueError):
            runner.verify(self.work)

    def test_input_hash_and_duplicate_case_are_rejected_before_freeze(self):
        path = self.work / 'cases.json'
        before = path.read_text()
        for field, value in (('input', 'changed'), ('input_bytes', True), ('id', 'I02')):
            cases = json.loads(before)
            cases['cases'][0][field] = value
            path.write_text(json.dumps(cases), encoding='utf-8')
            with self.subTest(field=field), self.assertRaises(ValueError):
                runner.prepare(self.work, self.binary, self.work / 'auth', 500)
            self.assertFalse((self.work / 'm12-freeze.json').exists())

    def test_previous_receipt_all_fields_and_nested_evidence_are_recomputed(self):
        contract = self.prepare()
        checked = self.checked_pair(contract['pairs'][0])
        self.assertEqual(runner.checked_totals(self.work, contract, 1), (2, 150))
        path = self.work / 'pair-00.checked.json'
        for field, value in (('observed_cost_upper_microusd', 0), ('physical_sends', 0),
                             ('scores', []), ('measurement_complete', False),
                             ('observation_complete', 1), ('extra', True)):
            changed = dict(checked)
            changed[field] = value
            path.write_text(json.dumps(changed), encoding='utf-8')
            with self.subTest(field=field), self.assertRaises(ValueError):
                runner.checked_totals(self.work, contract, 1)
        path.write_text(json.dumps(checked), encoding='utf-8')
        raw = self.work / 'pair-00/session/raw.jsonl'
        raw.write_text('changed\n', encoding='utf-8')
        with self.assertRaises(ValueError):
            runner.checked_totals(self.work, contract, 1)

    def test_previous_claim_missing_evidence_and_future_claim_stop_before_launch(self):
        contract = self.prepare()
        with patch.object(runner.subprocess, 'run') as child:
            with self.assertRaises(FileNotFoundError):
                runner.run_pair(self.work, 1)
            (self.work / 'pair-02.claim.json').write_text('{}', encoding='utf-8')
            with self.assertRaises(ValueError):
                runner.run_pair(self.work, 0)
            child.assert_not_called()
        (self.work / 'pair-02.claim.json').unlink()
        self.checked_pair(contract['pairs'][0])
        (self.work / 'pair-00.claim.json').write_text('{}', encoding='utf-8')
        with self.assertRaises(ValueError):
            runner.checked_totals(self.work, contract, 1)

    def test_success_passes_remaining_fee_readonly_and_private_logs_then_forbids_retry(self):
        contract = self.prepare()
        self.checked_pair(contract['pairs'][0])

        def finish(command, **kwargs):
            self.assertEqual(command, [str(self.binary), '--work-dir', str(self.work), '--cases',
                str(self.work / 'cases.json'), '--output', str(self.work / 'pair-01'), '--case', 'I02',
                '--live', '--auth-file', str(self.work / 'absent-auth.json'),
                '--fee-cap-microusd', '850'])
            self.assertEqual(kwargs['timeout'], 260)
            self.assertEqual(kwargs['env']['POLARIS_WS'], 'off')
            self.assertEqual(kwargs['env']['POLARIS_AUTH_READ_ONLY'], '1')
            kwargs['stdout'].write(b'PRIVATE DRIVER OUTPUT')
            kwargs['stderr'].write(b'PRIVATE DRIVER ERROR')
            self.emit_pair(contract['pairs'][1], standalone=False)
            return subprocess.CompletedProcess(command, 0)

        with patch.object(runner.subprocess, 'run', side_effect=finish) as child:
            checked = runner.run_pair(self.work, 1)
            self.assertEqual(checked['physical_sends'], 2)
            self.assertEqual([score['arm'] for score in checked['scores']], ['repeat_refs', 'off'])
            self.assertTrue(all(score['all_correct'] for score in checked['scores']))
            self.assertNotIn('PRIVATE', json.dumps(runner.public_result(checked)))
            with self.assertRaises(ValueError):
                runner.run_pair(self.work, 1)
            self.assertEqual(child.call_count, 1)

    def test_timeout_keeps_claim_and_prevents_all_next_sends(self):
        self.prepare()
        with patch.object(runner.subprocess, 'run', side_effect=subprocess.TimeoutExpired('fake', 260)):
            with self.assertRaises(subprocess.TimeoutExpired):
                runner.run_pair(self.work, 0)
        self.assertTrue((self.work / 'pair-00.claim.json').exists())
        self.assertTrue((self.work / 'm12-stopped.json').exists())
        with patch.object(runner.subprocess, 'run') as child:
            for index in (0, 1):
                with self.assertRaises(ValueError):
                    runner.run_pair(self.work, index)
            child.assert_not_called()

    def test_nonzero_driver_exit_marks_stopped_without_checked_receipt(self):
        self.prepare()
        with patch.object(runner.subprocess, 'run', return_value=subprocess.CompletedProcess('fake', 1)):
            with self.assertRaises(ValueError):
                runner.run_pair(self.work, 0)
        self.assertTrue((self.work / 'pair-00.claim.json').exists())
        self.assertTrue((self.work / 'm12-stopped.json').exists())
        self.assertFalse((self.work / 'pair-00.checked.json').exists())

    def test_incomplete_observation_cost_or_extra_send_cannot_become_checked(self):
        contract = self.prepare()
        pair = contract['pairs'][0]
        output = self.emit_pair(pair)
        path = output / 'I01-off.result.json'
        original = json.loads(path.read_text())
        for field, value in (('observation_complete', False), ('physical_sends', 2),
                             ('physical_sends', True), ('measurement_complete', None)):
            report = dict(original)
            report[field] = value
            path.write_text(json.dumps(report), encoding='utf-8')
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                runner.check_pair(self.work, pair, output)
        for fee in (None, -1, True, 101):
            report = dict(original, accounting=dict(original['accounting'], upper_cost_microusd=fee))
            path.write_text(json.dumps(report), encoding='utf-8')
            with self.subTest(fee=fee), self.assertRaises(ValueError):
                runner.check_pair(self.work, pair, output)

    def test_exact_fee_unknown_and_wrong_answer_remain_separate_from_observation(self):
        contract = self.prepare()
        pair = contract['pairs'][0]
        output = self.emit_pair(pair)
        path = output / 'I01-off.result.json'
        report = json.loads(path.read_text())
        report['measurement_complete'] = False
        report['accounting'].update(exact_fee_complete=False, exact_cost_microusd=None)
        report['score'].update(answer_correct=True, evidence_correct=False,
                               quality_complete=False, all_correct=False, raw_output='private answer')
        report['score']['field_evidence_correct']['project'] = False
        path.write_text(json.dumps(report), encoding='utf-8')
        checked = runner.check_pair(self.work, pair, output)
        self.assertTrue(checked['observation_complete'])
        self.assertFalse(checked['measurement_complete'])
        self.assertTrue(checked['scores'][0]['answer_correct'])
        self.assertFalse(checked['scores'][0]['evidence_correct'])
        self.assertFalse(checked['scores'][0]['quality_complete'])
        self.assertFalse(checked['scores'][0]['all_correct'])
        self.assertNotIn('private answer', json.dumps(runner.public_result(checked)))

    def test_cumulative_cap_rejects_overrun_and_stops_at_exact_ceiling(self):
        contract = self.prepare(fee=150)

        def finish(command, **kwargs):
            self.emit_pair(contract['pairs'][0], costs=(101, 50), standalone=False)
            return subprocess.CompletedProcess(command, 0)

        with patch.object(runner.subprocess, 'run', side_effect=finish):
            with self.assertRaises(ValueError):
                runner.run_pair(self.work, 0)
        self.assertTrue((self.work / 'm12-stopped.json').exists())
        self.assertFalse((self.work / 'pair-00.checked.json').exists())

    def test_exact_ceiling_blocks_the_next_pair_before_launch(self):
        contract = self.prepare(fee=150)
        self.checked_pair(contract['pairs'][0])
        with patch.object(runner.subprocess, 'run') as child:
            with self.assertRaises(ValueError):
                runner.run_pair(self.work, 1)
            child.assert_not_called()

    def test_all_fixed_case_field_sets_and_unprojected_controls_are_accepted(self):
        contract = self.prepare()
        for pair in contract['pairs']:
            output = self.emit_pair(pair)
            checked = runner.check_pair(self.work, pair, output)
            self.assertEqual(checked['physical_sends'], 2)
            for score in checked['scores']:
                self.assertEqual(set(score['field_answer_correct']), runner.CASE_FIELDS[pair['case']])
                self.assertEqual(set(score['field_evidence_correct']), runner.CASE_FIELDS[pair['case']])

    def test_result_identity_reopen_and_response_reference_are_required(self):
        pair = self.prepare()['pairs'][0]
        output = self.emit_pair(pair)
        path = output / 'I01-off.result.json'
        original = json.loads(path.read_text())
        for field, value in (
            ('schema_version', 2), ('schema_version', True), ('case', 'I02'),
            ('mode', 'repeat_refs'), ('raw_original_verified_after_reopen', False),
            ('response_file', 'I01-repeat_refs.response.json'),
            ('response_file', '../I01-off.response.json'), ('response_sha256', '0' * 64),
        ):
            changed = dict(original)
            changed[field] = value
            path.write_text(json.dumps(changed), encoding='utf-8')
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                runner.check_pair(self.work, pair, output)
        for field in ('schema_version', 'case', 'mode', 'raw_original_verified_after_reopen',
                      'response_file', 'response_sha256', 'request', 'preparation', 'score'):
            changed = dict(original)
            del changed[field]
            path.write_text(json.dumps(changed), encoding='utf-8')
            with self.subTest(missing=field), self.assertRaises((ValueError, KeyError, TypeError)):
                runner.check_pair(self.work, pair, output)

    def test_response_evidence_must_exist_and_match_its_bytes(self):
        pair = self.prepare()['pairs'][0]
        output = self.emit_pair(pair)
        response = output / 'I01-off.response.json'
        response.write_text('{"response":"substituted"}', encoding='utf-8')
        with self.assertRaises(ValueError):
            runner.check_pair(self.work, pair, output)
        response.unlink()
        with self.assertRaises(FileNotFoundError):
            runner.check_pair(self.work, pair, output)

    def test_score_flags_and_fixed_field_booleans_cannot_be_missing_or_coerced(self):
        pair = self.prepare()['pairs'][0]
        output = self.emit_pair(pair)
        path = output / 'I01-off.result.json'
        original = path.read_text()
        for flag in runner.SCORE_FLAGS:
            for value in (None, 1, 'false'):
                changed = json.loads(original)
                changed['score'][flag] = value
                path.write_text(json.dumps(changed), encoding='utf-8')
                with self.subTest(flag=flag, value=value), self.assertRaises(ValueError):
                    runner.check_pair(self.work, pair, output)
            changed = json.loads(original)
            del changed['score'][flag]
            path.write_text(json.dumps(changed), encoding='utf-8')
            with self.subTest(missing=flag), self.assertRaises(ValueError):
                runner.check_pair(self.work, pair, output)
        for name in ('field_answer_correct', 'field_evidence_correct'):
            for value in (None, {}, {'occurrences': True, 'name': True, 'identifier': True},
                          {**dict.fromkeys(runner.CASE_FIELDS['I01'], True), 'extra': True},
                          {**dict.fromkeys(runner.CASE_FIELDS['I01'], True), 'project': 1}):
                changed = json.loads(original)
                changed['score'][name] = value
                path.write_text(json.dumps(changed), encoding='utf-8')
                with self.subTest(field_map=name, value=value), self.assertRaises(ValueError):
                    runner.check_pair(self.work, pair, output)
        changed = json.loads(original)
        changed['score'] = {**dict.fromkeys(runner.SCORE_FLAGS, False),
                            'field_answer_correct': dict.fromkeys(runner.CASE_FIELDS['I01'], False),
                            'field_evidence_correct': dict.fromkeys(runner.CASE_FIELDS['I01'], False)}
        path.write_text(json.dumps(changed), encoding='utf-8')
        checked = runner.check_pair(self.work, pair, output)
        self.assertFalse(checked['scores'][0]['quality_complete'])

    def test_provider_boundary_source_and_accounting_completeness_are_bound(self):
        pair = self.prepare()['pairs'][0]
        output = self.emit_pair(pair)
        path = output / 'I01-off.result.json'
        original = path.read_text()
        for fields, value in (
            (('preparation', 'source_sha256'), '0' * 64),
            (('preparation', 'preparation', 'input_bytes'), 1),
            (('preparation', 'preparation', 'reason'), 'applied'),
            (('request', 'source'), 'synthetic'), (('request', 'user_sha256'), '0' * 64),
            (('request', 'user_bytes'), 1), (('request', 'user_tokens'), -1),
            (('request', 'serialized_bytes'), 131073), (('request', 'tool_schemas'), 1),
            (('request', 'system_sha256'), 'invalid'),
            (('accounting', 'exact_fee_complete'), False),
            (('accounting', 'exact_fee_complete'), 1),
            (('accounting', 'exact_cost_microusd'), None),
            (('accounting', 'exact_cost_microusd'), 101),
            (('accounting', 'lower_cost_microusd'), 101),
            (('accounting', 'source'), 'synthetic'),
            (('accounting', 'by_model', 'gpt-6-astra', 'physical_sends'), 2),
            (('known_cost_upper_microusd',), 0), (('measurement_complete',), False),
        ):
            changed = json.loads(original)
            target = changed
            for field in fields[:-1]:
                target = target[field]
            target[fields[-1]] = value
            path.write_text(json.dumps(changed), encoding='utf-8')
            with self.subTest(fields=fields, value=value), self.assertRaises(ValueError):
                runner.check_pair(self.work, pair, output)

    def test_applied_projection_matches_provider_bytes_tokens_and_a_distinct_hash(self):
        pair = self.prepare()['pairs'][0]
        output = self.emit_pair(pair)
        path = output / 'I01-repeat_refs.result.json'
        original = path.read_text()
        source_hash = json.loads(original)['preparation']['source_sha256']
        for field, value in (('user_sha256', source_hash), ('user_bytes', 1), ('user_tokens', 1)):
            changed = json.loads(original)
            changed['request'][field] = value
            path.write_text(json.dumps(changed), encoding='utf-8')
            with self.subTest(field=field), self.assertRaises(ValueError):
                runner.check_pair(self.work, pair, output)


if __name__ == '__main__':
    unittest.main()
