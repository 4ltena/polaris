"""Handle-relative path capability checks inside the validated probe child."""

from __future__ import annotations

import ctypes as c
from contextlib import ExitStack
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path, PureWindowsPath
import re
import secrets
import struct
import subprocess
import sys
import time


class _UnicodeString(c.Structure):
    _fields_ = [("Length", c.c_uint16), ("MaximumLength", c.c_uint16),
                ("Buffer", c.c_void_p)]


class _ObjectAttributes(c.Structure):
    _fields_ = [("Length", c.c_uint32), ("RootDirectory", c.c_void_p),
                ("ObjectName", c.POINTER(_UnicodeString)), ("Attributes", c.c_uint32),
                ("SecurityDescriptor", c.c_void_p), ("SecurityQualityOfService", c.c_void_p)]


class _Status(c.Union):
    _fields_ = [("Status", c.c_int32), ("Pointer", c.c_void_p)]


class _IoStatus(c.Structure):
    _fields_ = [("Status", _Status), ("Information", c.c_size_t)]


class _FileId(c.Structure):
    _fields_ = [("VolumeSerialNumber", c.c_uint64), ("FileId", c.c_ubyte * 16)]


class _AttributeTag(c.Structure):
    _fields_ = [("FileAttributes", c.c_uint32), ("ReparseTag", c.c_uint32)]


class ReparseRejected(RuntimeError):
    pass


def _component(name: str) -> None:
    if (not name or name in {".", ".."} or name[-1] in {".", " "}
            or any(ch in '/\\:*?"<>|' or ord(ch) < 32 for ch in name)
            or len(name.encode("utf-16-le")) > 510):
        raise ValueError("a single ordinary path component is required")


class _Native:
    def __init__(self):
        self.kernel = c.WinDLL("kernel32", use_last_error=True)
        self.nt = c.WinDLL("ntdll", use_last_error=True)
        self.kernel.CreateFileW.argtypes = [c.c_wchar_p, c.c_uint32, c.c_uint32,
                                           c.c_void_p, c.c_uint32, c.c_uint32, c.c_void_p]
        self.kernel.CreateFileW.restype = c.c_void_p
        self.kernel.GetFileInformationByHandleEx.argtypes = [c.c_void_p, c.c_int,
                                                            c.c_void_p, c.c_uint32]
        self.kernel.GetFileInformationByHandleEx.restype = c.c_int
        self.kernel.ReadFile.argtypes = [c.c_void_p, c.c_void_p, c.c_uint32,
                                        c.POINTER(c.c_uint32), c.c_void_p]
        self.kernel.ReadFile.restype = c.c_int
        self.kernel.CloseHandle.argtypes = [c.c_void_p]
        self.kernel.CloseHandle.restype = c.c_int
        self.nt.NtCreateFile.argtypes = [c.POINTER(c.c_void_p), c.c_uint32,
            c.POINTER(_ObjectAttributes), c.POINTER(_IoStatus), c.c_void_p,
            c.c_uint32, c.c_uint32, c.c_uint32, c.c_uint32, c.c_void_p, c.c_uint32]
        self.nt.NtCreateFile.restype = c.c_int32
        self.nt.RtlNtStatusToDosError.argtypes = [c.c_int32]
        self.nt.RtlNtStatusToDosError.restype = c.c_uint32

    def open_root(self, path: Path, *, preparation: bool = False):
        # LIST_DIRECTORY | READ_ATTRIBUTES | SYNCHRONIZE. Attribute-only access
        # does not participate in sharing checks. Deny write/delete sharing.
        # OPEN_EXISTING; BACKUP_SEMANTICS permits a directory, without privileges.
        access, share = (0x1000A0, 3) if preparation else (0x100081, 1)
        handle = self.kernel.CreateFileW(str(path), access, share, None, 3,
                                         0x02000000 | 0x00200000, None)
        if handle in (None, c.c_void_p(-1).value):
            raise c.WinError(c.get_last_error())
        return handle

    def open_relative(self, parent, name: str, *, delete_access: bool = False,
                      preparation: bool = False):
        _component(name)
        buffer = c.create_unicode_buffer(name)
        length = len(name.encode("utf-16-le"))
        unicode = _UnicodeString(length, length + 2, c.cast(buffer, c.c_void_p))
        attributes = _ObjectAttributes(c.sizeof(_ObjectAttributes), parent,
                                       c.pointer(unicode), 0x40, None, None)
        handle, status = c.c_void_p(), _IoStatus()
        # FILE_OPEN only; synchronous, no reparse processing. No NT path fallback.
        if preparation and delete_access:
            raise ValueError("preparation pins cannot request delete access")
        access = (0x1000A0 if preparation else 0x100081) | (0x10000 if delete_access else 0)
        share = 3 if preparation else 1
        result = self.nt.NtCreateFile(c.byref(handle), access, c.byref(attributes),
            c.byref(status), None, 0, share, 1, 0x00200000 | 0x20, None, 0)
        if result < 0:
            raise c.WinError(self.nt.RtlNtStatusToDosError(result))
        if preparation and result != 0:
            if handle.value:
                self.close(handle.value)
            raise RuntimeError("preparation open did not complete synchronously")
        if not handle.value:
            raise RuntimeError("relative open returned no handle")
        return handle.value

    def raw_info(self, handle):
        tag, identity = _AttributeTag(), _FileId()
        for kind, output in ((9, tag), (18, identity)):
            if not self.kernel.GetFileInformationByHandleEx(
                    handle, kind, c.byref(output), c.sizeof(output)):
                raise c.WinError(c.get_last_error())
        return (identity.VolumeSerialNumber, bytes(identity.FileId)), tag.FileAttributes, tag.ReparseTag

    def info(self, handle):
        identity, attributes, tag = self.raw_info(handle)
        # Reject all reparse tags, including unknown future tags.
        if attributes & 0x400:
            raise ReparseRejected(f"reparse tag {tag:#x}")
        if not any(identity[1]):
            raise RuntimeError("file identity is unavailable")
        return identity, bool(attributes & 0x10)

    def close(self, handle):
        if not self.kernel.CloseHandle(handle):
            raise c.WinError(c.get_last_error())

    def read(self, handle, limit: int) -> bytes:
        if not 0 < limit <= 4096:
            raise ValueError("probe read must be bounded")
        buffer, count = c.create_string_buffer(limit), c.c_uint32()
        if not self.kernel.ReadFile(handle, buffer, limit, c.byref(count), None):
            raise c.WinError(c.get_last_error())
        if count.value > limit:
            raise RuntimeError("invalid probe read length")
        return buffer.raw[:count.value]


