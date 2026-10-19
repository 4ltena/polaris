"""Entry/control-flow checks; no UAC or native filesystem acceptance."""

import importlib.util
import os
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, patch


def load_probe():
    spec = importlib.util.spec_from_file_location(
        "filesystem_probe_entry", Path(__file__).with_name("probe-windows-filesystem.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class EntryTests(unittest.TestCase):
    def test_internal_dispatch_never_runs_collectors(self):
        probe = load_probe()
        with patch.object(sys, "argv", ["probe", "--internal-prepare-path-links", "request.json"]), \
                patch.object(probe, "prepare_path_links", return_value=20) as helper, \
                patch.object(probe, "probe") as ordinary:
            self.assertEqual(probe.main(), 20)
            helper.assert_called_once_with(Path("request.json"))
            ordinary.assert_not_called()

    def test_internal_dispatch_rejects_mixed_ordinary_arguments(self):
        probe = load_probe()
        for option in ("--self-test", "--private-dacl", "--prepare-path-links-with-uac", "--root"):
            args = ["probe", "--internal-prepare-path-links", "request.json", option]
            if option == "--root":
                args.append("fixture")
            with self.subTest(option=option), patch.object(sys, "argv", args), \
                    patch.object(probe, "prepare_path_links") as helper, \
                    patch.object(probe, "probe") as ordinary, patch.object(sys, "stderr", Mock()):
                with self.assertRaises(SystemExit):
                    probe.main()
                helper.assert_not_called()
                ordinary.assert_not_called()

    def test_default_and_explicit_switch_dispatch(self):
        probe = load_probe()
        for options, kwargs in (([], {}), (["--private-dacl"], {"private_dacl": True}),
                                (["--prepare-path-links-with-uac", "--private-dacl"],
                                 {"private_dacl": True, "prepare_path_links_with_uac": True})):
            with self.subTest(options=options), patch.object(sys, "argv", ["probe", "--root", "fixture"] + options), \
                    patch.object(probe, "probe", return_value={}) as ordinary, patch("builtins.print"):
                self.assertEqual(probe.main(), 0)
                ordinary.assert_called_once_with(Path("fixture"), None, **kwargs)

    def test_elevated_ordinary_mode_refuses_before_root_or_fixture_io(self):
        probe = load_probe()
        with patch.object(os, "name", "nt"), \
                patch.object(probe, "require_normal_collector", side_effect=RuntimeError("admin")), \
                patch.object(Path, "resolve") as resolve, patch.object(Path, "mkdir") as mkdir:
            with self.assertRaisesRegex(RuntimeError, "admin"):
                probe.probe(Path("fixture"), prepare_path_links_with_uac=True)
            resolve.assert_not_called()
            mkdir.assert_not_called()

    def test_uac_wiring_preserves_nine_checks_and_recovery_null(self):
        probe = load_probe()
        root, stream = Path("fixture"), Mock()
        stream.__enter__, stream.__exit__ = Mock(return_value=stream), Mock(return_value=False)
        with patch.object(os, "name", "nt"), patch.object(probe, "require_normal_collector") as normal, \
                patch.object(Path, "resolve", return_value=root), patch.object(Path, "is_dir", return_value=True), \
                patch.object(Path, "is_symlink", return_value=False), patch.object(Path, "mkdir"), \
                patch.object(probe, "volume_info", return_value=("NTFS", 1)), \
                patch.object(Path, "open", side_effect=[stream, stream, FileExistsError(), stream]), \
                patch.object(Path, "read_bytes", side_effect=[b"polaris-existing\r\n", b"polaris-staged\r\n",
                    b"polaris-existing\r\n", b"polaris-existing\r\n", b"polaris-staged\r\n"]), \
                patch.object(Path, "rename", side_effect=FileExistsError()), patch.object(os, "fsync"), \
                patch.object(probe, "probe_lease", return_value=(True, {})), \
                patch.object(probe, "probe_volume", return_value=(True, {})), \
                patch.object(probe, "probe_private_dacl", return_value=(True, {})), \
                patch.object(probe, "probe_paths", return_value=(True, {"helper_bound": True})) as paths:
            result = probe.probe(root, private_dacl=True, prepare_path_links_with_uac=True)
        normal.assert_called_once()
        self.assertEqual(paths.call_args.kwargs, {"prepare_links_with_uac": True})
        self.assertEqual(set(result["checks"]), set(probe.REQUIRED))
        self.assertIsNone(result["checks"]["process_termination_recovery"])
        self.assertEqual(result["classification"], "C:inconclusive")
        self.assertTrue(result["observations"]["storage_prerequisites_passed"])


if __name__ == "__main__":
    unittest.main()
