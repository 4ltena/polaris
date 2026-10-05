#!/usr/bin/env python3
"""Conservatively probe Windows filesystem prerequisites in a disposable child."""

from __future__ import annotations

import argparse
import ctypes
import json
import os
import sys
import uuid
from pathlib import Path


REQUIRED = (
    "acl_filesystem",
    "exclusive_create",
    "no_clobber_rename",
    "file_flush_reopen",
    "protected_private_dacl",
    "handle_relative_no_reparse",
    "volume_identity_and_cross_volume_refusal",
    "cross_process_exclusive_lease",
    "process_termination_recovery",
)


def classify(checks: dict[str, bool | None]) -> str:
    if set(checks) != set(REQUIRED):
        raise ValueError("incomplete filesystem probe result")
    if any(value is False for value in checks.values()):
        return "B:unsafe_or_unsupported"
    if any(value is None for value in checks.values()):
        return "C:inconclusive"
    return "A:supported"


def self_test() -> None:
    good = dict.fromkeys(REQUIRED, True)
    assert classify(good) == "A:supported"
    unknown = dict(good)
    unknown["handle_relative_no_reparse"] = None
    assert classify(unknown) == "C:inconclusive"
    bad = dict(unknown)
    bad["no_clobber_rename"] = False
    assert classify(bad) == "B:unsafe_or_unsupported"
    try:
        classify({"exclusive_create": True})
    except ValueError:
        pass
    else:
        raise AssertionError("incomplete probe result was accepted")


def volume_info(path: Path) -> tuple[str | None, int | None]:
    volume_path = ctypes.create_unicode_buffer(260)
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    if not kernel32.GetVolumePathNameW(str(path), volume_path, len(volume_path)):
        return None, None
    filesystem = ctypes.create_unicode_buffer(260)
    serial = ctypes.c_ulong()
    if not kernel32.GetVolumeInformationW(
        volume_path, None, 0, ctypes.byref(serial), None, None,
        filesystem, len(filesystem),
    ):
        return None, None
    return filesystem.value, serial.value


def probe(root: Path) -> dict[str, object]:
    if os.name != "nt":
        raise RuntimeError("native Windows is required")
    root = root.resolve(strict=True)
    if not root.is_dir() or root.is_symlink():
        raise ValueError("the explicit probe root must be a normal directory")
    child = root / f"polaris-windows-fs-probe-{uuid.uuid4().hex}"
    child.mkdir()
    checks: dict[str, bool | None] = dict.fromkeys(REQUIRED)
    observations: dict[str, object] = {
        "probe_child": str(child),
        "retained_for_review": True,
    }

    filesystem, serial = volume_info(child)
    observations["filesystem"] = filesystem
    observations["volume_serial"] = serial
    checks["acl_filesystem"] = True if filesystem == "NTFS" else None

    existing = child / "existing"
    staged = child / "staged"
    before = b"polaris-existing\r\n"
    staged_bytes = b"polaris-staged\r\n"
    try:
        with existing.open("xb") as stream:
            stream.write(before)
            stream.flush()
            os.fsync(stream.fileno())
        with staged.open("xb") as stream:
            stream.write(staged_bytes)
            stream.flush()
            os.fsync(stream.fileno())
        checks["file_flush_reopen"] = (
            existing.read_bytes() == before and staged.read_bytes() == staged_bytes
        )
    except OSError as exc:
        observations["file_flush_error"] = str(exc)
    try:
        with existing.open("xb"):
            pass
    except FileExistsError:
        checks["exclusive_create"] = existing.read_bytes() == before
    except OSError as exc:
        observations["exclusive_create_error"] = str(exc)
    else:
        checks["exclusive_create"] = False

    try:
        staged.rename(existing)
    except FileExistsError:
        checks["no_clobber_rename"] = (
            existing.read_bytes() == before and staged.read_bytes() == staged_bytes
        )
    except OSError as exc:
        observations["rename_error"] = str(exc)
    else:
        checks["no_clobber_rename"] = False

    # The remaining properties require a native owner and process-kill tests.
    # Reporting them as unknown prevents this preliminary probe from enabling M2.
    result: dict[str, object] = {
        "classification": classify(checks),
        "checks": checks,
        "observations": observations,
    }
    report = child / "report.json"
    with report.open("x", encoding="utf-8") as stream:
        json.dump(result, stream, indent=2, sort_keys=True)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--root", type=Path)
    args = parser.parse_args()
    try:
        if args.self_test:
            self_test()
            print("self-test passed: A, B, C and incomplete-result rejection")
            if args.root is None:
                return 0
        if args.root is None:
            parser.error("--root is required for an actual probe")
        result = probe(args.root)
        print(json.dumps(result, indent=2, sort_keys=True))
    except (OSError, ValueError, RuntimeError, AssertionError) as exc:
        parser.exit(1, f"probe failed: {exc}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