class _Pinned:
    def __init__(self, api, handle, directory: bool):
        self.api, self.handle = api, handle
        try:
            self.identity, self.directory = api.info(handle)
            if self.directory != directory:
                raise RuntimeError("unexpected file type")
        except BaseException:
            api.close(handle)
            raise

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.api.close(self.handle)

    def unchanged(self):
        return self.api.info(self.handle) == (self.identity, self.directory)

    def child(self, name: str, *, directory: bool, delete_access: bool = False):
        _component(name)
        if not self.directory or not self.unchanged():
            raise RuntimeError("parent binding changed")
        return _Pinned(self.api, self.api.open_relative(
            self.handle, name, delete_access=delete_access), directory)


def _rejects_link(parent, name: str, *, directory: bool) -> bool:
    try:
        with parent.child(name, directory=directory):
            return False
    except ReparseRejected:
        return True


def probe_paths(child: Path, *, prepare_links_with_uac: bool = False) -> tuple[bool | None, dict[str, object]]:
    """Root preflight belongs to probe-windows-filesystem.py; no standalone CLI."""
    if os.name != "nt":
        raise RuntimeError("native Windows is required")
    if prepare_links_with_uac:
        return _probe_paths_with_uac(child)
    fixture = child / "path-binding"
    normal = fixture / "normal"
    marker = normal / "payload"
    payload = b"polaris-path-binding\r\n"
    observations: dict[str, object] = {}
    result: bool | None = None
    try:
        fixture.mkdir()
        normal.mkdir()
        with marker.open("xb") as stream:
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
        # Both targets remain inside this disposable fixture. No privilege changes.
        os.symlink(normal, fixture / "alias", target_is_directory=True)
        os.symlink(marker, normal / "alias-file", target_is_directory=False)
        api = _Native()
        with _Pinned(api, api.open_root(fixture), True) as root:
            result = _verify_fixture(api, root, normal, payload, observations)
    except (OSError, RuntimeError, ValueError) as exc:
        observations["error"] = str(exc)
        # An already observed unsafe outcome must not become inconclusive.
        if any(value is False for value in observations.values()):
            result = False
        else:
            result = None
    return result, observations


