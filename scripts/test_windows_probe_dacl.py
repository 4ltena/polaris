"""Mocked DACL decoding/control checks; no native ACL or token API execution."""

import ctypes as c
import os
from pathlib import Path
import struct
import unittest
from unittest.mock import Mock, patch

import windows_probe_dacl as dacl


# Synthetic fixture identity, never read from the host.
USER = bytes.fromhex("01020000000000051500000039300000")


def ace(sid=USER, *, kind=0, flags=0, mask=dacl.FULL_ACCESS):
    return struct.pack("<BBHI", kind, flags, 8 + len(sid), mask) + sid


def acl(*entries):
    return struct.pack("<BBHHH", 2, 0, 8 + sum(map(len, entries)), len(entries), 0) + b"".join(entries)


GOOD_ACL = acl(*(ace(sid) for sid in (USER, dacl.SYSTEM, dacl.ADMINISTRATORS)))
GOOD = dacl._Security(USER, 0x9004, dacl._acl_entries(GOOD_ACL))


def winerror(code):
    error = OSError("synthetic error with identity text that must not be logged")
    error.winerror = code
    return error


class DaclTests(unittest.TestCase):
    def native(self):
        kernel, nt, security = Mock(), Mock(), Mock()
        with patch.object(c, "WinDLL", side_effect=[kernel, nt, security], create=True):
            api = dacl._DaclNative()
        return api, kernel, nt, security

    def test_exact_private_policy_accepts_actual_decoded_allow_entries(self):
        self.assertTrue(dacl._is_private(GOOD, USER))
        self.assertEqual(len(GOOD.aces), 3)

    def test_rejects_owner_absent_null_unprotected_and_non_exact_grants(self):
        bad = [GOOD._replace(owner=dacl.SYSTEM), GOOD._replace(control=0x9000),
               GOOD._replace(control=0x8004), GOOD._replace(aces=None),
               GOOD._replace(aces=()), GOOD._replace(aces=GOOD.aces[:-1]),
               GOOD._replace(aces=GOOD.aces + (GOOD.aces[0],))]
        for change in (dict(kind=1), dict(kind=9), dict(flags=0x10), dict(flags=8),
                       dict(flags=3), dict(mask=0), dict(mask=dacl.FULL_ACCESS | 0x1000000),
                       dict(sid=bytes.fromhex("010100000000000100000000"))):
            bad.append(GOOD._replace(aces=(GOOD.aces[0]._replace(**change),) + GOOD.aces[1:]))
        for value in bad:
            with self.subTest(value=value):
                self.assertFalse(dacl._is_private(value, USER))

    def test_acl_parser_rejects_unknown_type_lengths_sid_and_hidden_entries(self):
        for value in (b"", GOOD_ACL[:7], GOOD_ACL[:-1],
                      acl(ace(kind=9)), acl(ace(sid=b"\x01\x0f" + bytes(6))),
                      acl(ace(sid=USER + bytes(4))),
                      struct.pack("<BBHHH", 2, 0, 12, 0, 0) + b"oops"):
            with self.subTest(value=value), self.assertRaises(dacl._UnsafeDescriptor):
                dacl._acl_entries(value)
        for offset, replacement in ((0, 3), (1, 1), (4, 65), (6, 1), (10, 0)):
            value = bytearray(GOOD_ACL)
            value[offset] = replacement
            with self.subTest(offset=offset), self.assertRaises(dacl._UnsafeDescriptor):
                dacl._acl_entries(bytes(value))

    def test_creation_is_handle_relative_exclusive_and_security_is_atomic(self):
        api, _, nt, _ = self.native()
        creating = True
        def opened(handle, access, attributes, status, allocation, attrs,
                   share, disposition, options, ea, ea_size):
            value = c.cast(attributes, c.POINTER(dacl._ObjectAttributes)).contents
            self.assertEqual(value.RootDirectory, 17)
            self.assertEqual(value.SecurityDescriptor, 123 if creating else None)
            self.assertEqual(c.wstring_at(value.ObjectName.contents.Buffer), "private-dacl")
            self.assertEqual((access, share, disposition, options),
                             (0x120081, 1, 2 if creating else 1, 0x200021))
            c.cast(handle, c.POINTER(c.c_void_p))[0] = 23
            c.cast(status, c.POINTER(dacl._IoStatus)).contents.Information = 2 if creating else 1
            return 0
        nt.NtCreateFile.side_effect = opened
        self.assertEqual(api._directory(17, "private-dacl", c.c_void_p(123)), 23)
        creating = False
        self.assertEqual(api._directory(17, "private-dacl"), 23)
        nt.NtCreateFile.reset_mock()
        for name in ("..", "a/b", "C:x", "bad."):
            with self.assertRaises(ValueError):
                api._directory(17, name, c.c_void_p(123))
        with self.assertRaises(ValueError):
            api._directory(17, "private-dacl", c.c_void_p())
        nt.NtCreateFile.assert_not_called()

    def test_unexpected_success_disposition_closes_handle_and_is_unsafe(self):
        api, kernel, nt, _ = self.native()
        def opened(handle, access, attributes, status, *args):
            c.cast(handle, c.POINTER(c.c_void_p))[0] = 23
            c.cast(status, c.POINTER(dacl._IoStatus)).contents.Information = 1
            return 0
        nt.NtCreateFile.side_effect = opened
        with self.assertRaises(dacl._UnsafeDescriptor):
            api._directory(17, "private-dacl", c.c_void_p(123))
        kernel.CloseHandle.assert_called_once_with(23)

    def test_descriptor_is_protected_exact_and_freed_even_on_error(self):
        api, kernel, _, security = self.native()
        def convert(sddl, revision, output, size):
            self.assertEqual(sddl, "O:S-1-5-21-12345D:P(A;;FA;;;S-1-5-21-12345)(A;;FA;;;SY)(A;;FA;;;BA)")
            self.assertEqual((revision, size), (1, None))
            c.cast(output, c.POINTER(c.c_void_p))[0] = 123
            return 1
        security.ConvertStringSecurityDescriptorToSecurityDescriptorW.side_effect = convert
        with self.assertRaisesRegex(RuntimeError, "fixture"):
            with api.descriptor("S-1-5-21-12345"):
                raise RuntimeError("fixture")
        self.assertEqual(kernel.LocalFree.call_args.args[0].value, 123)

    def test_handle_readback_parses_bounded_owner_and_acl_and_frees_descriptor(self):
        api, kernel, _, security = self.native()
        raw = c.create_string_buffer(bytes(20) + USER + GOOD_ACL)
        base = c.addressof(raw)
        def read(handle, kind, flags, owner, group, acl_out, sacl, descriptor):
            self.assertEqual((handle, kind, flags, group, sacl), (23, 1, 5, None, None))
            c.cast(owner, c.POINTER(c.c_void_p))[0] = base + 20
            c.cast(acl_out, c.POINTER(c.c_void_p))[0] = base + 20 + len(USER)
            c.cast(descriptor, c.POINTER(c.c_void_p))[0] = base
            return 0
        def control(descriptor, flags, revision):
            c.cast(flags, c.POINTER(c.c_uint16))[0] = 0x9004
            c.cast(revision, c.POINTER(c.c_uint32))[0] = 1
            return 1
        security.GetSecurityInfo.side_effect = read
        security.IsValidSecurityDescriptor.return_value = 1
        security.GetSecurityDescriptorLength.return_value = len(raw) - 1
        security.GetSecurityDescriptorControl.side_effect = control
        self.assertEqual(api.security(23), GOOD)
        self.assertEqual(kernel.LocalFree.call_args.args[0].value, base)
        security.GetSecurityDescriptorLength.return_value = 20
        with self.assertRaises(dacl._UnsafeDescriptor):
            api.security(23)
        self.assertEqual(kernel.LocalFree.call_count, 2)

    def test_thread_token_denial_does_not_fall_back_to_process_identity(self):
        api, _, _, security = self.native()
        security.OpenThreadToken.return_value = 0
        with patch.object(c, "get_last_error", return_value=5), \
                patch.object(c, "WinError", side_effect=winerror):
            with self.assertRaises(OSError):
                api.current_user()
        security.OpenProcessToken.assert_not_called()

    def test_no_thread_token_uses_bounded_process_token_query_and_closes_handle(self):
        api, kernel, _, security = self.native()
        security.OpenThreadToken.return_value = 0
        def opened(process, access, output):
            self.assertEqual(access, 8)
            c.cast(output, c.POINTER(c.c_void_p))[0] = 23
            return 1
        def query(token, kind, buffer, size, output):
            self.assertEqual(kind, 1)
            c.cast(output, c.POINTER(c.c_uint32))[0] = 70000
            return 0
        security.OpenProcessToken.side_effect = opened
        security.GetTokenInformation.side_effect = query
        with patch.object(c, "get_last_error", side_effect=[1008, 122]):
            with self.assertRaisesRegex(RuntimeError, "size unavailable"):
                api.current_user()
        self.assertEqual(kernel.CloseHandle.call_args.args[0].value, 23)
        security.ConvertSidToStringSidW.assert_not_called()

    def run_probe(self, *, unsafe=False, read_error=False, collision=True,
                  changed=False, creation_error=None, reparse=False, parent_changed=False):
        api = Mock()
        api.current_user.return_value = USER, "synthetic-user"
        api.descriptor.return_value.__enter__ = Mock(return_value=c.c_void_p(123))
        api.descriptor.return_value.__exit__ = Mock(return_value=False)
        api.open_root.return_value = 1
        api._directory.side_effect = ([creation_error] if creation_error else
                                     [2, winerror(183) if collision else 4, 3])
        def info(handle):
            if reparse:
                raise dacl.ReparseRejected("reparse fixture")
            return ((7, b"root" if handle == 1 else b"private" if handle == 2 or not changed else b"changed"), True)
        api.info.side_effect = info
        if parent_changed:
            api.info.side_effect = [((7, b"root"), True), ((7, b"changed"), True)]
        api.security.side_effect = [GOOD._replace(control=0x8004) if unsafe else GOOD,
                                    PermissionError("private identity") if read_error else GOOD, GOOD]
        child = Path("validated-disposable-child")
        with patch.object(os, "name", "nt"), patch.object(dacl, "_DaclNative", return_value=api):
            result = dacl.probe_private_dacl(child)
        return result, api

    def test_positive_requires_persisted_descriptor_collision_reopen_and_identity(self):
        (result, observations), api = self.run_probe()
        self.assertIs(result, True)
        self.assertEqual(len(observations), 6)
        self.assertEqual(api.security.call_count, 3)
        self.assertEqual([call.args[0] for call in api.close.call_args_list], [3, 2, 1])
        self.assertNotIn("synthetic-user", repr(observations))

    def test_bad_descriptor_stops_before_reopen_or_payload(self):
        (result, observations), api = self.run_probe(unsafe=True)
        self.assertIs(result, False)
        self.assertFalse(observations["created_private"])
        self.assertEqual(api._directory.call_count, 1)
        self.assertEqual([call.args[0] for call in api.close.call_args_list], [2, 1])

    def test_collision_and_identity_regressions_cannot_pass(self):
        for kwargs in (dict(collision=False), dict(changed=True), dict(reparse=True), dict(parent_changed=True)):
            with self.subTest(kwargs=kwargs):
                self.assertIs(self.run_probe(**kwargs)[0][0], False)

    def test_read_failure_is_unknown_and_does_not_log_identity(self):
        (result, observations), _ = self.run_probe(read_error=True)
        self.assertIsNone(result)
        self.assertEqual(observations["error_type"], "PermissionError")
        self.assertNotIn("private identity", repr(observations))

    def test_observed_unsafe_result_survives_later_read_failure(self):
        self.assertIs(self.run_probe(collision=False, read_error=True)[0][0], False)

    def test_existing_name_or_permission_failure_does_not_repair_or_retry(self):
        for error in (winerror(183), winerror(5)):
            with self.subTest(error=error):
                (result, _), api = self.run_probe(creation_error=error)
                self.assertIsNone(result)
                self.assertEqual(api._directory.call_count, 1)
                api.security.assert_not_called()



