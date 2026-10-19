"""No-clobber publication and cross-volume refusal in disposable probe fixtures."""

from __future__ import annotations

from contextlib import ExitStack
import ctypes as c
import os
from pathlib import Path

from windows_probe_paths import _IoStatus, _Native, _Pinned, _component


class _RenameOptions(c.Union):
    _fields_ = [("ReplaceIfExists", c.c_ubyte), ("Flags", c.c_uint32)]


class _RenameInfo(c.Structure):
    _anonymous_ = ("Options",)
    _fields_ = [("Options", _RenameOptions), ("RootDirectory", c.c_void_p),
                ("FileNameLength", c.c_uint32), ("FileName", c.c_uint16 * 1)]


class CrossVolumeRefused(RuntimeError):
    pass


class UnsafeProbe(RuntimeError):
    pass


class _VolumeNative(_Native):
    def __init__(self):
        super().__init__()
        self.nt.NtSetInformationFile.argtypes = [c.c_void_p, c.POINTER(_IoStatus),
                                                 c.c_void_p, c.c_uint32, c.c_int]
        self.nt.NtSetInformationFile.restype = c.c_int32
        self.kernel.SetFilePointerEx.argtypes = [c.c_void_p, c.c_int64,
                                               c.POINTER(c.c_int64), c.c_uint32]
        self.kernel.SetFilePointerEx.restype = c.c_int

    def open_publication_root(self, path: Path):
        # The rename's internal target-directory open requests WRITE_DATA.
        # TRAVERSE | READ_ATTRIBUTES | SYNCHRONIZE; share READ/WRITE, never DELETE.
        # This fixture-only pin still prevents directory replacement. Comparison
        # roots retain the unchanged read-only open_root/open_relative policy.
        handle = self.kernel.CreateFileW(str(path), 0x1000A0, 3, None, 3,
                                         0x02000000 | 0x00200000, None)
        if handle in (None, c.c_void_p(-1).value):
            raise c.WinError(c.get_last_error())
        return handle

    def rename(self, source, destination, name: str):
        _component(name)
        encoded = name.encode("utf-16-le")
        # FILE_RENAME_INFORMATION includes natural HANDLE alignment. Reserve the
        # documented sizeof(struct) + name bytes, with zeroed trailing space.
        size = c.sizeof(_RenameInfo) + len(encoded)
        alignment = c.sizeof(c.c_void_p)
        buffer = (c.c_void_p * ((size + alignment - 1) // alignment))()
        header = _RenameInfo.from_buffer(buffer)
        header.ReplaceIfExists = 0
        header.RootDirectory = destination
        header.FileNameLength = len(encoded)
        c.memmove(c.addressof(buffer) + _RenameInfo.FileName.offset, encoded, len(encoded))
        status = _IoStatus()
        # FileRenameInformation = 10. The source was opened synchronously with
        # DELETE access; only final STATUS_SUCCESS establishes completion.
        result = self.nt.NtSetInformationFile(source, c.byref(status), buffer, size, 10)
        if result != 0:
            raise c.WinError(self.nt.RtlNtStatusToDosError(result))

    def read_start(self, handle, limit: int) -> bytes:
        if not self.kernel.SetFilePointerEx(handle, 0, None, 0):
            raise c.WinError(c.get_last_error())
        return self.read(handle, limit)


def _publish(source, recovery, destination, name: str, rename) -> None:
    _component(name)
    if source.directory or not recovery.directory or not destination.directory:
        raise RuntimeError("invalid publication handle type")
    if not all(pin.unchanged() for pin in (source, recovery, destination)):
        raise RuntimeError("publication binding changed")
    if not source.identity[0] == recovery.identity[0] == destination.identity[0]:
        raise CrossVolumeRefused("publication crosses a pinned volume boundary")
    rename(source.handle, destination.handle, name)
    if not all(pin.unchanged() for pin in (source, recovery, destination)):
        raise UnsafeProbe("publication changed a pinned identity")


def _forbid_comparison_write(*_):
    # Independent of the volume guard: a broken guard must never write there.
    raise UnsafeProbe("native rename was attempted for the comparison root")


def _comparison_pin(stack: ExitStack, api, path: Path):
    # Keep the existing strict-resolution gate, then traverse the ORIGINAL input
    # by pinned components, so canonicalization cannot hide a junction/symlink.
    path.resolve(strict=True)
    absolute = path.absolute()
    if len(absolute.drive) != 2 or absolute.drive[1] != ":" or not absolute.is_absolute():
        raise ValueError("comparison root must be an explicit local drive directory")
    pin = stack.enter_context(_Pinned(api, api.open_root(Path(absolute.anchor)), True))
    for name in absolute.parts[1:]:
        pin = stack.enter_context(pin.child(name, directory=True))
    return pin


def _compare_volume(api, source, recovery, comparison: Path | None, observations) -> bool | None:
    if comparison is None:
        observations["comparison"] = "not supplied"
        return None
    try:
        with ExitStack() as stack:
            destination = _comparison_pin(stack, api, comparison)
            observations["comparison_volume_serial"] = destination.identity[0]
            if source.identity[0] == destination.identity[0]:
                observations["comparison"] = "same volume; cross-volume case not exercised"
                return None
            try:
                _publish(source, recovery, destination, "cross-volume-refusal", _forbid_comparison_write)
            except CrossVolumeRefused:
                if not all(pin.unchanged() for pin in (source, recovery, destination)):
                    raise UnsafeProbe("identity changed during cross-volume refusal")
                observations["cross_volume_refused"] = True
                return True
            raise UnsafeProbe("cross-volume publication was not refused")
    except UnsafeProbe:
        raise
    except (OSError, RuntimeError, ValueError) as exc:
        observations["comparison_error"] = str(exc)
        return None


def _require(condition: bool, message: str):
    if not condition:
        raise UnsafeProbe(message)


def probe_volume(child: Path, comparison: Path | None) -> tuple[bool | None, dict[str, object]]:
    """Only the disposable child is writable; comparison is metadata-only."""
    if os.name != "nt":
        raise RuntimeError("native Windows is required")
    fixture = child / "publication"
    staged_bytes, existing_bytes = b"polaris-staged-publication\r\n", b"polaris-existing-publication\r\n"
    observations: dict[str, object] = {}
    try:
        fixture.mkdir()
        (fixture / "recovery").mkdir()
        for name, data in (("stage", staged_bytes), ("occupied", existing_bytes)):
            with (fixture / name).open("xb") as stream:
                stream.write(data)
                stream.flush()
                os.fsync(stream.fileno())
        api = _VolumeNative()
        with _Pinned(api, api.open_publication_root(fixture), True) as root:
            with root.child("recovery", directory=True) as recovery:
                with root.child("occupied", directory=False) as existing:
                    with root.child("stage", directory=False, delete_access=True) as source:
                        before = source.identity
                        observations["source_volume_serial"] = before[0]
                        # This callback can publish only into the owned fixture.
                        def local_rename(source_handle, destination_handle, name):
                            if source_handle != source.handle or destination_handle != root.handle:
                                raise UnsafeProbe("publication escaped the disposable fixture")
                            api.rename(source_handle, destination_handle, name)
                        try:
                            _publish(source, recovery, root, "occupied", local_rename)
                        except OSError as exc:
                            if getattr(exc, "winerror", None) not in (80, 183):
                                raise
                        else:
                            raise UnsafeProbe("no-clobber collision unexpectedly succeeded")
                        _require(source.unchanged() and existing.unchanged(), "collision changed identity")
                        _require(api.read_start(source.handle, len(staged_bytes) + 1) == staged_bytes,
                                 "collision changed staged bytes")
                        _require(api.read_start(existing.handle, len(existing_bytes) + 1) == existing_bytes,
                                 "collision changed destination bytes")
                        observations["collision_preserved"] = True
                        compared = _compare_volume(api, source, recovery, comparison, observations)
                        _require(api.read_start(source.handle, len(staged_bytes) + 1) == staged_bytes,
                                 "cross-volume refusal changed source bytes")
                        _publish(source, recovery, root, "published", local_rename)
                    # Release the DELETE-access source handle before reopening it;
                    # the new read-only pin deliberately does not share DELETE.
                    try:
                        published_pin = root.child("published", directory=False)
                    except FileNotFoundError as exc:
                        raise UnsafeProbe("publication did not create the destination") from exc
                    with published_pin as published:
                        _require(published.identity == before, "published file identity changed")
                        _require(api.read_start(published.handle, len(staged_bytes) + 1) == staged_bytes,
                                 "published bytes changed")
                        _require(api.read_start(existing.handle, len(existing_bytes) + 1) == existing_bytes,
                                 "publication changed the existing destination")
                        try:
                            with root.child("stage", directory=False):
                                raise UnsafeProbe("source name remained after publication")
                        except FileNotFoundError:
                            pass
                        _require(all(pin.unchanged() for pin in (root, recovery, existing, published)),
                                 "publication readback changed identity")
                        observations["same_volume_publication"] = True
        return compared, observations
    except UnsafeProbe as exc:
        observations["error"] = str(exc)
        return False, observations
    except (OSError, RuntimeError, ValueError) as exc:
        observations["error"] = str(exc)
        return None, observations