# L-01 is confined to this fixed disposable case layout.
_CASE_ROOT = PureWindowsPath(r"D:\works\projects\codex\polaris-restored-20261008\windows-checks")
_PROJECT = PureWindowsPath(r"D:\works\projects\codex\polaris-v100")
_PYTHON = PureWindowsPath(r"C:\Users\example\AppData\Local\Programs\Python\Python310\python.exe")
_SOURCES = ("probe-windows-filesystem.py", "windows_probe_paths.py",
            "windows_probe_volume.py", "windows_probe_lease.py", "windows_probe_dacl.py")
_MODE = "internal-prepare-path-links-v1"
_PAYLOAD = b"polaris-path-binding\r\n"
_GUID = re.compile(r"\\\\\?\\Volume\{[0-9a-fA-F-]{36}\}\\")
_CASE = re.compile(r"storage-probe-\d{8}-\d{6}-[0-9a-f]{8}")
_CHILD = re.compile(r"polaris-windows-fs-probe-[0-9a-f]{32}")


def _utc():
    return datetime.now(timezone.utc).isoformat()


def _record(pin):
    return {"volume": pin.identity[0], "file_id": pin.identity[1].hex(),
            "directory": pin.directory}


def _symlink_target(data: bytes, target: str):
    if len(data) < 20:
        raise ValueError("truncated symlink reparse data")
    tag, size, reserved, sub_offset, sub_size, print_offset, print_size, flags = struct.unpack_from(
        "<IHHHHHHI", data)
    if tag != 0xA000000C or flags != 1 or reserved != 0 or size + 8 != len(data):
        raise ValueError("not an exact relative symlink")
    path_buffer = data[20:]
    for offset, length in ((sub_offset, sub_size), (print_offset, print_size)):
        if offset % 2 or length % 2 or offset + length > len(path_buffer):
            raise ValueError("invalid symlink target bounds")
        if path_buffer[offset:offset + length].decode("utf-16-le") != target:
            raise ValueError("unexpected raw symlink target")


class _ShellExecuteInfo(c.Structure):
    _fields_ = [("cbSize", c.c_uint32), ("fMask", c.c_uint32), ("hwnd", c.c_void_p),
                ("lpVerb", c.c_wchar_p), ("lpFile", c.c_wchar_p),
                ("lpParameters", c.c_wchar_p), ("lpDirectory", c.c_wchar_p),
                ("nShow", c.c_int), ("hInstApp", c.c_void_p), ("lpIDList", c.c_void_p),
                ("lpClass", c.c_wchar_p), ("hkeyClass", c.c_void_p),
                ("dwHotKey", c.c_uint32), ("hIcon", c.c_void_p), ("hProcess", c.c_void_p)]


