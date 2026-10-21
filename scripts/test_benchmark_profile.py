"""Offline contract tests: no credentials, providers or network required."""
import json
import hashlib
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import benchmark_profile as bp
import benchmark_conversation as bc


class ProfileTests(unittest.TestCase):
    def profile(self):
        return bp.Profile("legacy")

    def events(self, profile, calls=1):
        configured = dict(profile.expected(), schema_version=1, event="configured")
        completed = {"schema_version": 1, "event": "completed", "succeeded": True,
                     "decision": {"reported": calls, "missing": 0, "failed": 0,
                                  "input_tokens": 9 * calls, "output_tokens": calls,
                                  "accounting_scope": "completed_logical_decisions",
                                  "physical_attempts_known": False},
                     "coverage": {"reported": 1, "missing": 0, "failed": 0}}
        return "\n".join(bp.PREFIX + json.dumps(e) for e in (configured, completed))

    def test_every_tier_and_resumed_turn_are_explicit(self):
        p = self.profile()
        self.assertEqual(len(p.expected()["tiers"]), 4)
        self.assertIn('[tier.subgiant]\nmodel = "gpt-5.6-terra"\neffort = "medium"', p.toml())
        self.assertIn('--tool-memory', p.polaris_args())
        self.assertIn('retrieval', p.polaris_args())
        self.assertIn('gpt-6-astra', p.codex_args())
        self.assertIn('model_reasoning_effort="medium"', p.codex_args())
        self.assertIn('[files_md]\nauto_regenerate = false', p.toml())
        self.assertIn('initial_phase = "general"', p.toml())
        # The product's defaults must reach the measured run, so the profile
        # says nothing about them.
        self.assertNotIn('always =', p.toml())
        self.assertNotIn('phase_skills', p.toml())

    def test_missing_or_empty_key_stops_before_dispatch(self):
        for env in ({}, {"POLARIS_JEV_API_KEY": "  "}):
            with self.assertRaisesRegex(ValueError, "POLARIS_JEV_API_KEY"):
                self.profile().preflight(env)
        self.profile().preflight({"POLARIS_JEV_API_KEY": "synthetic-only"})

    def test_strict_history_requires_explicit_existing_resource_pins(self):
        with self.assertRaises(ValueError):
            bp.Profile("strict10")
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            runtime, model = root / "python", root / "model"
            runtime.touch()
            model.mkdir()
            pins = {"runtime": str(runtime), "model_path": str(model), "revision": "fixed"}
            p = bp.Profile("strict10", pins)
            self.assertIn('[embedding]', p.toml())
            self.assertIn('revision = "fixed"', p.toml())
            with self.assertRaises(ValueError):
                bp.Profile("strict10", dict(pins, api_key="must-not-serialize"))

    def test_constructed_is_distinct_from_actually_exercised(self):
        p = self.profile()
        self.assertEqual(p.observe(self.events(p))["decision_state"], "observed")
        self.assertEqual(p.observe(self.events(p, 0))["decision_state"], "not_exercised")

    def test_disabled_wrong_model_missing_or_failed_events_are_rejected(self):
        p = self.profile()
        text = self.events(p)
        for broken in ("", text.replace('"active": true', '"active": false'),
                       text.replace('gpt-5.6-terra', 'gpt-6-astra'),
                       text.replace('"failed": 0', '"failed": 1'),
                       text.replace('"missing": 0', '"missing": 1'),
                       text.replace('"succeeded": true', '"succeeded": false'),
                       text + '\n' + bp.PREFIX + '{}'):
            with self.subTest(broken=broken[:60]):
                with self.assertRaises(ValueError):
                    p.observe(broken)

    def test_typed_attempt_scope_requires_explicit_settled_observation_counts(self):
        p = self.profile()
        legacy = self.events(p)
        self.assertEqual(p.observe(legacy)["decision_state"], "observed")
        fresh = legacy.replace('"completed_logical_decisions"', '"settled_typed_adapter_observations"')
        fresh = fresh.replace('"physical_attempts_known": false',
                              '"physical_attempts_known": false, "abandoned_stages": 0, "observer_failures": 0')
        self.assertEqual(p.observe(fresh)["decision_state"], "observed")
        for broken in (fresh.replace('"abandoned_stages": 0', '"abandoned_stages": 1'),
                       fresh.replace('"observer_failures": 0', '"observer_failures": 1'),
                       fresh.replace('"abandoned_stages": 0, ', ''),
                       fresh.replace('"observer_failures": 0', '"observer_failures": false')):
            with self.assertRaises(ValueError):
                p.observe(broken)

    def test_manifest_contains_no_key_and_pins_exact_configuration(self):
        p = self.profile()
        manifest = p.manifest()
        self.assertEqual(len(manifest["config_sha256"]), 64)
        self.assertEqual(manifest["config_toml"], p.toml())
        self.assertNotIn("api_key", json.dumps(manifest))
        self.assertIsNone(manifest["total_usd"])

    def test_driver_preserves_profile_on_resume_and_records_calls(self):
        p = self.profile()
        with tempfile.TemporaryDirectory() as d:
            out = Path(d)
            fake = mock.Mock(stdout="synthetic answer", stderr=self.events(p), returncode=0)
            with mock.patch.dict(bc.os.environ, {"POLARIS_JEV_API_KEY": "fixture"}), \
                    mock.patch.object(bc.subprocess, "run", return_value=fake) as dispatch, \
                    mock.patch.object(bc, "polaris_session_id", return_value="synthetic-session"):
                result = bc.run_polaris(out, out, "sample", ["one", "two"], "read-only", 1, p)
            self.assertEqual(result['failures'], [])
            self.assertEqual(result['turns_sent'], 2)
            self.assertEqual([o['decision_state'] for o in result['observations']], ['observed'] * 2)
            for call in dispatch.call_args_list:
                command = call.args[0]
                for arg in p.polaris_args():
                    self.assertIn(arg, command)
                self.assertEqual(call.kwargs['env']['POLARIS_BENCHMARK_REPORT'], '1')
            self.assertIn('--resume', dispatch.call_args_list[1].args[0])

    def test_bad_effective_settings_stop_the_next_turn(self):
        p = self.profile()
        with tempfile.TemporaryDirectory() as d:
            out = Path(d)
            fake = mock.Mock(stdout="answer", stderr=self.events(p).replace('"active": true', '"active": false'), returncode=0)
            with mock.patch.dict(bc.os.environ, {"POLARIS_JEV_API_KEY": "fixture"}), \
                    mock.patch.object(bc.subprocess, "run", return_value=fake) as dispatch:
                result = bc.run_polaris(out, out, "sample", ["one", "two"], "read-only", 1, p)
            self.assertEqual(dispatch.call_count, 1)
            self.assertEqual(result['failures'][0]['kind'], 'observation_contract')

    def test_empty_key_preflight_calls_nothing_and_preserves_no_claim(self):
        with tempfile.TemporaryDirectory() as d:
            out = Path(d)
            with mock.patch.dict(bc.os.environ, {}, clear=True), \
                    mock.patch.object(bc.subprocess, 'run') as dispatch:
                with self.assertRaises(ValueError):
                    bc.run_polaris(out, out, 'sample', ['one'], 'read-only', 1, self.profile())
                dispatch.assert_not_called()
            self.assertFalse((out / 'sample.claim').exists())

    def test_preparation_requires_no_key_or_dispatch(self):
        with tempfile.TemporaryDirectory() as d:
            args = ['benchmark', '--arm', 'polaris', '--pattern', 'small', '--profile',
                    'jev-routed', '--history-mode', 'legacy', '--prepare-only', '--output', d]
            with mock.patch.object(bc.sys, 'argv', args), \
                    mock.patch.dict(bc.os.environ, {}, clear=True), \
                    mock.patch.object(bc.subprocess, 'run') as dispatch:
                self.assertEqual(bc.main(), 0)
                dispatch.assert_not_called()
            plan = json.loads((Path(d) / 'small-polaris.plan.json').read_text())
            self.assertEqual(plan['profile']['expected'], self.profile().expected())
            self.assertEqual(plan['turns_per_repeat'], 4)

    def test_codex_prior_evidence_is_never_overwritten(self):
        with tempfile.TemporaryDirectory() as d:
            out = Path(d)
            prior = out / 'sample.turn01.jsonl'
            prior.write_text('previous consumption')
            with mock.patch.object(bc.subprocess, 'run') as dispatch:
                with self.assertRaises(FileExistsError):
                    bc.run_codex(out, out, 'sample', ['one'], 'read-only', 1)
                dispatch.assert_not_called()
            self.assertEqual(prior.read_text(), 'previous consumption')

    def codex_rollout(self, path, models, thread='root'):
        events = [{'type': 'session_meta', 'payload': {'id': thread}}]
        usage = {'input_tokens': 10, 'cached_input_tokens': 0, 'cache_write_input_tokens': 0,
                 'output_tokens': 1, 'reasoning_output_tokens': 0}
        for i, (model, effort) in enumerate(models, 1):
            events += [{'type': 'turn_context', 'payload': {'model': model, 'effort': effort}},
                       {'type': 'event_msg', 'payload': {'type': 'token_count', 'info': {
                           'last_token_usage': usage,
                           'total_token_usage': {k: v*i for k, v in usage.items()}}}}]
        path.write_text('\n'.join(json.dumps(e) for e in events))

    def test_codex_effective_root_is_verified_without_restricting_children(self):
        with tempfile.TemporaryDirectory() as d:
            root, child = Path(d)/'root.jsonl', Path(d)/'child.jsonl'
            self.codex_rollout(root, [('gpt-6-astra', 'medium')])
            self.codex_rollout(child, [('gpt-5.6-terra', 'high')], 'child')
            observed = self.profile().observe_codex([root, child], 'root', 0)
            self.assertEqual(observed['observed_requests'], 1)
            for models in ([('gpt-6-astra', 'xhigh')], [('gpt-5.6-terra', 'medium')],
                           [('gpt-6-astra', None)]):
                self.codex_rollout(root, models)
                with self.assertRaises(ValueError):
                    self.profile().observe_codex([root, child], 'root', 0)

    def test_codex_replayed_or_wrong_session_is_not_new_observation(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)/'root.jsonl'
            self.codex_rollout(root, [('gpt-6-astra', 'medium')])
            for thread, previous in [('root', 1), ('another-thread', 0)]:
                with self.assertRaises(ValueError):
                    self.profile().observe_codex([root], thread, previous)
            for paths in ([], [root, root]):
                with self.assertRaises(ValueError):
                    self.profile().observe_codex(paths, 'root', 0)

    def test_wrong_codex_model_after_resume_stops_the_following_turn(self):
        with tempfile.TemporaryDirectory() as d:
            out = Path(d)
            root = out/'root.jsonl'
            calls = []
            def dispatch(argv, **kwargs):
                calls.append(argv)
                kwargs['stdout'].write(b'{"type":"thread.started","thread_id":"root"}\n')
                Path(argv[argv.index('-o')+1]).write_text('answer')
                models = [('gpt-6-astra', 'medium')]
                if len(calls) == 2:
                    models += [('gpt-6-astra', 'xhigh')]
                self.codex_rollout(root, models)
                return mock.Mock(returncode=0, stderr=b'')
            with mock.patch.object(bc.subprocess, 'run', side_effect=dispatch), \
                    mock.patch.object(bc.benchmark_codex, 'wait_for_rollouts', return_value=[root]):
                result = bc.run_codex(out, out, 'sample', ['a','b','c'], 'read-only', 1, self.profile())
            self.assertEqual(len(calls), 2)
            self.assertIn('resume', calls[1])
            for argv in calls:
                for arg in self.profile().codex_args():
                    self.assertIn(arg, argv)
            self.assertEqual(result['failures'][0]['kind'], 'observation_contract')

    def test_live_profile_cannot_implicitly_dispatch_an_old_binary(self):
        with tempfile.TemporaryDirectory() as d:
            args = ['benchmark', '--arm', 'polaris', '--pattern', 'small', '--profile',
                    'jev-routed', '--history-mode', 'legacy', '--output', d]
            with mock.patch.object(bc.sys, 'argv', args), \
                    mock.patch.object(bc.subprocess, 'run') as dispatch, \
                    mock.patch.object(bc.sys, 'stderr'):
                with self.assertRaises(SystemExit) as error:
                    bc.main()
                self.assertEqual(error.exception.code, 2)
                dispatch.assert_not_called()

    def test_binary_pin_rejects_rebuilt_or_missing_artifacts(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d)/'binary'
            path.write_bytes(b'fixture binary')
            digest = hashlib.sha256(path.read_bytes()).hexdigest()
            self.assertEqual(bp.verify_binary(path, digest), digest)
            for pin in (None, 'invalid', '0'*64):
                with self.assertRaises(ValueError):
                    bp.verify_binary(path, pin)
            path.write_bytes(b'rebuilt')
            with self.assertRaises(ValueError):
                bp.verify_binary(path, digest)

    def test_scoring_failure_preserves_dispatch_and_usage_evidence(self):
        with tempfile.TemporaryDirectory() as d:
            out = Path(d).resolve()
            run = {'session':'saved', 'seconds':2, 'turns_sent':4, 'failures':[],
                   'replies':['answer'], 'metrics_path':str(out/'usage.jsonl')}
            with mock.patch.object(bc, 'run_polaris', return_value=run), \
                    mock.patch.object(bc, 'score', side_effect=RuntimeError('synthetic scoring failure')), \
                    mock.patch.object(bc, 'polaris_usage', return_value={'known_tokens': {'input':123}}):
                row = bc.one_run('polaris', 'small', 1, out, 1, 1)
            self.assertEqual(row['turns_sent'], 4)
            self.assertEqual(row['usage']['known_tokens']['input'], 123)
            self.assertEqual(row['failures'][0]['kind'], 'scoring')
            self.assertFalse(row['pass'])


if __name__ == "__main__":
    unittest.main()
