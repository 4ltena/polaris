"""Private-DACL probe for an explicitly requested disposable fixture.

Native execution creates one empty directory under the main probe's child.
It never repairs existing ACLs or enables privileges.
"""

from __future__ import annotations

import ctypes as c
import os
import struct
from contextlib import contextmanager
from pathlib import Path
from typing import NamedTuple

from windows_probe_paths import (
    ReparseRejected, _IoStatus, _Native, _ObjectAttributes, _Pinned, _UnicodeString, _component,
)


FULL_ACCESS = 0x1F01FF
SYSTEM = bytes.fromhex("010100000000000512000000")
ADMINISTRATORS = bytes.fromhex("01020000000000052000000020020000")


class _UnsafeDescriptor(RuntimeError):
    pass


class _Ace(NamedTuple):
    kind: int
    flags: int
    mask: int
    sid: bytes


class _Security(NamedTuple):
    owner: bytes
    control: int
    aces: tuple[_Ace, ...] | None


def _sid_length(data: bytes) -> int:
    if len(data) < 8 or data[0] != 1 or data[1] > 15:
        raise _UnsafeDescriptor("invalid SID header")
    size = 8 + data[1] * 4
    if len(data) < size:
        raise _UnsafeDescriptor("truncated SID")
    return size


def _acl_entries(data: bytes) -> tuple[_Ace, ...]:
    if len(data) < 8:
        raise _UnsafeDescriptor("truncated ACL")
    revision, reserved, size, count, reserved2 = struct.unpack_from("<BBHHH", data)
    if revision not in (2, 4) or reserved or reserved2 or size != len(data) or count > 64:
        raise _UnsafeDescriptor("unsupported ACL header")
    offset, entries = 8, []
    for _ in range(count):
        if offset + 4 > size:
            raise _UnsafeDescriptor("truncated ACE header")
        kind, flags, length = struct.unpack_from("<BBH", data, offset)
        if length < 16 or length % 4 or offset + length > size:
            raise _UnsafeDescriptor("invalid ACE length")
        # Deny, object, callback and future ACE types are not this private policy.
        if kind != 0:
            raise _UnsafeDescriptor("unexpected ACE type")
        mask = struct.unpack_from("<I", data, offset + 4)[0]
        sid = data[offset + 8:offset + length]
        if _sid_length(sid) != len(sid):
            raise _UnsafeDescriptor("unexpected ACE payload")
        entries.append(_Ace(kind, flags, mask, sid))
        offset += length
    # An ACL may reserve unused capacity, but it must not conceal extra ACE data.
    if any(data[offset:]):
        raise _UnsafeDescriptor("unexpected ACL trailing data")
    return tuple(entries)


def _is_private(security: _Security, user: bytes) -> bool:
    expected = {user, SYSTEM, ADMINISTRATORS}
    if (security.owner != user or security.control & 0x1004 != 0x1004
            or security.aces is None or len(security.aces) != len(expected)):
        return False
    # Explicit full access for exactly these principals also bounds effective
    # grants; unknown/conditional/deny/inherited/inherit-only ACEs are refused.
    return (all(ace.kind == 0 and ace.flags == 0 and ace.mask == FULL_ACCESS
                for ace in security.aces)
            and {ace.sid for ace in security.aces} == expected)