class _PreparationNative(_Native):
    def __init__(self):
        super().__init__()
        kernel = self.kernel
        signatures = {
            "GetVolumeNameForVolumeMountPointW": ([c.c_wchar_p, c.c_void_p, c.c_uint32], c.c_int),
            "GetFinalPathNameByHandleW": ([c.c_void_p, c.c_void_p, c.c_uint32, c.c_uint32], c.c_uint32),
            "CreateSymbolicLinkW": ([c.c_wchar_p, c.c_wchar_p, c.c_uint32], c.c_ubyte),
            "DeviceIoControl": ([c.c_void_p, c.c_uint32, c.c_void_p, c.c_uint32,
                                 c.c_void_p, c.c_uint32, c.POINTER(c.c_uint32), c.c_void_p], c.c_int),
            "SetFilePointerEx": ([c.c_void_p, c.c_int64, c.c_void_p, c.c_uint32], c.c_int),
            "GetCurrentProcess": ([], c.c_void_p),
            "GetProcessId": ([c.c_void_p], c.c_uint32),
            "WaitForSingleObject": ([c.c_void_p, c.c_uint32], c.c_uint32),
            "GetExitCodeProcess": ([c.c_void_p, c.POINTER(c.c_uint32)], c.c_int),
        }
        for name, (arguments, result) in signatures.items():
            function = getattr(kernel, name)
            function.argtypes, function.restype = arguments, result
        self.security = c.WinDLL("advapi32", use_last_error=True)
        self.security.OpenProcessToken.argtypes = [c.c_void_p, c.c_uint32, c.POINTER(c.c_void_p)]
        self.security.OpenProcessToken.restype = c.c_int
        self.security.GetTokenInformation.argtypes = [c.c_void_p, c.c_int, c.c_void_p,
                                                      c.c_uint32, c.POINTER(c.c_uint32)]
        self.security.GetTokenInformation.restype = c.c_int
        self.shell = c.WinDLL("shell32", use_last_error=True)
        self.shell.ShellExecuteExW.argtypes = [c.POINTER(_ShellExecuteInfo)]
        self.shell.ShellExecuteExW.restype = c.c_int
        self.shell.IsUserAnAdmin.argtypes = []
        self.shell.IsUserAnAdmin.restype = c.c_int

    def require_normal(self):
        if PureWindowsPath(sys.executable) != _PYTHON:
            raise RuntimeError("the pinned Python executable is required")
        if self.shell.IsUserAnAdmin():
            raise RuntimeError("ordinary collection rejects administrator tokens")

    def require_full_token(self):
        if PureWindowsPath(sys.executable) != _PYTHON:
            raise RuntimeError("the pinned Python executable is required")
        token, elevation, length = c.c_void_p(), c.c_uint32(), c.c_uint32()
        if not self.security.OpenProcessToken(self.kernel.GetCurrentProcess(), 8, c.byref(token)):
            raise c.WinError(c.get_last_error())
        if not token.value:
            raise RuntimeError("token query returned no handle")
        try:
            if not self.security.GetTokenInformation(
                    token, 18, c.byref(elevation), c.sizeof(elevation), c.byref(length)):
                raise c.WinError(c.get_last_error())
            if length.value != c.sizeof(elevation) or elevation.value != 2:
                raise RuntimeError("helper requires TokenElevationTypeFull")
        finally:
            self.close(token.value)

    def volume_guid(self, drive: str):
        buffer = c.create_unicode_buffer(128)
        if not self.kernel.GetVolumeNameForVolumeMountPointW(drive, buffer, len(buffer)):
            raise c.WinError(c.get_last_error())
        if not _GUID.fullmatch(buffer.value):
            raise RuntimeError("volume GUID is unavailable")
        return buffer.value

    def final_path(self, handle):
        buffer = c.create_unicode_buffer(32768)
        count = self.kernel.GetFinalPathNameByHandleW(handle, buffer, len(buffer), 1)
        if not count or count >= len(buffer) or not _GUID.match(buffer.value):
            raise RuntimeError("normalized GUID handle path is unavailable")
        return buffer.value

    def contents(self, handle, limit=1024 * 1024):
        if not self.kernel.SetFilePointerEx(handle, 0, None, 0):
            raise c.WinError(c.get_last_error())
        data = bytearray()
        while True:
            part = self.read(handle, 4096)
            if not part:
                return bytes(data)
            data.extend(part)
            if len(data) > limit:
                raise ValueError("pinned file exceeds the bounded read")

    def absent(self, parent, name):
        try:
            handle = self.open_relative(parent.handle, name)
        except OSError as exc:
            if getattr(exc, "winerror", None) in (2, 3):
                return
            raise
        else:
            self.close(handle)
            raise RuntimeError("link leaf already exists")

    def validate_link(self, parent, name, target, directory):
        if not parent.unchanged():
            raise RuntimeError("link parent binding changed")
        handle = self.open_relative(parent.handle, name)
        try:
            before = self.raw_info(handle)
            identity, attributes, tag = before
            if tag != 0xA000000C or not attributes & 0x400 or bool(attributes & 0x10) != directory:
                raise RuntimeError("unexpected link tag or type")
            if not any(identity[1]) or identity[0] != parent.identity[0]:
                raise RuntimeError("link volume binding changed")
            buffer, size = c.create_string_buffer(16384), c.c_uint32()
            if not self.kernel.DeviceIoControl(
                    handle, 0x900A8, None, 0, buffer, len(buffer), c.byref(size), None):
                raise c.WinError(c.get_last_error())
            if size.value > len(buffer):
                raise RuntimeError("invalid reparse read length")
            _symlink_target(buffer.raw[:size.value], target)
            if self.raw_info(handle) != before or not parent.unchanged():
                raise RuntimeError("link binding changed during readback")
        finally:
            self.close(handle)

    def create_link(self, parent, name, target, directory):
        # All names/targets are fixed by the caller, never read from the request.
        if (name, target, directory) not in (("alias", "normal", True), ("alias-file", "payload", False)):
            raise ValueError("unapproved link creation")
        if not parent.unchanged():
            raise RuntimeError("preparation parent binding changed")
        self.absent(parent, name)
        pathname = self.final_path(parent.handle).rstrip("\\") + "\\" + name
        if not self.kernel.CreateSymbolicLinkW(pathname, target, 1 if directory else 0):
            raise c.WinError(c.get_last_error())
        self.validate_link(parent, name, target, directory)

    def launch(self, request: Path, provenance):
        main = _PROJECT / "scripts" / _SOURCES[0]
        arguments = ["-E", "-s", "-B", "-X", "utf8", str(main),
                     "--internal-prepare-path-links", str(request)]
        execution = _ShellExecuteInfo()
        execution.cbSize, execution.fMask = c.sizeof(execution), 0x40 | 0x100 | 0x400
        execution.lpVerb, execution.lpFile = "runas", str(_PYTHON)
        execution.lpParameters = subprocess.list2cmdline(arguments)
        execution.lpDirectory, execution.nShow = str(_PROJECT / "scripts"), 0
        provenance["launched_at"] = _utc()
        if not self.shell.ShellExecuteExW(c.byref(execution)):
            raise c.WinError(c.get_last_error())
        if not execution.hProcess:
            raise RuntimeError("helper launch returned no process handle")
        try:
            pid = self.kernel.GetProcessId(execution.hProcess)
            provenance["helper_pid"] = pid
            # Pins live in the normal main until this exact child has terminated.
            wait_failed = False
            while self.kernel.WaitForSingleObject(execution.hProcess, 0xFFFFFFFF) != 0:
                # A failed wait is never success and never releases live-child pins.
                wait_failed = True
                time.sleep(1)
            code = c.c_uint32()
            if not self.kernel.GetExitCodeProcess(execution.hProcess, c.byref(code)):
                raise c.WinError(c.get_last_error())
            provenance["exit_code"] = code.value
            provenance["completed_at"] = _utc()
            if not pid or pid == os.getpid() or wait_failed:
                raise RuntimeError("helper process identity or wait is unavailable")
            if code.value != 0:
                raise RuntimeError("link helper refused or partially prepared the fixture")
        finally:
            self.close(execution.hProcess)


