"""Mocked volume probe checks; no native or cross-volume acceptance evidence."""

from contextlib import ExitStack
import ctypes as c
import importlib.util
import os
from pathlib import Path
import unittest
from unittest.mock import Mock, patch

import windows_probe_volume as volume


def pin(handle, serial=1, *, directory=True):
    return Mock(handle=handle, identity=(serial, bytes([handle])), directory=directory,
                unchanged=Mock(return_value=True))


class VolumeTests(unittest.TestCase):
    def test_native_rename_has_no_replace_and_a_relative_destination(self):
        kernel, nt = Mock(), Mock()
        with patch.object(c, "WinDLL", side_effect=[kernel, nt], create=True):
            api = volume._VolumeNative()
        self.assertEqual(nt.NtSetInformationFile.argtypes,
                         [c.c_void_p, c.POINTER(volume._IoStatus), c.c_void_p, c.c_uint32, c.c_int])
        self.assertIs(nt.NtSetInformationFile.restype, c.c_int32)
        pointer_size = c.sizeof(c.c_void_p)
        self.assertEqual(volume._RenameInfo.RootDirectory.offset, pointer_size)
        self.assertEqual(volume._RenameInfo.FileNameLength.offset, pointer_size * 2)
        self.assertEqual(volume._RenameInfo.FileName.offset, pointer_size * 2 + 4)
        self.assertEqual(c.sizeof(volume._IoStatus), pointer_size * 2)
        expected_name = ""
        def renamed(source, status, buffer, size, kind):
            header = volume._RenameInfo.from_buffer(buffer)
            self.assertEqual((source, kind, header.ReplaceIfExists, header.Flags,
                              header.RootDirectory), (17, 10, 0, 0, 23))
            self.assertEqual(c.addressof(buffer) % c.alignment(volume._RenameInfo), 0)
            start = volume._RenameInfo.FileName.offset
            raw = c.string_at(c.addressof(buffer), size)
            encoded = raw[start:start + header.FileNameLength]
            self.assertEqual(encoded, expected_name.encode("utf-16-le"))
            self.assertEqual(size, c.sizeof(volume._RenameInfo) + len(encoded))
            self.assertEqual(raw[start + len(encoded):], bytes(size - start - len(encoded)))
            c.cast(status, c.POINTER(volume._IoStatus)).contents.Status.Status = 0
            return 0
        nt.NtSetInformationFile.side_effect = renamed
        for expected_name in ("x", "公開😀"):
            api.rename(17, 23, expected_name)
        with self.assertRaises(ValueError):
            api.rename(17, 23, "../escape")
        self.assertEqual(nt.NtSetInformationFile.call_count, 2)
        nt.RtlNtStatusToDosError.assert_not_called()
        kernel.SetFileInformationByHandle.assert_not_called()

    def test_native_rename_converts_errors_and_never_accepts_pending_as_success(self):
        kernel, nt = Mock(), Mock()
        with patch.object(c, "WinDLL", side_effect=[kernel, nt], create=True):
            api = volume._VolumeNative()
        for status, code in ((0xC0000035, 183), (0xC000000D, 87),
                             (0xC0000043, 32), (0x103, 997)):
            with self.subTest(status=status):
                signed = c.c_int32(status).value
                nt.NtSetInformationFile.return_value = signed
                nt.RtlNtStatusToDosError.return_value = code
                error = OSError("native rename rejected")
                error.winerror = code
                with patch.object(c, "WinError", return_value=error, create=True), \
                        self.assertRaises(OSError) as caught:
                    api.rename(17, 23, "published")
                self.assertEqual(caught.exception.winerror, code)
                nt.RtlNtStatusToDosError.assert_called_with(signed)
        kernel.SetFileInformationByHandle.assert_not_called()

    def test_publication_root_allows_internal_write_open_but_never_delete_sharing(self):
        kernel, nt = Mock(), Mock()
        kernel.CreateFileW.return_value = 23
        with patch.object(c, "WinDLL", side_effect=[kernel, nt], create=True):
            api = volume._VolumeNative()
        root = Path("fixture/publication")
        self.assertEqual(api.open_publication_root(root), 23)
        kernel.CreateFileW.assert_called_once_with(str(root), 0x1000A0, 3, None, 3,
                                                  0x02200000, None)
        kernel.CreateFileW.reset_mock()
        self.assertEqual(api.open_root(Path("X:/")), 23)
        kernel.CreateFileW.assert_called_once_with(str(Path("X:/")), 0x100081, 1, None, 3,
                                                  0x02200000, None)

    def test_readback_rewinds_same_handle(self):
        kernel, nt = Mock(), Mock()
        with patch.object(c, "WinDLL", side_effect=[kernel, nt], create=True):
            api = volume._VolumeNative()
        with patch.object(api, "read", return_value=b"data") as read:
            self.assertEqual(api.read_start(17, 5), b"data")
        kernel.SetFilePointerEx.assert_called_once_with(17, 0, None, 0)
        read.assert_called_once_with(17, 5)

    def test_stage_open_requests_delete_without_write_or_delete_sharing(self):
        kernel, nt = Mock(), Mock()
        with patch.object(c, "WinDLL", side_effect=[kernel, nt], create=True):
            api = volume._VolumeNative()
        def opened(handle, access, attributes, status, allocation, attrs,
                   share, disposition, options, ea, ea_size):
            self.assertEqual((access, share, disposition), (0x110081, 1, 1))
            self.assertEqual(options, 0x00200000 | 0x20)
            c.cast(handle, c.POINTER(c.c_void_p))[0] = 23
            return 0
        nt.NtCreateFile.side_effect = opened
        self.assertEqual(api.open_relative(17, "stage", delete_access=True), 23)

    def test_publish_positive_and_cross_volume_refusal(self):
        source, recovery, target, rename = pin(1, directory=False), pin(2), pin(3), Mock()
        volume._publish(source, recovery, target, "new", rename)
        rename.assert_called_once_with(1, 3, "new")
        for foreign in (recovery, target):
            with self.subTest(handle=foreign.handle):
                foreign.identity = (2, b"foreign")
                rename.reset_mock()
                with self.assertRaises(volume.CrossVolumeRefused):
                    volume._publish(source, recovery, target, "new", rename)
                rename.assert_not_called()
                foreign.identity = (1, bytes([foreign.handle]))

    def test_changed_or_wrong_type_handle_refuses_before_mutation(self):
        source, recovery, target, rename = pin(1, directory=False), pin(2), pin(3), Mock()
        source.unchanged.return_value = False
        with self.assertRaisesRegex(RuntimeError, "binding changed"):
            volume._publish(source, recovery, target, "new", rename)
        source.unchanged.return_value = True
        source.directory = True
        with self.assertRaisesRegex(RuntimeError, "handle type"):
            volume._publish(source, recovery, target, "new", rename)
        rename.assert_not_called()

    def test_publish_rejects_identity_change_after_rename(self):
        source, recovery, target, rename = pin(1, directory=False), pin(2), pin(3), Mock()
        target.unchanged.side_effect = [True, False]
        with self.assertRaisesRegex(volume.UnsafeProbe, "pinned identity"):
            volume._publish(source, recovery, target, "new", rename)
        rename.assert_called_once_with(1, 3, "new")

    def test_comparison_traverses_original_components_and_retains_ancestors(self):
        api = Mock()
        api.open_root.return_value = 1
        api.open_relative.side_effect = [2, 3]
        api.info.return_value = ((7, b"directory"), True)
        original = Path("X:/parent/child")
        with patch.object(Path, "resolve", return_value=Path("Y:/resolved")) as resolve:
            with ExitStack() as stack:
                result = volume._comparison_pin(stack, api, original)
                self.assertEqual(result.handle, 3)
                api.close.assert_not_called()
        resolve.assert_called_once_with(strict=True)
        api.open_root.assert_called_once_with(Path("X:/"))
        self.assertEqual([call.args[1] for call in api.open_relative.call_args_list], ["parent", "child"])
        self.assertEqual([call.args[0] for call in api.close.call_args_list], [3, 2, 1])

    def test_comparison_missing_same_volume_or_denied_stays_unknown(self):
        source, recovery, observations = pin(1, directory=False), pin(2), {}
        with patch.object(volume, "_comparison_pin") as opened:
            self.assertIsNone(volume._compare_volume(Mock(), source, recovery, None, observations))
            opened.assert_not_called()
        with patch.object(volume, "_comparison_pin", return_value=pin(9)):
            self.assertIsNone(volume._compare_volume(Mock(), source, recovery, Path("X:/"), {}))
        with patch.object(volume, "_comparison_pin", side_effect=PermissionError("denied")):
            self.assertIsNone(volume._compare_volume(Mock(), source, recovery, Path("X:/"), {}))

    def test_comparison_write_sentinel_catches_broken_guard(self):
        source, recovery, destination = pin(1, directory=False), pin(2), pin(9, 2)
        def broken_guard(source, recovery, destination, name, rename):
            rename(source.handle, destination.handle, name)
        with patch.object(volume, "_comparison_pin", return_value=destination), \
                patch.object(volume, "_publish", side_effect=broken_guard):
            with self.assertRaisesRegex(volume.UnsafeProbe, "comparison root"):
                volume._compare_volume(Mock(), source, recovery, Path("X:/"), {})

    def run_probe(self, *, comparison=True, collision=True, publish=True, changed_bytes=False,
                  rename_error=None):
        api = Mock()
        api.open_publication_root.return_value = 1
        published = False
        def open_relative(parent, name, *, delete_access=False):
            self.assertEqual(parent, 1)
            if name == "stage":
                if published:
                    raise FileNotFoundError("stage was published")
            if name == "published" and not published:
                raise FileNotFoundError("no destination")
            return {"recovery": 2, "occupied": 3, "stage": 4, "published": 5}[name]
        def info(handle):
            return ((1, bytes([4 if handle == 5 else handle])), handle in (1, 2))
        def rename(source, target, name):
            nonlocal published
            self.assertEqual((source, target), (4, 1))
            if rename_error is not None:
                error = OSError("native rename rejected")
                error.winerror = rename_error
                raise error
            if name == "occupied" and collision:
                error = OSError("collision")
                error.winerror = 183
                raise error
            if name == "published" and publish:
                published = True
        def read(handle, _):
            if changed_bytes:
                return b"corrupt"
            if handle == 3:
                return b"polaris-existing-publication\r\n"
            return b"polaris-staged-publication\r\n"
        api.open_relative.side_effect = open_relative
        api.info.side_effect = info
        api.rename.side_effect = rename
        api.read_start.side_effect = read
        stream = Mock()
        stream.__enter__ = Mock(return_value=stream)
        stream.__exit__ = Mock(return_value=False)
        child, compare_path = Path("fixture"), Path("X:/") if comparison else None
        with patch.object(os, "name", "nt"), patch.object(Path, "mkdir"), \
                patch.object(Path, "open", return_value=stream), patch.object(os, "fsync"), \
                patch.object(volume, "_VolumeNative", return_value=api), \
                patch.object(volume, "_comparison_pin", return_value=pin(9, 2)):
            result = volume.probe_volume(child, compare_path)
        return result, api

    def test_probe_positive_contains_real_guard_and_publication_checks(self):
        (result, observations), api = self.run_probe()
        self.assertIs(result, True)
        self.assertTrue(observations["collision_preserved"])
        self.assertTrue(observations["cross_volume_refused"])
        self.assertTrue(observations["same_volume_publication"])
        self.assertEqual([call.args for call in api.rename.call_args_list],
                         [(4, 1, "occupied"), (4, 1, "published")])
        api.open_publication_root.assert_called_once_with(Path("fixture/publication"))
        api.open_root.assert_not_called()

    def test_probe_unexpected_rename_errors_remain_unknown_and_close_all_pins(self):
        for code in (87, 32, 997):
            with self.subTest(code=code):
                (result, observations), api = self.run_probe(rename_error=code)
                self.assertIsNone(result)
                self.assertNotIn("collision_preserved", observations)
                self.assertNotIn("same_volume_publication", observations)
                self.assertEqual([call.args[0] for call in api.close.call_args_list], [4, 3, 2, 1])

    def test_probe_detects_noop_publish_collision_overwrite_and_corrupted_bytes(self):
        for kwargs in ({"publish": False}, {"collision": False}, {"changed_bytes": True}):
            with self.subTest(kwargs=kwargs):
                self.assertIs(self.run_probe(**kwargs)[0][0], False)
        self.assertIsNone(self.run_probe(comparison=False)[0][0])

    def test_main_volume_result_and_comparison_argument_are_wired(self):
        script = Path(__file__).with_name("probe-windows-filesystem.py")
        spec = importlib.util.spec_from_file_location("filesystem_probe", script)
        probe = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(probe)
        root, comparison = Path("fixture"), Path("X:/")
        for value, expected in ((True, "C:inconclusive"), (False, "B:unsafe_or_unsupported"), (None, "C:inconclusive")):
            stream = Mock()
            stream.__enter__ = Mock(return_value=stream)
            stream.__exit__ = Mock(return_value=False)
            with self.subTest(value=value), patch.object(os, "name", "nt"), \
                    patch.object(Path, "resolve", return_value=root), patch.object(Path, "is_dir", return_value=True), \
                    patch.object(Path, "is_symlink", return_value=False), patch.object(Path, "mkdir"), \
                    patch.object(probe, "volume_info", return_value=("NTFS", 1)), \
                    patch.object(Path, "open", side_effect=[stream, stream, FileExistsError(), stream]), \
                    patch.object(Path, "read_bytes", side_effect=[b"polaris-existing\r\n", b"polaris-staged\r\n",
                        b"polaris-existing\r\n", b"polaris-existing\r\n", b"polaris-staged\r\n"]), \
                    patch.object(Path, "rename", side_effect=FileExistsError()), patch.object(os, "fsync"), \
                    patch.object(probe, "probe_lease", return_value=(True, {})), \
                    patch.object(probe, "probe_paths", return_value=(True, {})), \
                    patch.object(probe, "probe_volume", return_value=(value, {"fixture": True})) as start:
                result = probe.probe(root, comparison)
            start.assert_called_once()
            self.assertEqual(start.call_args.args[0].parent, root)
            self.assertEqual(start.call_args.args[1], comparison)
            self.assertIs(result["checks"]["volume_identity_and_cross_volume_refusal"], value)
            self.assertEqual(result["classification"], expected)
            self.assertIsNone(result["checks"]["protected_private_dacl"])
            self.assertIsNone(result["checks"]["process_termination_recovery"])
        with patch("sys.argv", ["probe", "--root", str(root), "--comparison-root", str(comparison)]), \
                patch.object(probe, "probe", return_value={}) as run, patch("builtins.print"):
            self.assertEqual(probe.main(), 0)
        run.assert_called_once_with(root, comparison)


if __name__ == "__main__":
    unittest.main()