class _DaclNative(_Native):
    def __init__(self):
        super().__init__()
        self.security_api = c.WinDLL("advapi32", use_last_error=True)
        ptr, dword, boolean = c.c_void_p, c.c_uint32, c.c_int
        pp = c.POINTER(ptr)
        bindings = {
            "OpenThreadToken": ([ptr, dword, boolean, pp], boolean),
            "OpenProcessToken": ([ptr, dword, pp], boolean),
            "GetTokenInformation": ([ptr, c.c_int, ptr, dword, c.POINTER(dword)], boolean),
            "ConvertSidToStringSidW": ([ptr, pp], boolean),
            "ConvertStringSecurityDescriptorToSecurityDescriptorW":
                ([c.c_wchar_p, dword, pp, c.POINTER(dword)], boolean),
            "GetSecurityInfo": ([ptr, c.c_int, dword, pp, pp, pp, pp, pp], dword),
            "IsValidSecurityDescriptor": ([ptr], boolean),
            "GetSecurityDescriptorLength": ([ptr], dword),
            "GetSecurityDescriptorControl": ([ptr, c.POINTER(c.c_uint16), c.POINTER(dword)], boolean),
        }
        for name, (args, result) in bindings.items():
            function = getattr(self.security_api, name)
            function.argtypes, function.restype = args, result
        for name in ("GetCurrentProcess", "GetCurrentThread"):
            function = getattr(self.kernel, name)
            function.argtypes, function.restype = [], ptr
        self.kernel.LocalFree.argtypes, self.kernel.LocalFree.restype = [ptr], ptr

    @staticmethod
    def _bounded(base: int, size: int, pointer: int | None, length: int) -> int:
        if pointer is None or length < 0 or pointer < base or pointer + length > base + size:
            raise _UnsafeDescriptor("security data is outside its buffer")
        return pointer

    def _sid(self, base: int, size: int, pointer: int | None) -> bytes:
        address = self._bounded(base, size, pointer, 8)
        header = c.string_at(address, 8)
        if header[0] != 1 or header[1] > 15:
            raise _UnsafeDescriptor("invalid SID header")
        length = 8 + 4 * header[1]
        self._bounded(base, size, address, length)
        return c.string_at(address, length)

    def current_user(self) -> tuple[bytes, str]:
        token = c.c_void_p()
        api = self.security_api
        if not api.OpenThreadToken(self.kernel.GetCurrentThread(), 8, True, c.byref(token)):
            error = c.get_last_error()
            if error != 1008:  # ERROR_NO_TOKEN is the only process-token fallback.
                raise c.WinError(error)
            if not api.OpenProcessToken(self.kernel.GetCurrentProcess(), 8, c.byref(token)):
                raise c.WinError(c.get_last_error())
        if not token.value:
            raise RuntimeError("token query returned no handle")
        try:
            required = c.c_uint32()
            if api.GetTokenInformation(token, 1, None, 0, c.byref(required)):
                raise RuntimeError("token size query unexpectedly succeeded")
            if c.get_last_error() != 122 or not 16 <= required.value <= 65536:
                raise RuntimeError("token size unavailable")
            buffer = c.create_string_buffer(required.value)
            if not api.GetTokenInformation(token, 1, buffer, len(buffer), c.byref(required)):
                raise c.WinError(c.get_last_error())
            if not c.sizeof(c.c_void_p) <= required.value <= len(buffer):
                raise RuntimeError("invalid token read length")
            pointer = c.c_void_p.from_buffer(buffer).value
            sid = self._sid(c.addressof(buffer), required.value, pointer)
            text = c.c_void_p()
            if not api.ConvertSidToStringSidW(pointer, c.byref(text)):
                raise c.WinError(c.get_last_error())
            try:
                if not text.value:
                    raise RuntimeError("SID conversion returned no value")
                return sid, c.wstring_at(text.value)
            finally:
                if text.value:
                    self.kernel.LocalFree(text)
        finally:
            self.close(token)

    @contextmanager
    def descriptor(self, user_text: str):
        # The SID comes directly from TokenUser. Never persist or log this SDDL.
        sddl = f"O:{user_text}D:P(A;;FA;;;{user_text})(A;;FA;;;SY)(A;;FA;;;BA)"
        descriptor = c.c_void_p()
        if not self.security_api.ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl, 1, c.byref(descriptor), None):
            raise c.WinError(c.get_last_error())
        try:
            if not descriptor.value:
                raise RuntimeError("descriptor conversion returned no value")
            yield descriptor
        finally:
            if descriptor.value:
                self.kernel.LocalFree(descriptor)

    def _directory(self, parent, name: str, descriptor=None):
        _component(name)
        if descriptor is not None and not descriptor:
            raise ValueError("creation requires a non-null private descriptor")
        buffer = c.create_unicode_buffer(name)
        length = len(name.encode("utf-16-le"))
        unicode = _UnicodeString(length, length + 2, c.cast(buffer, c.c_void_p))
        attributes = _ObjectAttributes(c.sizeof(_ObjectAttributes), parent,
            c.pointer(unicode), 0x40, descriptor, None)
        handle, status = c.c_void_p(), _IoStatus()
        creating = descriptor is not None
        # READ_CONTROL | LIST_DIRECTORY | READ_ATTRIBUTES | SYNCHRONIZE;
        # FILE_CREATE or FILE_OPEN only. No replacement or default-DACL creation.
        result = self.nt.NtCreateFile(c.byref(handle), 0x120081, c.byref(attributes),
            c.byref(status), None, 0, 1, 2 if creating else 1, 0x200021, None, 0)
        if result < 0:
            raise c.WinError(self.nt.RtlNtStatusToDosError(result))
        if not handle.value:
            raise RuntimeError("directory operation returned no handle")
        if status.Information != (2 if creating else 1):
            self.close(handle.value)
            raise _UnsafeDescriptor("directory disposition was not honored")
        return handle.value

    def security(self, handle) -> _Security:
        owner, dacl, descriptor = c.c_void_p(), c.c_void_p(), c.c_void_p()
        api = self.security_api
        error = api.GetSecurityInfo(handle, 1, 5, c.byref(owner), None,
                                    c.byref(dacl), None, c.byref(descriptor))
        if error:
            raise c.WinError(error)
        try:
            if not descriptor.value or not api.IsValidSecurityDescriptor(descriptor):
                raise _UnsafeDescriptor("invalid security descriptor")
            size = api.GetSecurityDescriptorLength(descriptor)
            if not 20 <= size <= 65536:
                raise _UnsafeDescriptor("invalid descriptor length")
            control, revision = c.c_uint16(), c.c_uint32()
            if not api.GetSecurityDescriptorControl(descriptor, c.byref(control), c.byref(revision)):
                raise c.WinError(c.get_last_error())
            if revision.value != 1:
                raise _UnsafeDescriptor("unsupported descriptor revision")
            sid = self._sid(descriptor.value, size, owner.value)
            entries = None
            if dacl.value is not None:
                address = self._bounded(descriptor.value, size, dacl.value, 8)
                acl_size = c.c_uint16.from_address(address + 2).value
                self._bounded(descriptor.value, size, address, acl_size)
                entries = _acl_entries(c.string_at(address, acl_size))
            return _Security(sid, control.value, entries)
        finally:
            if descriptor.value:
                self.kernel.LocalFree(descriptor)