def _case_child(child):
    path = PureWindowsPath(str(child))
    if (len(path.parts) != len(_CASE_ROOT.parts) + 2 or path.parent.parent != _CASE_ROOT
            or not _CASE.fullmatch(path.parent.name) or not _CHILD.fullmatch(path.name)):
        raise ValueError("only a fresh fixed storage-probe child is allowed")
    return path


def _pin_chain(api, stack, path, *, directory=True, preparation=True):
    path = PureWindowsPath(str(path))
    if not path.is_absolute() or path.drive not in ("C:", "D:") or path.root != "\\":
        raise ValueError("a fixed local absolute path is required")
    guid = api.volume_guid(path.anchor)
    # Path on Python 3.10 strips the root separator and denotes a volume device.
    root = stack.enter_context(_Pinned(api, api.open_root(guid, preparation=preparation), True))
    pins = [root]
    expected = guid.rstrip("\\")
    for index, name in enumerate(path.parts[1:]):
        _component(name)
        is_directory = directory or index != len(path.parts) - 2
        handle = api.open_relative(pins[-1].handle, name,
                                   preparation=preparation and is_directory)
        pin = stack.enter_context(_Pinned(api, handle, is_directory))
        expected += "\\" + name
        if api.final_path(pin.handle).casefold().rstrip("\\") != expected.casefold():
            raise RuntimeError("handle pathname does not match its pinned chain")
        if pin.identity[0] != root.identity[0]:
            raise RuntimeError("ancestor crosses a volume boundary")
        pins.append(pin)
    return pins


def _check_pins(pins):
    if not all(pin.unchanged() for pin in pins):
        raise RuntimeError("preparation ancestor identity changed")


