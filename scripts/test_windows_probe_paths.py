"""Mocked path-probe control checks, not native filesystem acceptance."""

import ctypes as c
import importlib.util
import os
from pathlib import Path
import unittest
from unittest.mock import Mock, patch

import windows_probe_paths as paths


class PathTests(unittest.TestCase):
    def native(self):
        kernel, nt = Mock(), Mock()
        with patch.object(c, "WinDLL", side_effect=[kernel, nt], create=True):
            api = paths._Native()
        return api, kernel, nt

    def test_root_open_does_not_follow_reparse_or_allow_delete(self):
        api, kernel, _ = self.native()
        kernel.CreateFileW.return_value = 17
        self.assertEqual(api.open_root(Path("fixture")), 17)
        self.assertEqual(kernel.CreateFileW.call_args.args[1:],
                         (0x100081, 1, None, 3, 0x02200000, None))

    def test_relative_open_has_pinned_parent_and_no_reparse_flag(self):
        api, _, nt = self.native()
        def opened(handle, access, attributes, status, allocation, attrs,
                   share, disposition, options, ea, ea_size):
            value = c.cast(attributes, c.POINTER(paths._ObjectAttributes)).contents
            self.assertEqual(value.Length, c.sizeof(paths._ObjectAttributes))
            self.assertEqual(value.RootDirectory, 17)
            self.assertEqual(value.Attributes, 0x40)
            name = value.ObjectName.contents
            self.assertEqual(c.wstring_at(name.Buffer, name.Length // 2), "payload")
            self.assertEqual((access, share, disposition, options), (0x100081, 1, 1, 0x200020))
            self.assertIsNone(value.SecurityDescriptor)
            c.cast(handle, c.POINTER(c.c_void_p))[0] = 23
            return 0
        nt.NtCreateFile.side_effect = opened
        self.assertEqual(api.open_relative(17, "payload"), 23)

    def test_rejects_invalid_components_before_native_open(self):
        api, _, nt = self.native()
        for name in ("", ".", "..", "a/b", "a\\b", "C:foo", "a:ads", "\\root", "a\0b", "a.", "a ", "*", "?"):
            with self.subTest(name=name), self.assertRaises(ValueError):
                api.open_relative(17, name)
        nt.NtCreateFile.assert_not_called()

    def test_info_uses_full_file_id_and_volume_and_rejects_every_tag(self):
        api, kernel, _ = self.native()
        attributes, tag = 0x10, 0
        def info(handle, kind, output, size):
            self.assertEqual(handle, 17)
            if kind == 9:
                value = c.cast(output, c.POINTER(paths._AttributeTag)).contents
                self.assertEqual(size, c.sizeof(paths._AttributeTag))
                value.FileAttributes, value.ReparseTag = attributes, tag
            elif kind == 18:
                value = c.cast(output, c.POINTER(paths._FileId)).contents
                self.assertEqual(size, c.sizeof(paths._FileId))
                value.VolumeSerialNumber = 0x123456789ABCDEF0
                value.FileId[:] = bytes(range(1, 17))
            else:
                self.fail("wrong information class")
            return 1
        kernel.GetFileInformationByHandleEx.side_effect = info
        self.assertEqual(api.info(17), ((0x123456789ABCDEF0, bytes(range(1, 17))), True))
        attributes = 0x410
        for tag in (0xA000000C, 0xA0000003, 0x80000099, 0):
            with self.subTest(tag=tag), self.assertRaises(paths.ReparseRejected):
                api.info(17)

    def test_pin_closes_reparse_root_and_retains_each_parent(self):
        api = Mock()
        api.info.side_effect = paths.ReparseRejected("root link")
        with self.assertRaises(paths.ReparseRejected):
            paths._Pinned(api, 1, True)
        api.close.assert_called_once_with(1)
        api.reset_mock()
        api.info.side_effect = [((1, b"root"), True), ((1, b"root"), True), ((1, b"file"), False)]
        api.open_relative.return_value = 2
        with paths._Pinned(api, 1, True) as root:
            with root.child("payload", directory=False) as child:
                self.assertEqual(child.identity, (1, b"file"))
                api.close.assert_not_called()
            api.close.assert_called_once_with(2)
        self.assertEqual([call.args[0] for call in api.close.call_args_list], [2, 1])

    def test_changed_parent_refuses_before_opening_child(self):
        api = Mock()
        api.info.side_effect = [((1, b"root"), True), ((2, b"root"), True)]
        with paths._Pinned(api, 1, True) as root:
            with self.assertRaisesRegex(RuntimeError, "binding changed"):
                root.child("payload", directory=False)
        api.open_relative.assert_not_called()

    def test_bounded_read_uses_the_existing_file_handle(self):
        api, kernel, _ = self.native()
        def read(handle, buffer, limit, count, overlapped):
            self.assertEqual((handle, limit, overlapped), (17, 8, None))
            c.memmove(buffer, b"fixture", 7)
            c.cast(count, c.POINTER(c.c_uint32))[0] = 7
            return 1
        kernel.ReadFile.side_effect = read
        self.assertEqual(api.read(17, 8), b"fixture")
        with self.assertRaises(ValueError):
            api.read(17, 4097)

    def run_probe(self, *, symlink_error=None, reparse_refused=True,
                  rename_blocked=True, changed=False, readback=None, read_error=None):
        api = Mock()
        api.open_root.return_value = 1
        api.open_relative.side_effect = [2, 3, 4, 5, 6]
        def info(handle):
            if handle in (5, 6):
                if reparse_refused:
                    raise paths.ReparseRejected("fixture")
                return ((7, b"link"), handle == 5)
            return ((7, bytes([handle if handle != 4 or changed else 3])), handle <= 2)
        api.info.side_effect = info
        def read(handle, limit):
            # Reparse handles have closed; all ordinary ancestors and files remain pinned.
            self.assertEqual([call.args[0] for call in api.close.call_args_list], [5, 6])
            self.assertEqual((handle, limit), (3, len(b"polaris-path-binding\r\n") + 1))
            if read_error is not None:
                raise read_error
            return b"polaris-path-binding\r\n" if readback is None else readback
        api.read.side_effect = read
        stream = Mock()
        stream.__enter__ = Mock(return_value=stream)
        stream.__exit__ = Mock(return_value=False)
        denied = OSError("sharing violation")
        denied.winerror = 32
        child = Path("fixture")
        with patch.object(os, "name", "nt"), patch.object(Path, "mkdir"), \
                patch.object(Path, "open", return_value=stream), patch.object(os, "fsync"), \
                patch.object(os, "symlink", side_effect=symlink_error) as links, \
                patch.object(paths, "_Native", return_value=api) as native, \
                patch.object(Path, "rename", side_effect=denied if rename_blocked else None), \
                patch.object(Path, "read_bytes", side_effect=AssertionError("pathname read")):
            result = paths.probe_paths(child)
        return result, native, api, links

    def test_probe_positive_requires_reopen_links_replacement_and_bytes(self):
        (result, evidence), _, api, links = self.run_probe()
        self.assertIs(result, True)
        self.assertEqual(evidence["volume_serial"], 7)
        self.assertTrue(evidence["bytes_unchanged"])
        self.assertEqual(links.call_count, 2)
        self.assertEqual(sorted(call.args[0] for call in api.close.call_args_list), [1, 2, 3, 4, 5, 6])

    def test_probe_cannot_pass_followed_link_replaced_directory_or_changed_file(self):
        for kwargs in ({"reparse_refused": False}, {"rename_blocked": False},
                       {"changed": True}, {"readback": b"changed"}):
            with self.subTest(kwargs=kwargs):
                self.assertIs(self.run_probe(**kwargs)[0][0], False)

    def test_missing_symlink_permission_leaves_unknown_without_native_retry(self):
        (result, evidence), native, _, links = self.run_probe(symlink_error=PermissionError("denied"))
        self.assertIsNone(result)
        self.assertIn("denied", evidence["error"])
        links.assert_called_once()
        native.assert_not_called()

    def test_readback_error_cannot_retain_an_earlier_success(self):
        (result, evidence), _, _, _ = self.run_probe(read_error=PermissionError("readback denied"))
        self.assertIsNone(result)
        self.assertIn("readback denied", evidence["error"])

    def test_probe_integration_records_path_result_and_keeps_other_contracts_unknown(self):
        script = Path(__file__).with_name("probe-windows-filesystem.py")
        spec = importlib.util.spec_from_file_location("filesystem_probe", script)
        probe = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(probe)
        root = Path("fixture")
        for value, expected in ((True, "C:inconclusive"), (False, "B:unsafe_or_unsupported"), (None, "C:inconclusive")):
            stream = Mock()
            stream.__enter__ = Mock(return_value=stream)
            stream.__exit__ = Mock(return_value=False)
            with self.subTest(value=value), patch.object(os, "name", "nt"), \
                    patch.object(Path, "resolve", return_value=root), \
                    patch.object(Path, "is_dir", return_value=True), \
                    patch.object(Path, "is_symlink", return_value=False), patch.object(Path, "mkdir"), \
                    patch.object(probe, "volume_info", return_value=("NTFS", 1)), \
                    patch.object(Path, "open", side_effect=[stream, stream, FileExistsError(), stream]), \
                    patch.object(Path, "read_bytes", side_effect=[b"polaris-existing\r\n", b"polaris-staged\r\n",
                        b"polaris-existing\r\n", b"polaris-existing\r\n", b"polaris-staged\r\n"]), \
                    patch.object(Path, "rename", side_effect=FileExistsError()), patch.object(os, "fsync"), \
                    patch.object(probe, "probe_lease", return_value=(True, {})), \
                    patch.object(probe, "probe_volume", return_value=(None, {})), \
                    patch.object(probe, "probe_paths", return_value=(value, {"fixture": True})) as start:
                result = probe.probe(root)
            start.assert_called_once()
            self.assertEqual(start.call_args.args[0].parent, root)
            self.assertIs(result["checks"]["handle_relative_no_reparse"], value)
            self.assertEqual(result["observations"]["paths"], {"fixture": True})
            self.assertEqual(result["classification"], expected)
            self.assertTrue(all(result["checks"][name] is None for name in (
                "protected_private_dacl", "volume_identity_and_cross_volume_refusal", "process_termination_recovery")))



class PreparationTests(unittest.TestCase):
    guid = "\\\\?\\Volume{11111111-1111-1111-1111-111111111111}\\"
    child = paths._CASE_ROOT / "storage-probe-20261011-120000-12345678" / (
        "polaris-windows-fs-probe-" + "1" * 32)

    def native(self):
        kernel, nt, security, shell = Mock(), Mock(), Mock(), Mock()
        with patch.object(c, "WinDLL", side_effect=[kernel, nt, security, shell], create=True):
            api = paths._PreparationNative()
        return api, kernel, nt, security, shell

    def test_preparation_masks_leave_strict_masks_unchanged(self):
        api, kernel, nt, _, _ = self.native()
        kernel.CreateFileW.return_value = 17
        api.open_root(Path("fixture"), preparation=True)
        self.assertEqual(kernel.CreateFileW.call_args.args[1:3], (0x1000A0, 3))
        def opened(handle, *args):
            c.cast(handle, c.POINTER(c.c_void_p))[0] = 23
            return 0
        nt.NtCreateFile.side_effect = opened
        api.open_relative(17, "normal", preparation=True)
        self.assertEqual((nt.NtCreateFile.call_args.args[1], nt.NtCreateFile.call_args.args[6]),
                         (0x1000A0, 3))
        with self.assertRaises(ValueError):
            api.open_relative(17, "normal", preparation=True, delete_access=True)
        nt.NtCreateFile.side_effect = lambda handle, *args: opened(handle, *args) or 0x103
        with self.assertRaises(RuntimeError):
            api.open_relative(17, "normal", preparation=True)
        kernel.CloseHandle.assert_called_with(23)

    def test_full_token_check_and_close_before_writes(self):
        api, kernel, _, security, _ = self.native()
        kernel.GetCurrentProcess.return_value = 5
        def token(process, access, output):
            self.assertEqual((process, access), (5, 8))
            c.cast(output, c.POINTER(c.c_void_p))[0] = 7
            return 1
        security.OpenProcessToken.side_effect = token
        def elevation(value):
            def query(handle, kind, output, size, length):
                self.assertEqual((handle.value, kind, size), (7, 18, 4))
                c.cast(output, c.POINTER(c.c_uint32))[0] = value
                c.cast(length, c.POINTER(c.c_uint32))[0] = 4
                return 1
            return query
        with patch.object(paths.sys, "executable", str(paths._PYTHON)):
            security.GetTokenInformation.side_effect = elevation(2)
            api.require_full_token()
            for value in (0, 1, 3):
                with self.subTest(value=value):
                    security.GetTokenInformation.side_effect = elevation(value)
                    with self.assertRaises(RuntimeError):
                        api.require_full_token()
            self.assertEqual(kernel.CloseHandle.call_count, 4)
            kernel.CreateSymbolicLinkW.assert_not_called()

    def test_exact_creation_api_and_guid_only_path(self):
        api, kernel, _, _, _ = self.native()
        self.assertIs(kernel.CreateSymbolicLinkW.restype, c.c_ubyte)
        parent = Mock(handle=9)
        parent.unchanged.return_value = True
        api.absent = Mock()
        api.validate_link = Mock()
        api.final_path = Mock(return_value=self.guid + "fixture")
        kernel.CreateSymbolicLinkW.return_value = 1
        api.create_link(parent, "alias", "normal", True)
        api.create_link(parent, "alias-file", "payload", False)
        self.assertEqual([call.args for call in kernel.CreateSymbolicLinkW.call_args_list],
                         [(self.guid + "fixture\\alias", "normal", 1),
                          (self.guid + "fixture\\alias-file", "payload", 0)])
        with self.assertRaises(ValueError):
            api.create_link(parent, "other", "outside", True)
        api.absent.side_effect = RuntimeError("existing leaf")
        with self.assertRaises(RuntimeError):
            api.create_link(parent, "alias", "normal", True)
        self.assertEqual(kernel.CreateSymbolicLinkW.call_count, 2)
        def final(handle, buffer, length, flags):
            self.assertEqual(flags, 1)
            buffer.value = "D:\\fixture"
            return len(buffer.value)
        kernel.GetFinalPathNameByHandleW.side_effect = final
        with self.assertRaises(RuntimeError):
            paths._PreparationNative.final_path(api, 9)

    def test_raw_relative_symlink_parser_rejects_tag_flags_target_and_bounds(self):
        target = "normal".encode("utf-16-le")
        good = paths.struct.pack("<IHHHHHHI", 0xA000000C, 12 + len(target) * 2,
                                 0, 0, len(target), len(target), len(target), 1) + target * 2
        paths._symlink_target(good, "normal")
        variants = [good[:19], good[:-1]]
        for offset, fmt, value in ((0, "<I", 0xA0000003), (16, "<I", 0),
                                   (8, "<H", 1), (10, "<H", 999)):
            bad = bytearray(good)
            paths.struct.pack_into(fmt, bad, offset, value)
            variants.append(bytes(bad))
        variants.append(good.replace(target, "beyond".encode("utf-16-le")))
        for data in variants:
            with self.subTest(data=data[:20]), self.assertRaises(ValueError):
                paths._symlink_target(data, "normal")

    def test_raw_leaf_tag_type_identity_and_readback(self):
        api, kernel, _, _, _ = self.native()
        parent = Mock(handle=1, identity=(7, b"parent"))
        parent.unchanged.return_value = True
        api.open_relative = Mock(return_value=9)
        target = b"n\0o\0r\0m\0a\0l\0"
        data = paths.struct.pack("<IHHHHHHI", 0xA000000C, 12 + 2 * len(target),
                                 0, 0, len(target), len(target), len(target), 1) + target * 2
        def ioctl(handle, code, inp, insize, output, size, count, overlapped):
            self.assertEqual((handle, code, inp, insize, overlapped), (9, 0x900A8, None, 0, None))
            c.memmove(output, data, len(data))
            c.cast(count, c.POINTER(c.c_uint32))[0] = len(data)
            return 1
        kernel.DeviceIoControl.side_effect = ioctl
        good = ((7, b"link"), 0x410, 0xA000000C)
        api.raw_info = Mock(return_value=good)
        api.validate_link(parent, "alias", "normal", True)
        for bad in (((7, b"link"), 0x10, 0), ((7, b"link"), 0x400, 0xA000000C),
                    ((8, b"link"), 0x410, 0xA000000C)):
            api.raw_info.return_value = bad
            with self.subTest(bad=bad), self.assertRaises(RuntimeError):
                api.validate_link(parent, "alias", "normal", True)
        api.raw_info.side_effect = [good, ((7, b"changed"), 0x410, 0xA000000C)]
        with self.assertRaises(RuntimeError):
            api.validate_link(parent, "alias", "normal", True)
        self.assertEqual(kernel.CloseHandle.call_count, 5)

    def test_full_chain_pins_each_ancestor_and_refuses_each_replacement(self):
        api = Mock()
        api.volume_guid.return_value = self.guid
        api.open_root.return_value = 1
        api.open_relative.side_effect = [2, 3, 4]
        api.info.side_effect = lambda handle: ((7, bytes([handle])), True)
        api.final_path.side_effect = [self.guid + "a", self.guid + "a\\b", self.guid + "a\\b\\c"]
        with paths.ExitStack() as stack:
            pins = paths._pin_chain(api, stack, paths.PureWindowsPath("D:\\a\\b\\c"))
            # Python 3.10 Path removes the GUID root's trailing separator.
            # CreateFileW must receive a directory root, never a volume device.
            self.assertEqual(str(api.open_root.call_args.args[0]), self.guid)
            self.assertEqual(len(pins), 4)
            api.close.assert_not_called()
            for replacement in range(1, 5):
                api.info.side_effect = lambda handle: (
                    (8 if handle == replacement else 7, bytes([handle])), True)
                with self.subTest(replacement=replacement), self.assertRaises(RuntimeError):
                    paths._check_pins(pins)
            api.info.side_effect = lambda handle: ((7, bytes([handle])), True)
            paths._check_pins(pins)
        self.assertEqual([call.args[0] for call in api.close.call_args_list], [4, 3, 2, 1])

    def request(self):
        records = [{"volume": 7, "file_id": "1" * 32, "directory": True},
                   {"volume": 7, "file_id": "2" * 32, "directory": True},
                   {"volume": 7, "file_id": "3" * 32, "directory": False}]
        sources = {label: {"sha256": "a" * 64, "binding": records[-1]}
                   for label in ("python.exe",) + paths._SOURCES}
        return {"version": 1, "mode": paths._MODE, "nonce": "b" * 64, "state": "pending",
                "parent_pid": os.getpid(), "child": str(self.child), "bindings": records,
                "sources": sources, "volume_guid": self.guid}

    def test_request_rejects_nonce_state_source_identity_and_command_expansion(self):
        request = self.request()
        args = (self.child, request["nonce"], request["bindings"], request["sources"], self.guid)
        paths._validate_request(request, *args)
        for key, value in (("nonce", "c" * 64), ("state", "complete"), ("sources", {}),
                           ("bindings", []), ("child", "D:\\outside"), ("command", "cmd")):
            changed = dict(request, **{key: value})
            with self.subTest(key=key), self.assertRaises(ValueError):
                paths._validate_request(changed, *args)

    def provenance(self, request, data):
        return {"mode": paths._MODE, "nonce": request["nonce"], "parent_pid": os.getpid(),
                "helper_pid": os.getpid() + 1, "exit_code": 0,
                "sources": request["sources"], "child": request["child"],
                "volume_guid": request["volume_guid"],
                "request_sha256": paths.hashlib.sha256(data).hexdigest(),
                "evidence_kind": "helper-internal-full-token-check",
                "launched_at": "2026-10-11T00:00:00+00:00", "completed_at": "2026-10-11T00:00:01+00:00"}

    def test_provenance_does_not_accept_success_code_alone_or_drift(self):
        request, data = self.request(), b"request"
        good = self.provenance(request, data)
        paths._validate_provenance(good, request, data)
        for key in good:
            incomplete = dict(good)
            incomplete.pop(key)
            with self.subTest(missing=key), self.assertRaises((ValueError, TypeError)):
                paths._validate_provenance(incomplete, request, data)
        for key, value in (("exit_code", 20), ("helper_pid", os.getpid()), ("nonce", "c" * 64),
                           ("sources", {}), ("request_sha256", "0" * 64),
                           ("completed_at", "2026-10-10T00:00:00+00:00")):
            with self.subTest(drift=key), self.assertRaises(ValueError):
                paths._validate_provenance(dict(good, **{key: value}), request, data)

    def test_helper_validates_before_two_writes_and_non_full_or_partial_returns_nonzero(self):
        request = self.request()
        fixture, normal, payload, reqpin = [Mock(handle=n) for n in (1, 2, 3, 4)]
        pins = [fixture, normal, payload]
        for pin in pins + [reqpin]:
            pin.unchanged.return_value = True
        api = Mock()
        api.volume_guid.return_value = self.guid
        api.contents.side_effect = lambda handle, *args: (
            paths.json.dumps(request).encode() if handle == 4 else paths._PAYLOAD)
        path = Path(self.child / "path-binding" / ("link-request-" + request["nonce"] + ".json"))
        with patch.object(paths.os, "name", "nt"), patch.object(paths, "_PreparationNative", return_value=api), \
                patch.object(paths, "_pin_chain", return_value=[reqpin]), \
                patch.object(paths, "_source_pins", return_value=([], request["sources"])), \
                patch.object(paths, "_fixture_pins", return_value=(
                    pins, fixture, normal, payload, request["bindings"])):
            self.assertEqual(paths.prepare_path_links(path), 0)
            self.assertEqual([call.args[1:] for call in api.create_link.call_args_list],
                             [("alias", "normal", True), ("alias-file", "payload", False)])
            api.reset_mock()
            api.require_full_token.side_effect = RuntimeError("non-full")
            self.assertEqual(paths.prepare_path_links(path), 20)
            api.create_link.assert_not_called()
            api.require_full_token.side_effect = None
            api.create_link.side_effect = [None, OSError("partial")]
            self.assertEqual(paths.prepare_path_links(path), 20)
            self.assertEqual(api.create_link.call_count, 2)
            api.create_link.reset_mock()
            request["nonce"] = "c" * 64
            self.assertEqual(paths.prepare_path_links(path), 20)
            api.create_link.assert_not_called()

    def test_parent_keeps_pins_until_strict_handover_and_refuses_noop_or_canceled_helper(self):
        request = self.request()
        fixture, normal, payload, reqpin = [unittest.mock.MagicMock(handle=n, identity=(7, bytes([n])),
                                                   directory=n in (1, 2)) for n in (1, 2, 3, 4)]
        pins = [fixture, normal, payload]
        for pin in pins + [reqpin]:
            pin.unchanged.return_value = True
        fixture.child.return_value = reqpin
        reqpin.__enter__.return_value = reqpin
        api, stream = Mock(), Mock()
        written = []
        stream.write.side_effect = written.append
        context = unittest.mock.MagicMock()
        context.__enter__.return_value = stream
        api.contents.side_effect = lambda *args: written[-1]
        api.volume_guid.return_value = self.guid
        def launch(path, provenance):
            req = paths.json.loads(written[-1])
            provenance.update(self.provenance(req, written[-1]))
        api.launch.side_effect = launch
        def fixture_pins(*args, **kwargs):
            if kwargs.get("preparation") is False:
                for pin in pins + [reqpin]:
                    self.assertGreaterEqual(pin.unchanged.call_count, 2)
                    pin.api.close.assert_not_called()
            return pins, fixture, normal, payload, request["bindings"]
        with patch.object(paths.os, "name", "nt"), patch.object(paths.os, "fsync"), \
                patch.object(Path, "mkdir"), patch.object(Path, "open", return_value=context), \
                patch.object(paths.secrets, "token_hex", return_value=request["nonce"]), \
                patch.object(paths, "_PreparationNative", return_value=api), \
                patch.object(paths, "_fixture_pins", side_effect=fixture_pins), \
                patch.object(paths, "_source_pins", return_value=([], request["sources"])), \
                patch.object(paths, "_verify_fixture", return_value=True) as verify:
            self.assertTrue(paths._probe_paths_with_uac(self.child)[0])
            self.assertEqual(api.validate_link.call_count, 2)
            verify.assert_called_once()
            for failure in (None, OSError("UAC canceled"), RuntimeError("partial helper")):
                verify.reset_mock()
                api.launch.side_effect = failure
                api.launch.return_value = None
                self.assertIsNone(paths._probe_paths_with_uac(self.child)[0])
                verify.assert_not_called()

    def test_launch_uses_fixed_quoted_command_and_retains_process_until_exit(self):
        api, kernel, _, _, shell = self.native()
        kernel.GetProcessId.return_value = os.getpid() + 1
        kernel.WaitForSingleObject.return_value = 0
        def launched(info):
            value = c.cast(info, c.POINTER(paths._ShellExecuteInfo)).contents
            self.assertEqual(value.lpVerb, "runas")
            self.assertEqual(value.lpFile, str(paths._PYTHON))
            self.assertEqual(value.fMask & 0x40, 0x40)
            self.assertIn("--internal-prepare-path-links", value.lpParameters)
            self.assertIn('"' + str(paths._PROJECT / "scripts" / paths._SOURCES[0]) + '"',
                          value.lpParameters) if " " in str(paths._PROJECT) else None
            value.hProcess = 77
            return 1
        def exited(handle, code):
            self.assertEqual(handle, 77)
            c.cast(code, c.POINTER(c.c_uint32))[0] = 0
            return 1
        shell.ShellExecuteExW.side_effect = launched
        kernel.GetExitCodeProcess.side_effect = exited
        provenance = {}
        api.launch(Path("request with spaces.json"), provenance)
        self.assertEqual(provenance["exit_code"], 0)
        self.assertEqual(provenance["helper_pid"], os.getpid() + 1)
        kernel.CloseHandle.assert_called_once_with(77)
        kernel.GetProcessId.return_value = 0
        with self.assertRaises(RuntimeError):
            api.launch(Path("request.json"), {})
        self.assertEqual(kernel.WaitForSingleObject.call_count, 2)

if __name__ == "__main__":
    unittest.main()