def probe_private_dacl(child: Path) -> tuple[bool | None, dict[str, object]]:
    """Create only inside the main probe's validated disposable child.

    No payload, secret, journal or report is written in the new empty directory.
    """
    if os.name != "nt":
        raise RuntimeError("native Windows is required")
    observations: dict[str, object] = {}
    try:
        api = _DaclNative()
        user, user_text = api.current_user()
        with api.descriptor(user_text) as descriptor:
            with _Pinned(api, api.open_root(child), True) as root:
                if not root.unchanged():
                    raise _UnsafeDescriptor("parent binding changed")
                with _Pinned(api, api._directory(root.handle, "private-dacl", descriptor), True) as first:
                    original = api.security(first.handle)
                    observations["created_private"] = _is_private(original, user)
                    if not observations["created_private"]:
                        raise _UnsafeDescriptor("created descriptor does not match private policy")
                    try:
                        duplicate = api._directory(root.handle, "private-dacl", descriptor)
                    except OSError as exc:
                        if getattr(exc, "winerror", None) not in (80, 183):
                            raise
                        observations["collision_refused"] = True
                    else:
                        api.close(duplicate)
                        observations["collision_refused"] = False
                    with _Pinned(api, api._directory(root.handle, "private-dacl"), True) as reopened:
                        observations["reopen_same_directory"] = first.identity == reopened.identity
                        observations["reopened_private"] = _is_private(api.security(reopened.handle), user)
                        observations["descriptor_unchanged"] = api.security(first.handle) == original
                        observations["identities_unchanged"] = all(
                            entry.unchanged() for entry in (root, first, reopened))
        return all(value is True for value in observations.values()), observations
    except (_UnsafeDescriptor, ReparseRejected):
        observations["unsafe_descriptor_or_binding"] = True
        return False, observations
    except (OSError, RuntimeError, ValueError) as exc:
        # Error text can contain identity strings. Persist only type/code.
        observations["error_type"] = type(exc).__name__
        observations["winerror"] = getattr(exc, "winerror", None)
        return (False if any(value is False for value in observations.values()) else None), observations