class DaclIntegrationTests(unittest.TestCase):
    def test_explicit_dacl_result_is_wired_without_claiming_recovery(self):
        import importlib.util
        script = Path(__file__).with_name("probe-windows-filesystem.py")
        spec = importlib.util.spec_from_file_location("filesystem_probe_dacl", script)
        probe = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(probe)
        root = Path("fixture")
        for value, expected in ((True, "C:inconclusive"),
                                (False, "B:unsafe_or_unsupported"),
                                (None, "C:inconclusive")):
            stream = Mock()
            stream.__enter__ = Mock(return_value=stream)
            stream.__exit__ = Mock(return_value=False)
            with self.subTest(value=value), patch.object(os, "name", "nt"), \
                    patch.object(Path, "resolve", return_value=root), \
                    patch.object(Path, "is_dir", return_value=True), \
                    patch.object(Path, "is_symlink", return_value=False), \
                    patch.object(Path, "mkdir"), \
                    patch.object(probe, "volume_info", return_value=("NTFS", 1)), \
                    patch.object(Path, "open", side_effect=[stream, stream, FileExistsError(), stream]), \
                    patch.object(Path, "read_bytes", side_effect=[
                        b"polaris-existing\r\n", b"polaris-staged\r\n",
                        b"polaris-existing\r\n", b"polaris-existing\r\n", b"polaris-staged\r\n"]), \
                    patch.object(Path, "rename", side_effect=FileExistsError()), \
                    patch.object(os, "fsync"), \
                    patch.object(probe, "probe_lease", return_value=(True, {})), \
                    patch.object(probe, "probe_paths", return_value=(True, {})), \
                    patch.object(probe, "probe_volume", return_value=(True, {})), \
                    patch.object(probe, "probe_private_dacl",
                                 return_value=(value, {"fixture": True})) as collector:
                result = probe.probe(root, private_dacl=True)
            collector.assert_called_once()
            self.assertEqual(collector.call_args.args[0].parent, root)
            self.assertIs(result["checks"]["protected_private_dacl"], value)
            self.assertEqual(result["observations"]["private_dacl"], {"fixture": True})
            self.assertIs(result["observations"]["storage_prerequisites_passed"], value is True)
            self.assertIsNone(result["checks"]["process_termination_recovery"])
            self.assertEqual(result["classification"], expected)
        with patch("sys.argv", ["probe", "--root", str(root), "--private-dacl"]), \
                patch.object(probe, "probe", return_value={}) as run, patch("builtins.print"):
            self.assertEqual(probe.main(), 0)
        run.assert_called_once_with(root, None, private_dacl=True)


if __name__ == "__main__":
    unittest.main()
