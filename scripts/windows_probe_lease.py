"""Disposable LockFileEx capability check; never a product recovery owner."""

from __future__ import annotations

import ctypes
from ctypes import wintypes
import multiprocessing
import os
from pathlib import Path
import time


WAIT_SECONDS = 5
ERROR_LOCK_VIOLATION = 33
LEASE_BYTES = b"polaris-disposable-lease\r\n"


class _Overlapped(ctypes.Structure):
    # The zeroed Offset/OffsetHigh pair occupies OVERLAPPED's offset union.
    _fields_ = [
        ("Internal", ctypes.c_size_t), ("InternalHigh", ctypes.c_size_t),
        ("Offset", wintypes.DWORD), ("OffsetHigh", wintypes.DWORD),
        ("hEvent", wintypes.HANDLE),
    ]


class _Lock:
    def __init__(self, stream):
        import msvcrt

        self.handle = wintypes.HANDLE(msvcrt.get_osfhandle(stream.fileno()))
        self.overlapped = _Overlapped()
        self.api = ctypes.WinDLL("kernel32", use_last_error=True)
        self.api.LockFileEx.argtypes = [
            wintypes.HANDLE, wintypes.DWORD, wintypes.DWORD,
            wintypes.DWORD, wintypes.DWORD, ctypes.POINTER(_Overlapped),
        ]
        self.api.LockFileEx.restype = wintypes.BOOL
        self.api.UnlockFileEx.argtypes = [
            wintypes.HANDLE, wintypes.DWORD, wintypes.DWORD,
            wintypes.DWORD, ctypes.POINTER(_Overlapped),
        ]
        self.api.UnlockFileEx.restype = wintypes.BOOL

    def acquire(self) -> bool:
        # EXCLUSIVE | FAIL_IMMEDIATELY, byte [0, 1). Never wait in the OS call.
        if self.api.LockFileEx(self.handle, 3, 0, 1, 0, ctypes.byref(self.overlapped)):
            return True
        error = ctypes.get_last_error()
        if error == ERROR_LOCK_VIOLATION:
            return False
        raise ctypes.WinError(error)

    def release(self) -> None:
        if not self.api.UnlockFileEx(self.handle, 0, 1, 0, ctypes.byref(self.overlapped)):
            raise ctypes.WinError(ctypes.get_last_error())


def _identity(stream) -> tuple[int, int]:
    stat = os.fstat(stream.fileno())
    if not stat.st_ino:
        raise RuntimeError("lease file identity is unavailable")
    return stat.st_dev, stat.st_ino


def _worker(path: str, identity: tuple[int, int], connection, hold: bool,
            retry_seconds: float) -> None:
    try:
        with open(path, "r+b", buffering=0) as stream:
            if _identity(stream) != identity:
                raise RuntimeError("lease file identity changed")
            lock = _Lock(stream)
            deadline = time.monotonic() + retry_seconds
            while not lock.acquire():
                if time.monotonic() >= deadline:
                    connection.send({"state": "blocked"})
                    return
                time.sleep(0.05)
            try:
                if hold:
                    connection.send({"state": "locked"})
                    if connection.recv() != "release":
                        raise RuntimeError("unexpected lease command")
            finally:
                lock.release()
            connection.send({"state": "released" if hold else "acquired"})
    except (OSError, RuntimeError, EOFError) as exc:
        connection.send({"state": "error", "kind": type(exc).__name__,
                         "winerror": getattr(exc, "winerror", None)})
    finally:
        connection.close()


class _Child:
    def __init__(self, path: Path, identity: tuple[int, int], *, hold: bool = False,
                 retry_seconds: float = 0):
        context = multiprocessing.get_context("spawn")
        self.connection, self.remote = context.Pipe()
        self.process = context.Process(
            target=_worker, args=(str(path), identity, self.remote, hold, retry_seconds),
        )

    def __enter__(self):
        try:
            self.process.start()
        except BaseException:
            self.connection.close()
            self.remote.close()
            self.process.close()
            raise
        self.remote.close()
        return self

    def message(self) -> str:
        if not self.connection.poll(WAIT_SECONDS):
            raise RuntimeError("lease child response timed out")
        message = self.connection.recv()
        if not isinstance(message, dict) or message.get("state") not in {
            "locked", "released", "acquired", "blocked",
        }:
            raise RuntimeError(f"lease child did not complete: {message!r}")
        return message["state"]

    def join(self, *, terminated: bool = False) -> None:
        self.process.join(WAIT_SECONDS)
        if self.process.is_alive():
            raise RuntimeError("lease child exit timed out")
        if (self.process.exitcode == 0) == terminated:
            raise RuntimeError("unexpected lease child exit status")

    def __exit__(self, *_):
        try:
            if self.process.is_alive():
                self.process.terminate()
            self.process.join(WAIT_SECONDS)
            if self.process.is_alive():
                raise RuntimeError("lease child cleanup timed out")
        finally:
            self.connection.close()
            if not self.process.is_alive():
                self.process.close()


def _attempt(path: Path, identity: tuple[int, int], *, retry_seconds: float = 0) -> str:
    with _Child(path, identity, retry_seconds=retry_seconds) as child:
        state = child.message()
        child.join()
    if state not in {"acquired", "blocked"}:
        raise RuntimeError("unexpected lease attempt state")
    return state


def _round(path: Path, identity: tuple[int, int], *, terminate: bool) -> bool:
    with _Child(path, identity, hold=True) as owner:
        if owner.message() != "locked":
            raise RuntimeError("lease owner did not acquire the lock")
        if _attempt(path, identity) != "blocked":
            return False
        # Verify that contention was observed while the owner was still alive.
        if not owner.process.is_alive():
            raise RuntimeError("lease owner exited before contention completed")
        if terminate:
            # Only the Process object created above can be terminated here.
            owner.process.terminate()
            owner.join(terminated=True)
        else:
            owner.connection.send("release")
            if owner.message() != "released":
                raise RuntimeError("lease owner did not release the lock")
            owner.join()
    # Windows may delay releasing locks after termination. Bound the retry.
    return _attempt(path, identity, retry_seconds=2) == "acquired"


def probe_lease(child: Path) -> tuple[bool | None, dict[str, object]]:
    """Called only after the main probe validates its root and creates its child."""
    if os.name != "nt":
        raise RuntimeError("native Windows is required")
    path = child / "exclusive-lease"
    observations: dict[str, object] = {}
    result: bool | None = None
    created = False
    identity = None
    try:
        with path.open("xb") as stream:
            created = True
            identity = _identity(stream)
            stream.write(LEASE_BYTES)
            stream.flush()
            os.fsync(stream.fileno())
        for name, terminate in (("normal_release", False), ("owner_termination", True)):
            passed = _round(path, identity, terminate=terminate)
            observations[name] = passed
            if not passed:
                result = False
                break
        else:
            result = True
    except (OSError, RuntimeError, EOFError) as exc:
        observations["error"] = str(exc)
    if created and identity is not None:
        try:
            with path.open("rb") as stream:
                unchanged = _identity(stream) == identity and stream.read() == LEASE_BYTES
            observations["unchanged"] = unchanged
            if not unchanged:
                result = False
        except (OSError, RuntimeError) as exc:
            observations["readback_error"] = str(exc)
            if result is not False:
                result = None
    return result, observations
