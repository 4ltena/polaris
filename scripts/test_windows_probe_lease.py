"""Control-flow tests only: these do not establish Windows volume support."""

import ctypes
import importlib.util
import io
import os
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, patch

import windows_probe_lease as lease


class LeaseTests(unittest.TestCase):
    def test_lock_api_flags_errors_and_unlock(self):
        api = Mock()
        stream = Mock()
        with patch.dict(sys.modules, {"msvcrt": Mock(get_osfhandle=lambda _: 17)}), \
                patch.object(ctypes, "WinDLL", return_value=api, create=True):
            lock = lease._Lock(stream)
        api.LockFileEx.return_value = 1
        self.assertTrue(lock.acquire())
        call = api.LockFileEx.call_args.args
        self.assertEqual(call[0].value, 17)
        self.assertEqual(call[1:5], (3, 0, 1, 0))
        self.assertEqual((lock.overlapped.Offset, lock.overlapped.OffsetHigh), (0, 0))
        api.LockFileEx.return_value = 0
        with patch.object(ctypes, "get_last_error", return_value=33, create=True):
            self.assertFalse(lock.acquire())
        # Sharing/access failures are not successful lock contention.
        for error in (5, 32):
            with self.subTest(error=error), \
                    patch.object(ctypes, "get_last_error", return_value=error, create=True), \
                    patch.object(ctypes, "WinError", return_value=OSError(error, "denied"), create=True):
                with self.assertRaises(OSError):
                    lock.acquire()
        api.UnlockFileEx.return_value = 1
        lock.release()
        self.assertEqual(api.UnlockFileEx.call_args.args[1:4], (0, 1, 0))

    def test_worker_holds_until_release_and_preserves_identity(self):
        events = []
        lock = Mock()
        lock.acquire.side_effect = lambda: events.append("lock") or True
        lock.release.side_effect = lambda: events.append("unlock")
        connection = Mock()
        connection.send.side_effect = lambda value: events.append(value["state"])
        connection.recv.side_effect = lambda: events.append("release command") or "release"
        with patch("builtins.open", return_value=io.BytesIO(lease.LEASE_BYTES)), \
                patch.object(lease, "_identity", return_value=(1, 2)), \
                patch.object(lease, "_Lock", return_value=lock):
            lease._worker("fixture", (1, 2), connection, True, 0)
        self.assertEqual(events, ["lock", "locked", "release command", "unlock", "released"])
        connection.close.assert_called_once()

    def test_worker_rejects_replaced_file_before_lock(self):
        connection = Mock()
        with patch("builtins.open", return_value=io.BytesIO()), \
                patch.object(lease, "_identity", return_value=(1, 3)), \
                patch.object(lease, "_Lock") as lock:
            lease._worker("fixture", (1, 2), connection, False, 0)
        lock.assert_not_called()
        self.assertEqual(connection.send.call_args.args[0]["state"], "error")

    def test_worker_reports_only_real_contention_and_requires_unlock(self):
        for acquired, release_error, expected in (
                (False, None, "blocked"),
                (True, OSError("unlock failed"), "error")):
            with self.subTest(expected=expected):
                connection, lock = Mock(), Mock()
                lock.acquire.return_value = acquired
                lock.release.side_effect = release_error
                with patch("builtins.open", return_value=io.BytesIO()), \
                        patch.object(lease, "_identity", return_value=(1, 2)), \
                        patch.object(lease, "_Lock", return_value=lock):
                    lease._worker("fixture", (1, 2), connection, False, 0)
                self.assertEqual(connection.send.call_args.args[0]["state"], expected)
                self.assertEqual(lock.release.call_count, int(acquired))

    def run_round(self, *, terminate=False, contention="blocked", successor="acquired"):
        owner, contender, fresh = (Mock() for _ in range(3))
        for child in (owner, contender, fresh):
            child.__enter__ = Mock(return_value=child)
            child.__exit__ = Mock(return_value=False)
        owner.message.side_effect = ["locked", "released"]
        owner.process.is_alive.return_value = True
        contender.message.return_value = contention
        fresh.message.return_value = successor
        with patch.object(lease, "_Child", side_effect=[owner, contender, fresh]) as factory:
            result = lease._round(Path("fixture"), (1, 2), terminate=terminate)
        return result, owner, factory

    def test_round_normal_release_and_fresh_process(self):
        result, owner, factory = self.run_round()
        self.assertTrue(result)
        owner.connection.send.assert_called_once_with("release")
        owner.process.terminate.assert_not_called()
        owner.join.assert_called_once_with()
        self.assertEqual(factory.call_count, 3)
        self.assertEqual(factory.call_args.kwargs, {"retry_seconds": 2})

    def test_round_terminates_only_its_owner_before_fresh_attempt(self):
        result, owner, factory = self.run_round(terminate=True)
        self.assertTrue(result)
        owner.process.terminate.assert_called_once_with()
        owner.join.assert_called_once_with(terminated=True)
        owner.connection.send.assert_not_called()
        self.assertEqual(factory.call_count, 3)

    def test_round_fails_if_contender_acquires_or_successor_stays_blocked(self):
        self.assertFalse(self.run_round(contention="acquired")[0])
        self.assertFalse(self.run_round(successor="blocked")[0])

    def test_child_timeout_and_error_are_not_contention(self):
        child = lease._Child.__new__(lease._Child)
        child.connection = Mock()
        child.connection.poll.return_value = False
        with self.assertRaisesRegex(RuntimeError, "timed out"):
            child.message()
        child.connection.poll.assert_called_once_with(lease.WAIT_SECONDS)
        child.connection.poll.return_value = True
        child.connection.recv.return_value = {"state": "error", "winerror": 32}
        with self.assertRaisesRegex(RuntimeError, "did not complete"):
            child.message()

    def test_child_cleanup_reaps_only_started_process(self):
        child = lease._Child.__new__(lease._Child)
        child.connection = Mock()
        child.process = Mock()
        child.process.is_alive.side_effect = [True, False, False]
        child.__exit__(None, None, None)
        child.process.terminate.assert_called_once_with()
        child.process.join.assert_called_once_with(lease.WAIT_SECONDS)
        child.process.close.assert_called_once()
        child.connection.close.assert_called_once()

    def run_probe(self, *, rounds=(True, True), readback=lease.LEASE_BYTES, error=None):
        # Mock only filesystem/child effects; execute the production result logic.
        created, reopened = Mock(), Mock()
        created.__enter__ = Mock(return_value=created)
        created.__exit__ = Mock(return_value=False)
        reopened.__enter__ = Mock(return_value=reopened)
        reopened.__exit__ = Mock(return_value=False)
        reopened.read.return_value = readback
        child = Path("fixture")
        with patch.object(lease.os, "name", "nt"), \
                patch.object(Path, "open", side_effect=[created, reopened]), \
                patch.object(lease.os, "fsync"), \
                patch.object(lease, "_identity", return_value=(1, 2)), \
                patch.object(lease, "_round", side_effect=error or rounds) as run:
            result = lease.probe_lease(child)
        return result, run

    def test_probe_requires_both_rounds_and_unchanged_bytes(self):
        (result, observations), run = self.run_probe()
        self.assertIs(result, True)
        self.assertEqual([call.kwargs["terminate"] for call in run.call_args_list], [False, True])
        self.assertEqual(observations, {"normal_release": True, "owner_termination": True,
                                       "unchanged": True})
        self.assertIs(self.run_probe(rounds=(False,))[0][0], False)
        self.assertIs(self.run_probe(rounds=(True, False))[0][0], False)
        self.assertIs(self.run_probe(readback=b"changed")[0][0], False)
        self.assertIsNone(self.run_probe(error=RuntimeError("timeout"))[0][0])

    def test_main_probe_still_refuses_before_children_on_root_denial(self):
        script = Path(__file__).with_name("probe-windows-filesystem.py")
        spec = importlib.util.spec_from_file_location("filesystem_probe", script)
        probe = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(probe)
        root = Path("fixture")
        with patch.object(os, "name", "nt"), \
                patch.object(Path, "resolve", side_effect=PermissionError("root denied")) as resolve, \
                patch.object(Path, "mkdir") as mkdir, \
                patch.object(probe, "probe_lease") as start, \
                patch.object(probe, "probe_paths") as paths, \
                patch.object(probe, "probe_volume") as volume:
            with self.assertRaises(PermissionError):
                probe.probe(root)
        resolve.assert_called_once_with(strict=True)
        mkdir.assert_not_called()
        start.assert_not_called()
        paths.assert_not_called()
        volume.assert_not_called()
        checks = dict.fromkeys(probe.REQUIRED, True)
        checks["process_termination_recovery"] = None
        self.assertEqual(probe.classify(checks), "C:inconclusive")

    def test_main_probe_records_lease_but_retains_other_unknown_contracts(self):
        script = Path(__file__).with_name("probe-windows-filesystem.py")
        spec = importlib.util.spec_from_file_location("filesystem_probe", script)
        probe = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(probe)
        root = Path("fixture")
        for lease_result, classification in ((True, "C:inconclusive"),
                                             (False, "B:unsafe_or_unsupported"),
                                             (None, "C:inconclusive")):
            stream = Mock()
            stream.__enter__ = Mock(return_value=stream)
            stream.__exit__ = Mock(return_value=False)
            with self.subTest(lease_result=lease_result), \
                    patch.object(os, "name", "nt"), \
                    patch.object(Path, "resolve", return_value=root), \
                    patch.object(Path, "is_dir", return_value=True), \
                    patch.object(Path, "is_symlink", return_value=False), \
                    patch.object(Path, "mkdir"), \
                    patch.object(probe, "volume_info", return_value=("NTFS", 1)), \
                    patch.object(Path, "open", side_effect=[stream, stream, FileExistsError(), stream]), \
                    patch.object(Path, "read_bytes", side_effect=[
                        b"polaris-existing\r\n", b"polaris-staged\r\n", b"polaris-existing\r\n",
                        b"polaris-existing\r\n", b"polaris-staged\r\n"]), \
                    patch.object(Path, "rename", side_effect=FileExistsError()), \
                    patch.object(os, "fsync"), \
                    patch.object(probe, "probe_paths", return_value=(None, {})), \
                    patch.object(probe, "probe_volume", return_value=(None, {})), \
                    patch.object(probe, "probe_lease", return_value=(lease_result, {"fixture": True})) as start:
                result = probe.probe(root)
            start.assert_called_once()
            self.assertEqual(start.call_args.args[0].parent, root)
            self.assertIs(result["checks"]["cross_process_exclusive_lease"], lease_result)
            self.assertEqual(result["observations"]["lease"], {"fixture": True})
            self.assertEqual(result["classification"], classification)
            self.assertEqual({name for name, value in result["checks"].items()
                              if value is None and name != "cross_process_exclusive_lease"}, {
                "protected_private_dacl", "handle_relative_no_reparse",
                "volume_identity_and_cross_volume_refusal", "process_termination_recovery",
            })


if __name__ == "__main__":
    unittest.main()