def _source_pins(api, stack):
    pins, records = [], {}
    for label, path in [("python.exe", _PYTHON)] + [
            (name, _PROJECT / "scripts" / name) for name in _SOURCES]:
        chain = _pin_chain(api, stack, path, directory=False)
        pins.extend(chain)
        records[label] = {"sha256": hashlib.sha256(api.contents(chain[-1].handle)).hexdigest(),
                          "binding": _record(chain[-1])}
    return pins, records


def _fixture_pins(api, stack, child, *, preparation=True):
    chain = _pin_chain(api, stack, child / "path-binding" / "normal", preparation=preparation)
    fixture, normal = chain[-2:]
    payload = stack.enter_context(normal.child("payload", directory=False))
    if payload.identity[0] != normal.identity[0] or api.contents(payload.handle) != _PAYLOAD:
        raise RuntimeError("payload binding or bytes changed")
    records = [_record(pin) for pin in chain] + [_record(payload)]
    return chain + [payload], fixture, normal, payload, records


def _validate_request(request, child, nonce, records, sources, guid):
    keys = {"version", "mode", "nonce", "state", "parent_pid", "child",
            "bindings", "sources", "volume_guid"}
    if (type(request) is not dict or set(request) != keys
            or request["version"] != 1 or request["mode"] != _MODE
            or request["state"] != "pending" or request["nonce"] != nonce
            or type(request["parent_pid"]) is not int or request["parent_pid"] <= 0
            or request["child"] != str(child) or request["bindings"] != records
            or request["sources"] != sources or request["volume_guid"] != guid):
        raise ValueError("request/source/fixture binding mismatch")


def _validate_provenance(provenance, request, request_bytes):
    if (provenance.get("mode") != _MODE or provenance.get("nonce") != request["nonce"]
            or provenance.get("parent_pid") != os.getpid()
            or type(provenance.get("helper_pid")) is not int
            or provenance["helper_pid"] <= 0 or provenance["helper_pid"] == os.getpid()
            or type(provenance.get("exit_code")) is not int or provenance["exit_code"] != 0
            or provenance.get("sources") != request["sources"]
            or provenance.get("child") != request["child"]
            or provenance.get("volume_guid") != request["volume_guid"]
            or provenance.get("request_sha256") != hashlib.sha256(request_bytes).hexdigest()
            or provenance.get("evidence_kind") != "helper-internal-full-token-check"):
        raise ValueError("missing or inconsistent helper provenance")
    launched = datetime.fromisoformat(provenance.get("launched_at", ""))
    completed = datetime.fromisoformat(provenance.get("completed_at", ""))
    if launched.utcoffset() is None or completed.utcoffset() is None or completed < launched:
        raise ValueError("invalid helper provenance times")


def prepare_path_links(request_path: Path) -> int:
    """Internal helper: no report, directories, ACL changes or ordinary collectors."""
    try:
        if os.name != "nt":
            raise RuntimeError("native Windows is required")
        api = _PreparationNative()
        api.require_full_token()  # Before path reads and both writes.
        path = PureWindowsPath(str(request_path))
        match = re.fullmatch(r"link-request-([0-9a-f]{64})\.json", path.name)
        if match is None:
            raise ValueError("invalid preparation request name")
        child, nonce = _case_child(path.parent.parent), match.group(1)
        if path.parent != child / "path-binding":
            raise ValueError("invalid request parent")
        with ExitStack() as stack:
            request_chain = _pin_chain(api, stack, path, directory=False)
            request_pin = request_chain[-1]
            request = json.loads(api.contents(request_pin.handle, 16384).decode("utf-8"))
            source_handles, sources = _source_pins(api, stack)
            pins, fixture, normal, payload, records = _fixture_pins(api, stack, child)
            guid = api.volume_guid(child.anchor)
            _validate_request(request, child, nonce, records, sources, guid)
            all_pins = request_chain + source_handles + pins
            _check_pins(all_pins)
            api.absent(fixture, "alias")
            api.absent(normal, "alias-file")
            for parent, name, target, directory in (
                    (fixture, "alias", "normal", True), (normal, "alias-file", "payload", False)):
                _check_pins(all_pins)
                api.create_link(parent, name, target, directory)
            _check_pins(all_pins)
            if api.contents(payload.handle) != _PAYLOAD:
                raise RuntimeError("payload changed after preparation")
        return 0
    except (OSError, RuntimeError, ValueError, UnicodeError):
        return 20


