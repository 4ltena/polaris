"""Offline process-budget tests; these never call a model."""
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('pilot', Path(__file__).with_name('run_sadalmelik_pilot.py'))
pilot = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pilot)


class PilotTests(unittest.TestCase):
    def test_main_sets_readonly_auth_without_a_tool_broker(self):
        with tempfile.TemporaryDirectory() as directory:
            work=Path(directory).resolve()
            def check_environment(binary, actual_work):
                self.assertEqual(actual_work, work)
                self.assertEqual(pilot.os.environ['POLARIS_AUTH_READ_ONLY'], '1')
                self.assertEqual(pilot.os.environ['POLARIS_DATA_DIR'], str(work/'data'))
            with patch.dict('os.environ', {}, clear=False), patch('sys.argv', ['pilot', '--binary', str(work/'binary'), '--work', str(work)]), patch.object(pilot, 'verify', side_effect=check_environment), patch.object(pilot, 'run_batch') as batch:
                pilot.os.environ.pop('POLARIS_AUTH_READ_ONLY', None)
                pilot.main()
                batch.assert_called_once()

    def test_two_arms_once_and_relaunch_cannot_send(self):
        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory)
            calls = []
            def launch(argv, **kwargs):
                calls.append(argv[-1])
                self.assertLessEqual(kwargs['timeout'], 120)
                self.assertEqual(kwargs['env']['POLARIS_AUTH_READ_ONLY'], '1')
                self.assertEqual(kwargs['env']['SSL_CERT_FILE'], '/managed/public-ca.pem')
                (work / (argv[-1] + '.result.json')).write_text(json.dumps({'pass': True, 'physical_sends': 1, 'api_equivalent_usd_interval': [0.01, 0.02]}))
                return subprocess.CompletedProcess(argv, 0, '', '')
            with patch.dict('os.environ', {'POLARIS_AUTH_READ_ONLY': '1', 'SSL_CERT_FILE': '/managed/public-ca.pem'}), patch.object(pilot.subprocess, 'run', side_effect=launch), patch('builtins.print'):
                pilot.run_batch(Path('/fake/driver'), work)
                with self.assertRaises(FileExistsError):
                    pilot.run_batch(Path('/fake/driver'), work)
            self.assertEqual(calls, list(pilot.ARMS))

    def test_failure_or_timeout_prevents_second_arm_and_relaunch(self):
        for failure in [subprocess.CompletedProcess([], 1, '', ''), subprocess.TimeoutExpired('fake', 120)]:
            with self.subTest(failure=type(failure).__name__), tempfile.TemporaryDirectory() as directory:
                work = Path(directory)
                def launch(*args, **kwargs):
                    if isinstance(failure, Exception):
                        raise failure
                    return failure
                with patch.object(pilot.subprocess, 'run', side_effect=launch) as call:
                    with self.assertRaises(ValueError):
                        pilot.run_batch(Path('/fake/driver'), work)
                    with self.assertRaises(FileExistsError):
                        pilot.run_batch(Path('/fake/driver'), work)
                self.assertEqual(call.call_count, 1)
                self.assertTrue((work / 'pilot.stopped.json').exists())

    def test_missing_or_failed_quality_result_never_launches_next_arm(self):
        with tempfile.TemporaryDirectory() as directory:
            work=Path(directory)
            (work / (pilot.ARMS[0]+'.result.json')).write_text(json.dumps({'pass':False,'physical_sends':1}))
            with patch.object(pilot.subprocess,'run',return_value=subprocess.CompletedProcess([],0,'','')) as call:
                with self.assertRaises(ValueError):
                    pilot.run_batch(Path('/fake/driver'),work)
                self.assertEqual(call.call_count,1)

    def test_combined_cost_exceedance_is_not_a_successful_pilot(self):
        with tempfile.TemporaryDirectory() as directory:
            work=Path(directory)
            def launch(argv, **kwargs):
                (work/(argv[-1]+'.result.json')).write_text(json.dumps({'pass':True, 'physical_sends':1, 'api_equivalent_usd_interval':[20,25]}))
                return subprocess.CompletedProcess(argv,0,'','')
            with patch.object(pilot.subprocess,'run',side_effect=launch):
                with self.assertRaises(ValueError):
                    pilot.run_batch(Path('/fake/driver'),work)
            self.assertFalse((work/'summary.json').exists())
            self.assertEqual(json.loads((work/'pilot.stopped.json').read_text())['upper_usd'],50)


if __name__ == '__main__':
    unittest.main()