def require_normal_collector():
    _PreparationNative().require_normal()


def _verify_fixture(api, root, normal_path, payload, observations):
    with root.child("normal", directory=True) as directory:
        with directory.child("payload", directory=False) as first:
            with directory.child("payload", directory=False) as reopened:
                observations["reopen_same_file"] = first.identity == reopened.identity
                observations["volume_serial"] = first.identity[0]
                observations["directory_link_rejected"] = _rejects_link(root, "alias", directory=True)
                observations["file_link_rejected"] = _rejects_link(directory, "alias-file", directory=False)
                try:
                    normal_path.rename(normal_path.parent / "moved")
                except OSError as exc:
                    if getattr(exc, "winerror", None) != 32:
                        raise
                    observations["replacement_blocked"] = True
                else:
                    observations["replacement_blocked"] = False
                observations["bytes_unchanged"] = api.read(first.handle, len(payload) + 1) == payload
                observations["pinned_identity_unchanged"] = all(
                    handle.unchanged() for handle in (root, directory, first, reopened))
    keys = ("reopen_same_file", "directory_link_rejected", "file_link_rejected",
            "replacement_blocked", "pinned_identity_unchanged", "bytes_unchanged")
    return all(observations[name] for name in keys)


def _probe_paths_with_uac(child):
    observations = {"preparation_mode": _MODE}
    try:
        child = _case_child(child)
        api = _PreparationNative()
        api.require_normal()
        fixture_path, normal_path = Path(child / "path-binding"), Path(child / "path-binding" / "normal")
        fixture_path.mkdir()
        normal_path.mkdir()
        with (normal_path / "payload").open("xb") as stream:
            stream.write(_PAYLOAD)
            stream.flush()
            os.fsync(stream.fileno())
        with ExitStack() as stack:
            pins, fixture, normal, _, records = _fixture_pins(api, stack, child)
            source_handles, sources = _source_pins(api, stack)
            api.absent(fixture, "alias")
            api.absent(normal, "alias-file")
            nonce = secrets.token_hex(32)
            request = {"version": 1, "mode": _MODE, "nonce": nonce, "state": "pending",
                       "parent_pid": os.getpid(), "child": str(child), "bindings": records,
                       "sources": sources, "volume_guid": api.volume_guid(child.anchor)}
            request_bytes = (json.dumps(request, sort_keys=True) + "\n").encode("utf-8")
            request_path = fixture_path / ("link-request-" + nonce + ".json")
            with request_path.open("xb") as stream:
                stream.write(request_bytes)
                stream.flush()
                os.fsync(stream.fileno())
            request_pin = stack.enter_context(fixture.child(request_path.name, directory=False))
            if api.contents(request_pin.handle) != request_bytes:
                raise RuntimeError("request readback changed")
            provenance = {"mode": _MODE, "nonce": nonce, "parent_pid": os.getpid(),
                          "request_sha256": hashlib.sha256(request_bytes).hexdigest(),
                          "request_binding": _record(request_pin), "sources": sources,
                          "child": str(child), "volume_guid": request["volume_guid"],
                          "evidence_kind": "helper-internal-full-token-check"}
            observations["helper"] = provenance
            all_pins = pins + source_handles + [request_pin]
            _check_pins(all_pins)
            api.launch(request_path, provenance)
            _validate_provenance(provenance, request, request_bytes)
            _check_pins(all_pins)
            if api.contents(request_pin.handle) != request_bytes:
                raise RuntimeError("request changed while preparing links")
            api.validate_link(fixture, "alias", "normal", True)
            api.validate_link(normal, "alias-file", "payload", False)
            # Strict pins overlap preparation pins throughout the handover.
            strict, strict_fixture, _, _, strict_records = _fixture_pins(
                api, stack, child, preparation=False)
            if strict_records != records:
                raise RuntimeError("strict handover changed fixture binding")
            _check_pins(all_pins + strict)
            observations["helper_bound"] = True
            return _verify_fixture(api, strict_fixture, normal_path, _PAYLOAD, observations), observations
    except (OSError, RuntimeError, ValueError, UnicodeError) as exc:
        observations["error"] = str(exc)
        result = False if any(value is False for value in observations.values()) else None
        return result, observations
