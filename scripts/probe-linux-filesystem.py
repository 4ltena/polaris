#!/usr/bin/env python3
"""Probe Linux recovery filesystem capabilities in one owned disposable child.

The USB run requires a separately saved host-channel preflight record. It never
mounts a filesystem, opens a project child, or treats this process's mount view
alone as proof of the host mount namespace.
"""

from __future__ import annotations

import argparse
import ctypes
import errno
import hashlib
import json
import os
import re
import stat
import subprocess
import sys
import tempfile
from pathlib import Path


PARENT = "/run/media/example/USBSSD/projects/codex"
MOUNTPOINT = "/run/media/example/USBSSD"
UUID = "FBB5-BBFC"
FINDMNT = ["/usr/bin/findmnt", "--json", "--target", PARENT,
           "--output", "SOURCE,UUID,FSTYPE,TARGET,OPTIONS,MAJ:MIN"]
LSBLK = ["/usr/bin/lsblk", "--json", "--paths", "--output",
         "PATH,LABEL,UUID,MAJ:MIN"]
DIR_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
FILE_FLAGS = os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC
SOURCE_BYTES = b"polaris-linux-probe-source-v1\n"
COLLISION_BYTES = b"polaris-linux-probe-collision-v1\n"
MAX_FIXTURE_BYTES = 64 * 1024
assert len(SOURCE_BYTES) + len(COLLISION_BYTES) <= MAX_FIXTURE_BYTES
CLASS_A = "A:capabilities_and_privacy_pass"
CLASS_P = "P:capabilities_pass_privacy_blocked_by_current_mask"
CLASS_B = "B:native_filesystem_required"
CLASS_C = "C:inconclusive"
CAPABILITY_SYSCALLS = {
    "exclusive_child": "mkdirat",
    "no_clobber_collision": "renameat2(RENAME_NOREPLACE)",
    "no_clobber_success": "renameat2(RENAME_NOREPLACE)",
    "directory_fsync": "fsync(directory fd)",
    "file_mode": "fchmod/fstat(file fd)",
    "directory_mode": "fchmod/fstat(directory fd)",
    "same_device": "fstat(open fds)",
}
_LIBC = ctypes.CDLL(None, use_errno=True)
_RENAME_NOREPLACE = 1


class ProbeRejected(ValueError):
    """A preflight or fixture invariant failed; no unsafe cleanup is permitted."""


class ChildCollision(ProbeRejected):
    """The random child name already exists and must remain untouched."""


class CreatedChildUnverified(ProbeRejected):
    """A child was created, but its identity could not be safely proved."""


def capability(name: str, result: str = "not_run", error: int | None = None,
               detail: str = "") -> dict[str, object]:
    return {"syscall": CAPABILITY_SYSCALLS[name], "result": result,
            "errno": error, "detail": detail}


def identity(info: os.stat_result) -> dict[str, object]:
    return {"device": info.st_dev, "inode": info.st_ino,
            "owner": info.st_uid, "mode": f"{stat.S_IMODE(info.st_mode):04o}"}


def _same_identity(fd: int, saved: os.stat_result, owner: int) -> bool:
    now = os.fstat(fd)
    return (stat.S_ISDIR(now.st_mode) and now.st_dev == saved.st_dev
            and now.st_ino == saved.st_ino and now.st_uid == owner)


def open_component_walk(path: str, owner: int) -> tuple[int, os.stat_result]:
    """Open absolute components from / without following any symlink."""
    if not path.startswith("/") or os.path.normpath(path) != path or path == "/":
        raise ProbeRejected("parent must be a normalized absolute directory")
    fd = os.open("/", DIR_FLAGS)
    try:
        for component in path.split("/")[1:]:
            if not component or component in (".", ".."):
                raise ProbeRejected("invalid parent component")
            next_fd = os.open(component, DIR_FLAGS, dir_fd=fd)
            os.close(fd)
            fd = next_fd
        info = os.fstat(fd)
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != owner:
            raise ProbeRejected("parent is not a directory owned by effective uid")
        return fd, info
    except BaseException:
        os.close(fd)
        raise


def _command_json(argv: list[str]) -> dict[str, object]:
    completed = subprocess.run(argv, check=True, capture_output=True,
                               timeout=10, env={"LC_ALL": "C"})
    if len(completed.stdout) > 64 * 1024:
        raise ProbeRejected("mount command output exceeded 64 KiB")
    parsed = json.loads(completed.stdout)
    if not isinstance(parsed, dict):
        raise ProbeRejected("mount command did not return a JSON object")
    return parsed


def parse_findmnt(data: dict[str, object]) -> dict[str, str | None]:
    if not isinstance(data, dict):
        raise ProbeRejected("findmnt output is not an object")
    rows = data.get("filesystems")
    if not isinstance(rows, list) or len(rows) != 1 or not isinstance(rows[0], dict):
        raise ProbeRejected("findmnt must return exactly one filesystem")
    row = rows[0]
    mount = {key: row.get(key) for key in
             ("source", "uuid", "fstype", "target", "options", "maj:min")}
    if any(not isinstance(value, str) or not value for value in mount.values()):
        raise ProbeRejected("findmnt identity is incomplete")
    return {"source": mount["source"], "uuid": mount["uuid"],
            "fstype": mount["fstype"], "target": mount["target"],
            "options": mount["options"], "maj_min": mount["maj:min"],
            "label": None}


def _block_rows(data: dict[str, object]) -> list[dict[str, object]]:
    roots = data.get("blockdevices")
    if not isinstance(roots, list):
        raise ProbeRejected("lsblk blockdevices is absent")
    rows = []

    def visit(item: object) -> None:
        if not isinstance(item, dict):
            raise ProbeRejected("lsblk record is invalid")
        rows.append(item)
        children = item.get("children", [])
        if not isinstance(children, list):
            raise ProbeRejected("lsblk children are invalid")
        for child in children:
            visit(child)

    for root in roots:
        visit(root)
    return rows


def verify_device(mount: dict[str, str | None], lsblk: dict[str, object],
                  expected_label: str, device_stat: os.stat_result) -> str | None:
    source, uuid, maj_min = (mount[key] for key in ("source", "uuid", "maj_min"))
    if not source or not source.startswith("/dev/") or not uuid or not maj_min:
        raise ProbeRejected("source, UUID or major:minor is missing")
    if not stat.S_ISBLK(device_stat.st_mode):
        raise ProbeRejected("reported source is not a block device")
    actual_major_minor = f"{os.major(device_stat.st_rdev)}:{os.minor(device_stat.st_rdev)}"
    if actual_major_minor != maj_min:
        raise ProbeRejected("source alias resolves to a different block device")
    rows = _block_rows(lsblk)
    matches = [row for row in rows if row.get("path") == source
               and row.get("uuid") == uuid and row.get("maj:min") == maj_min]
    if len(matches) != 1:
        raise ProbeRejected("lsblk source/UUID/major:minor is not unique")
    if sum(row.get("uuid") == uuid for row in rows) != 1:
        raise ProbeRejected("lsblk UUID is ambiguous")
    labels = [row.get("label") for row in rows if row.get("label") is not None]
    if any(not isinstance(label, str) for label in labels):
        raise ProbeRejected("lsblk label is invalid")
    if labels.count(expected_label) > 1:
        raise ProbeRejected("lsblk label is duplicated")
    label = matches[0].get("label")
    if label is not None and label != expected_label:
        raise ProbeRejected("lsblk label differs from expected label")
    return label


def _major_minor(device: int) -> str:
    return f"{os.major(device)}:{os.minor(device)}"


def is_strict_descendant(path: str, ancestor: str) -> bool:
    return (ancestor.startswith("/") and os.path.normpath(ancestor) == ancestor
            and path.startswith(ancestor.rstrip("/") + "/")
            and path != ancestor and os.path.normpath(path) == path)


def verify_preflight(parent: str, info: os.stat_result, namespace: str,
                     host: dict[str, object], live: dict[str, str | None],
                     expected: dict[str, str], lsblk: dict[str, object],
                     device_stat: os.stat_result) -> dict[str, str | None]:
    if host.get("execution_record_id") != expected["record_id"]:
        raise ProbeRejected("host execution record ID mismatch")
    if host.get("mount_namespace") != namespace or namespace != expected["namespace"]:
        raise ProbeRejected("mount namespace differs from host preflight")
    host_mount = parse_findmnt(host.get("findmnt", {}))
    for key in ("source", "uuid", "fstype", "target", "options", "maj_min"):
        if live[key] != host_mount[key]:
            raise ProbeRejected(f"live {key} differs from host preflight")
    for key, expected_key in (("uuid", "uuid"), ("fstype", "fstype"),
                              ("target", "mountpoint")):
        if live[key] != expected[expected_key]:
            raise ProbeRejected(f"unexpected mount {key}")
    if not is_strict_descendant(parent, str(live["target"])):
        raise ProbeRejected("reported mountpoint is not a strict parent ancestor")
    if _major_minor(info.st_dev) != live["maj_min"]:
        raise ProbeRejected("pinned parent device differs from mount")
    if not re.fullmatch(r"[0-9]+:[0-9]+", str(live["maj_min"])):
        raise ProbeRejected("invalid mount major:minor")
    if "rw" not in str(live["options"]).split(","):
        raise ProbeRejected("mounted filesystem is not writable")
    label = verify_device(live, lsblk, expected["label"], device_stat)
    return dict(live, label=label)


def _rename_noreplace(dir_fd: int, source: str, destination: str) -> None:
    func = getattr(_LIBC, "renameat2", None)
    if func is None:
        raise OSError(errno.ENOSYS, "libc renameat2 is unavailable")
    func.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int,
                     ctypes.c_char_p, ctypes.c_uint]
    func.restype = ctypes.c_int
    if func(dir_fd, os.fsencode(source), dir_fd, os.fsencode(destination),
            _RENAME_NOREPLACE) != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))


def _create_file(dir_fd: int, name: str, data: bytes) -> tuple[int, os.stat_result]:
    if len(data) > MAX_FIXTURE_BYTES:
        raise AssertionError("fixture exceeds bound")
    fd = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW
                 | os.O_CLOEXEC, 0o600, dir_fd=dir_fd)
    try:
        written = 0
        while written < len(data):
            n = os.write(fd, data[written:])
            if n <= 0:
                raise OSError(errno.EIO, "short fixture write")
            written += n
        os.fdatasync(fd)
        return fd, os.fstat(fd)
    except BaseException:
        os.close(fd)
        raise


def _read_hash(dir_fd: int, name: str, size: int) -> str:
    fd = os.open(name, FILE_FLAGS, dir_fd=dir_fd)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size != size:
            raise ProbeRejected(f"fixture identity/size mismatch: {name}")
        data = os.read(fd, size + 1)
        if len(data) != size:
            raise ProbeRejected(f"fixture bytes changed: {name}")
        return hashlib.sha256(data).hexdigest()
    finally:
        os.close(fd)


def _create_owned_child(parent_fd: int, name: str) -> tuple[int, os.stat_result]:
    if not re.fullmatch(r"\.polaris-linux-fs-probe-[0-9]+-[0-9a-f]{32}", name):
        raise ProbeRejected("invalid owned child name")
    try:
        os.mkdir(name, 0o700, dir_fd=parent_fd)
    except FileExistsError as exc:
        raise ChildCollision("random child collision; existing path retained") from exc
    child_fd = None
    try:
        child_fd = os.open(name, DIR_FLAGS, dir_fd=parent_fd)
        child_info = os.fstat(child_fd)
        named_info = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
        if (not stat.S_ISDIR(child_info.st_mode)
                or (child_info.st_dev, child_info.st_ino)
                != (named_info.st_dev, named_info.st_ino)
                or child_info.st_uid != os.geteuid() or os.listdir(child_fd)):
            raise ProbeRejected("new child was replaced or acquired unexpected entries")
        return child_fd, child_info
    except (OSError, ProbeRejected) as exc:
        if child_fd is not None:
            os.close(child_fd)
        raise CreatedChildUnverified(f"created child not verified: {exc}") from exc


def _validate_cleanup(parent_fd: int, parent_info: os.stat_result,
                      child_fd: int, child_info: os.stat_result, child_name: str,
                      expected: dict[str, bytes], owner: int) -> None:
    if not (_same_identity(parent_fd, parent_info, owner)
            and _same_identity(child_fd, child_info, owner)):
        raise ProbeRejected("parent or child descriptor identity changed")
    if child_info.st_dev != parent_info.st_dev:
        raise ProbeRejected("probe child is on another device")
    named_info = os.stat(child_name, dir_fd=parent_fd, follow_symlinks=False)
    if (not stat.S_ISDIR(named_info.st_mode)
            or (named_info.st_dev, named_info.st_ino)
            != (child_info.st_dev, child_info.st_ino)):
        raise ProbeRejected("owned child name no longer refers to pinned child")
    if set(os.listdir(child_fd)) != set(expected):
        raise ProbeRejected("unexpected cleanup entry; child retained")
    for name, data in expected.items():
        if _read_hash(child_fd, name, len(data)) != hashlib.sha256(data).hexdigest():
            raise ProbeRejected(f"fixture hash changed: {name}")


def _mask_options(options: str) -> list[str]:
    return [part for part in options.split(",") if part.startswith(("fmask=", "dmask=", "umask="))]


def _mask_expected_modes(modes: dict[str, object]) -> tuple[int, int] | None:
    values = {}
    for option in modes["current_mask_options"]:
        name, _, raw = option.partition("=")
        if not re.fullmatch(r"0?[0-7]{3}", raw):
            return None
        values[name] = int(raw, 8)
    if not values:
        return None
    file_mask = values.get("fmask", values.get("umask"))
    dir_mask = values.get("dmask", values.get("umask"))
    if file_mask is None or dir_mask is None:
        return None
    # exfat_make_mode() starts regular files and directories at 0777.
    return 0o777 & ~file_mask, 0o777 & ~dir_mask


def _privacy_blocked_by_mask(modes: dict[str, object]) -> bool:
    file_mode = modes["effective_file"]
    dir_mode = modes["effective_directory"]
    if not isinstance(file_mode, str) or not isinstance(dir_mode, str):
        return False
    expected = _mask_expected_modes(modes)
    if expected is None:
        return False
    observed = int(file_mode, 8), int(dir_mode, 8)
    return (observed == expected
            and bool(observed[0] & 0o077 or observed[1] & 0o077))


def classify(result: dict[str, object]) -> str:
    if result["preflight"]["result"] != "pass" or result["cleanup"]["state"] in ("not_started", "retained"):
        return CLASS_C
    checks = result["capabilities"]
    if result["cleanup"]["state"] == "failed" and not (
            checks["directory_fsync"]["result"] == "fail"
            and checks["directory_fsync"]["errno"] in (errno.ENOSYS, errno.EINVAL,
                                                          errno.EOPNOTSUPP, errno.ENOTSUP)):
        return CLASS_C
    required = ("exclusive_child", "no_clobber_collision", "no_clobber_success",
                "directory_fsync", "same_device")
    if any(checks[name]["result"] in ("not_run", "inconclusive") for name in required):
        return CLASS_C
    for name in required:
        entry = checks[name]
        if entry["result"] == "fail":
            if (name in ("no_clobber_collision", "no_clobber_success", "directory_fsync")
                    and entry["errno"] in (errno.ENOSYS, errno.EINVAL, errno.EOPNOTSUPP,
                                            errno.ENOTSUP)):
                return CLASS_B
            return CLASS_C
    modes = result["mode_observations"]
    if (checks["file_mode"]["errno"] is not None
            or checks["directory_mode"]["errno"] is not None):
        return CLASS_C
    if checks["file_mode"]["result"] == "pass" and checks["directory_mode"]["result"] == "pass":
        return CLASS_A
    if result["mount"]["fstype"] == "exfat" and _privacy_blocked_by_mask(modes):
        return CLASS_P
    if (checks["file_mode"]["result"] == "not_run"
            or checks["directory_mode"]["result"] == "not_run"):
        return CLASS_C
    # A mismatch under a public mask does not prove a native filesystem is
    # required. The owner-only mask decision and repeat probe are external.
    if result["mount"]["fstype"] == "exfat" and _mask_expected_modes(modes) == (0o600, 0o700):
        return CLASS_B
    return CLASS_C


def _empty_report(record_id: str, namespace: str, host_mount: dict[str, str | None],
                  parent_info: os.stat_result, options: str) -> dict[str, object]:
    return {
        "schema_version": 1, "parent": PARENT,
        "parent_identity": identity(parent_info),
        "host_execution_record_id": record_id, "mount_namespace": namespace,
        "self_test_passed": True,
        "mount": {key: None for key in ("source", "uuid", "fstype", "target", "options", "maj_min", "label")},
        "preflight": {"result": "rejected", "host_namespace": namespace,
                      "host_mount": host_mount, "error": None},
        "capabilities": {name: capability(name) for name in CAPABILITY_SYSCALLS},
        "mode_observations": {"requested_file": "0600", "effective_file": None,
                              "requested_directory": "0700", "effective_directory": None,
                              "current_mask_options": _mask_options(options)},
        "hash_observations": {name: None for name in (
            "collision_source_before", "collision_source_after",
            "collision_destination_before", "collision_destination_after",
            "moved_destination_after")},
        "cleanup": {"state": "not_started", "retained_child": None, "error": None},
        "classification": CLASS_C, "error": None,
    }


def probe_fixture(parent_fd: int, parent_info: os.stat_result, parent_path: str,
                  report: dict[str, object], child_name: str | None = None,
                  chmod=None) -> None:
    """Run bounded descriptor-relative mutations and clean only verified fixtures."""
    name = child_name or f".polaris-linux-fs-probe-{os.getpid()}-{os.urandom(16).hex()}"
    checks = report["capabilities"]
    expected: dict[str, bytes] = {}
    child_fd = None
    child_info = None
    child_created = False
    owner = os.geteuid()
    chmod = chmod or os.fchmod
    try:
        child_fd, child_info = _create_owned_child(parent_fd, name)
        child_created = True
        checks["exclusive_child"] = capability("exclusive_child", "pass")
        report["cleanup"] = {"state": "retained", "retained_child": parent_path + "/" + name,
                             "error": "probe not yet safely cleaned"}
        if child_info.st_dev == parent_info.st_dev and child_info.st_uid == owner:
            checks["same_device"] = capability("same_device", "pass", detail=_major_minor(child_info.st_dev))
        else:
            checks["same_device"] = capability("same_device", "fail", detail="device or owner mismatch")
            raise ProbeRejected("child device or owner differs from pinned parent")
        try:
            chmod(child_fd, 0o700)
            mode = stat.S_IMODE(os.fstat(child_fd).st_mode)
            report["mode_observations"]["effective_directory"] = f"{mode:04o}"
            checks["directory_mode"] = capability("directory_mode", "pass" if mode == 0o700 else "fail",
                                                  detail=f"effective={mode:04o}")
        except OSError as exc:
            mode = stat.S_IMODE(os.fstat(child_fd).st_mode)
            report["mode_observations"]["effective_directory"] = f"{mode:04o}"
            checks["directory_mode"] = capability("directory_mode", "fail", exc.errno,
                                                  f"effective={mode:04o}")
        source_fd, source_info = _create_file(child_fd, "source", SOURCE_BYTES)
        expected["source"] = SOURCE_BYTES
        try:
            try:
                chmod(source_fd, 0o600)
                mode = stat.S_IMODE(os.fstat(source_fd).st_mode)
                report["mode_observations"]["effective_file"] = f"{mode:04o}"
                checks["file_mode"] = capability("file_mode", "pass" if mode == 0o600 else "fail",
                                                 detail=f"effective={mode:04o}")
            except OSError as exc:
                mode = stat.S_IMODE(os.fstat(source_fd).st_mode)
                report["mode_observations"]["effective_file"] = f"{mode:04o}"
                checks["file_mode"] = capability("file_mode", "fail", exc.errno,
                                                 f"effective={mode:04o}")
            if source_info.st_dev != parent_info.st_dev:
                checks["same_device"] = capability("same_device", "fail", detail="source differs")
                raise ProbeRejected("source file is on another device")
        finally:
            os.close(source_fd)
        collision_fd, collision_info = _create_file(child_fd, "collision", COLLISION_BYTES)
        expected["collision"] = COLLISION_BYTES
        os.close(collision_fd)
        if collision_info.st_dev != parent_info.st_dev:
            checks["same_device"] = capability("same_device", "fail", detail="collision differs")
            raise ProbeRejected("collision file is on another device")
        hashes = report["hash_observations"]
        hashes["collision_source_before"] = _read_hash(child_fd, "source", len(SOURCE_BYTES))
        hashes["collision_destination_before"] = _read_hash(child_fd, "collision", len(COLLISION_BYTES))
        try:
            _rename_noreplace(child_fd, "source", "collision")
        except OSError as exc:
            hashes["collision_source_after"] = _read_hash(child_fd, "source", len(SOURCE_BYTES))
            hashes["collision_destination_after"] = _read_hash(child_fd, "collision", len(COLLISION_BYTES))
            passed = (exc.errno == errno.EEXIST
                      and hashes["collision_source_after"] == hashes["collision_source_before"]
                      and hashes["collision_destination_after"] == hashes["collision_destination_before"])
            checks["no_clobber_collision"] = capability("no_clobber_collision", "pass" if passed else "fail",
                                                        None if passed else exc.errno,
                                                        "EEXIST; both hashes unchanged" if passed else str(exc))
        else:
            checks["no_clobber_collision"] = capability("no_clobber_collision", "fail",
                                                        detail="collision target was replaced")
            raise ProbeRejected("no-clobber rename replaced collision destination")
        if checks["no_clobber_collision"]["result"] == "pass":
            try:
                _rename_noreplace(child_fd, "source", "moved")
                expected.pop("source")
                expected["moved"] = SOURCE_BYTES
                moved_hash = _read_hash(child_fd, "moved", len(SOURCE_BYTES))
                report["hash_observations"]["moved_destination_after"] = moved_hash
                try:
                    os.stat("source", dir_fd=child_fd, follow_symlinks=False)
                except FileNotFoundError:
                    source_absent = True
                else:
                    source_absent = False
                passed = source_absent and moved_hash == hashes["collision_source_before"]
                checks["no_clobber_success"] = capability("no_clobber_success", "pass" if passed else "fail",
                                                          detail="destination hash and source absence" if passed else "rename postcondition failed")
            except OSError as exc:
                checks["no_clobber_success"] = capability("no_clobber_success", "fail", exc.errno, str(exc))
        try:
            os.fsync(child_fd)
            os.fsync(parent_fd)
            checks["directory_fsync"] = capability("directory_fsync", "pass")
        except OSError as exc:
            checks["directory_fsync"] = capability("directory_fsync", "fail", exc.errno, str(exc))
    except ChildCollision as exc:
        checks["exclusive_child"] = capability("exclusive_child", "fail", errno.EEXIST, str(exc))
        report["error"] = str(exc)
    except CreatedChildUnverified as exc:
        checks["exclusive_child"] = capability("exclusive_child", "inconclusive", detail=str(exc))
        report["cleanup"] = {"state": "retained", "retained_child": parent_path + "/" + name,
                             "error": str(exc)}
        report["error"] = str(exc)
    except (OSError, ProbeRejected) as exc:
        report["error"] = str(exc)
    finally:
        if child_fd is not None and child_info is not None:
            try:
                _validate_cleanup(parent_fd, parent_info, child_fd, child_info, name, expected, owner)
                for entry in sorted(expected):
                    os.unlink(entry, dir_fd=child_fd)
                os.fsync(child_fd)
                os.rmdir(name, dir_fd=parent_fd)
                child_created = False
                os.fsync(parent_fd)
                report["cleanup"] = {"state": "removed", "retained_child": None, "error": None}
            except (OSError, ProbeRejected) as exc:
                report["cleanup"] = {"state": "retained" if child_created else "failed",
                                     "retained_child": parent_path + "/" + name if child_created else None,
                                     "error": str(exc)}
            os.close(child_fd)
    report["classification"] = classify(report)


def _schema_check(value: object, schema: dict[str, object], root: dict[str, object], path: str = "$" ) -> None:
    """Validate the owned schema subset without a third-party dependency."""
    if "$ref" in schema:
        ref = schema["$ref"]
        if not isinstance(ref, str) or not ref.startswith("#/$defs/"):
            raise ValueError("unsupported schema reference")
        _schema_check(value, root["$defs"][ref.split("/")[-1]], root, path)
        return
    if "const" in schema and (type(value) is not type(schema["const"])
                              or value != schema["const"]):
        raise ValueError(f"{path}: wrong constant")
    if "enum" in schema and not any(
            type(value) is type(allowed) and value == allowed
            for allowed in schema["enum"]):
        raise ValueError(f"{path}: unknown value")
    types = schema.get("type")
    if types:
        if isinstance(types, str):
            types = [types]
        valid = any((kind == "object" and isinstance(value, dict))
                    or (kind == "array" and isinstance(value, list))
                    or (kind == "string" and isinstance(value, str))
                    or (kind == "integer" and isinstance(value, int) and not isinstance(value, bool))
                    or (kind == "boolean" and isinstance(value, bool))
                    or (kind == "null" and value is None) for kind in types)
        if not valid:
            raise ValueError(f"{path}: wrong type")
    if isinstance(value, dict):
        properties = schema.get("properties", {})
        missing = set(schema.get("required", [])) - set(value)
        if missing:
            raise ValueError(f"{path}: missing {sorted(missing)}")
        if schema.get("additionalProperties") is False and set(value) - set(properties):
            raise ValueError(f"{path}: unexpected properties")
        for key, item in value.items():
            if key in properties:
                _schema_check(item, properties[key], root, f"{path}.{key}")
    if isinstance(value, list) and "items" in schema:
        for index, item in enumerate(value):
            _schema_check(item, schema["items"], root, f"{path}[{index}]")
    if isinstance(value, str):
        if len(value) < schema.get("minLength", 0):
            raise ValueError(f"{path}: too short")
        if "pattern" in schema and not re.fullmatch(schema["pattern"], value):
            raise ValueError(f"{path}: pattern mismatch")
    if isinstance(value, int) and not isinstance(value, bool):
        if value < schema.get("minimum", value):
            raise ValueError(f"{path}: below minimum")


def validate_report(report: dict[str, object], schema_path: Path) -> None:
    schema = json.loads(schema_path.read_text(encoding="utf-8"))
    if schema.get("$schema") != "https://json-schema.org/draft/2020-12/schema":
        raise ValueError("unexpected probe schema dialect")
    _schema_check(report, schema, schema)
    if report["preflight"]["result"] == "pass":
        fields = ("source", "uuid", "fstype", "target", "options", "maj_min")
        for name, mount in (("mount", report["mount"]),
                            ("preflight.host_mount", report["preflight"]["host_mount"])):
            if any(not isinstance(mount[field], str) or not mount[field] for field in fields):
                raise ValueError(f"{name}: successful preflight requires complete mount identity")
        if any(report["mount"][field] != report["preflight"]["host_mount"][field]
               for field in fields):
            raise ValueError("successful preflight mount identity differs from host record")
    cleanup = report["cleanup"]
    state, retained, error = (cleanup[key] for key in ("state", "retained_child", "error"))
    if state == "retained":
        pattern = (re.escape(report["parent"])
                   + r"/\.polaris-linux-fs-probe-[0-9]+-[0-9a-f]{32}")
        if not isinstance(retained, str) or not re.fullmatch(pattern, retained) or not error:
            raise ValueError("retained cleanup requires an exact owned-child path and error")
    elif retained is not None:
        raise ValueError("non-retained cleanup must not report a retained child")
    if state == "removed" and error is not None:
        raise ValueError("removed cleanup must not report an error")
    if state == "failed" and not error:
        raise ValueError("failed cleanup requires an error")
    if report["classification"] != classify(report):
        raise ValueError("classification disagrees with evidence")


def _verify_preflight_file_metadata(info: os.stat_result, owner: int) -> None:
    if (not stat.S_ISREG(info.st_mode) or info.st_uid != owner or info.st_nlink != 1
            or info.st_size > 64 * 1024 or stat.S_IMODE(info.st_mode) & 0o077):
        raise ProbeRejected("host preflight record must be an owner-only regular file under 64 KiB")


def _read_preflight(path: Path) -> dict[str, object]:
    if path.parent != Path("/tmp") or path.name in ("", ".", ".."):
        raise ProbeRejected("host preflight record must be a direct child of /tmp")
    fd = os.open(path, FILE_FLAGS)
    try:
        info = os.fstat(fd)
        _verify_preflight_file_metadata(info, os.geteuid())
        raw = os.read(fd, info.st_size + 1)
        if len(raw) != info.st_size:
            raise ProbeRejected("host preflight record changed while reading")
        data = json.loads(raw)
    finally:
        os.close(fd)
    if not isinstance(data, dict):
        raise ProbeRejected("host preflight record must be an object")
    return data


def _write_result(path: Path, report: dict[str, object],
                  allowed_parent: Path = Path("/tmp")) -> None:
    if path.parent != allowed_parent or path.name in ("", ".", ".."):
        raise ProbeRejected("result path must be a direct child of its approved temporary parent")
    data = (json.dumps(report, indent=2, sort_keys=True) + "\n").encode()
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW
                 | os.O_CLOEXEC, 0o600)
    try:
        os.fchmod(fd, 0o600)
        written = 0
        while written < len(data):
            count = os.write(fd, data[written:])
            if count <= 0:
                raise OSError(errno.EIO, "short result write")
            written += count
        os.fsync(fd)
    finally:
        os.close(fd)


def validate_actual_scope(args: argparse.Namespace, effective_uid: int) -> None:
    if sys.platform != "linux":
        raise ProbeRejected("native Linux is required")
    if effective_uid == 0:
        raise ProbeRejected("USB probe refuses effective uid 0")
    if args.parent != PARENT:
        raise ProbeRejected("only the exact approved parent is accepted")
    if (args.expected_mountpoint != MOUNTPOINT or args.expected_label != "USBSSD"
            or args.expected_fstype != "exfat" or args.expected_uuid != UUID):
        raise ProbeRejected("expected USBSSD mount identity is outside approved scope")


def run_actual(args: argparse.Namespace) -> dict[str, object]:
    validate_actual_scope(args, os.geteuid())
    owned_schema = Path(__file__).parent / "schemas" / "polaris-linux-filesystem-probe-v1.schema.json"
    if os.path.abspath(args.schema) != os.path.abspath(owned_schema):
        raise ProbeRejected("actual probe requires the adjacent owned schema")
    self_test(args.schema)
    host = _read_preflight(args.host_preflight)
    namespace = os.readlink("/proc/self/ns/mnt")
    host_mount = parse_findmnt(host.get("findmnt", {}))
    parent_fd, parent_info = open_component_walk(args.parent, os.geteuid())
    report = _empty_report(args.host_execution_record_id, namespace, host_mount,
                           parent_info, str(host_mount["options"] or ""))
    report["preflight"]["host_namespace"] = host.get("mount_namespace", "")
    try:
        live = parse_findmnt(_command_json(FINDMNT))
        report["mount"] = live
        try:
            source = live["source"]
            if not isinstance(source, str) or not source.startswith("/dev/"):
                raise ProbeRejected("findmnt source is not a device path")
            block = _command_json(LSBLK + [source])
            expected = {"record_id": args.host_execution_record_id,
                        "namespace": args.expected_mount_namespace,
                        "uuid": args.expected_uuid, "label": args.expected_label,
                        "fstype": args.expected_fstype,
                        "mountpoint": args.expected_mountpoint}
            report["mount"] = verify_preflight(args.parent, parent_info, namespace, host,
                                                live, expected, block, os.stat(source))
            if not _same_identity(parent_fd, parent_info, os.geteuid()):
                raise ProbeRejected("pinned parent changed before mutation")
            report["preflight"] = {"result": "pass", "host_namespace": host["mount_namespace"],
                                   "host_mount": host_mount, "error": None}
            report["mode_observations"]["current_mask_options"] = _mask_options(str(live["options"]))
            probe_fixture(parent_fd, parent_info, args.parent, report)
        except (OSError, ValueError, subprocess.SubprocessError) as exc:
            report["preflight"]["error"] = str(exc)
            report["error"] = str(exc)
            report["classification"] = CLASS_C
    except (OSError, ValueError, subprocess.SubprocessError) as exc:
        report["preflight"]["error"] = str(exc)
        report["error"] = str(exc)
    finally:
        os.close(parent_fd)
    validate_report(report, args.schema)
    _write_result(args.result, report)
    return report


def self_test(schema_path: Path) -> None:
    """Exercise real /tmp descriptors and canned host command responses only."""
    with tempfile.TemporaryDirectory(prefix="polaris-linux-fs-selftest-", dir="/tmp") as temp:
        parent_fd, info = open_component_walk(temp, os.geteuid())
        try:
            namespace = "mnt:[12345]"
            mount = {"source": "/dev/fake1", "uuid": "ABCD-1234", "fstype": "exfat",
                     "target": "/tmp", "options": "rw,dmask=0022,fmask=0022",
                     "maj_min": _major_minor(info.st_dev), "label": None}
            row = {"source": mount["source"], "uuid": mount["uuid"],
                   "fstype": mount["fstype"], "target": mount["target"],
                   "options": mount["options"], "maj:min": mount["maj_min"]}
            host = {"execution_record_id": "selftest-host-record", "mount_namespace": namespace,
                    "findmnt": {"filesystems": [row]}}
            block = {"blockdevices": [{"path": mount["source"], "uuid": mount["uuid"],
                                       "maj:min": mount["maj_min"], "label": "USBSSD"}]}
            expected = {"record_id": "selftest-host-record", "namespace": namespace,
                        "uuid": mount["uuid"], "label": "USBSSD", "fstype": "exfat",
                        "mountpoint": "/tmp"}
            # os.stat_result constructor cannot set st_rdev; use a narrow test object.
            class Block:
                st_mode = stat.S_IFBLK
                st_rdev = info.st_dev
            verified = verify_preflight(temp, info, namespace, host, mount, expected, block, Block())
            assert verified["label"] == "USBSSD"
            assert is_strict_descendant(PARENT, MOUNTPOINT)

            def must_reject(reason: str, fn) -> None:
                try:
                    fn()
                except (OSError, ProbeRejected, ValueError):
                    return
                raise AssertionError(f"self-test accepted {reason}")

            approved_args = argparse.Namespace(
                parent=PARENT, expected_uuid=UUID, expected_label="USBSSD",
                expected_fstype="exfat", expected_mountpoint=MOUNTPOINT)
            validate_actual_scope(approved_args, 1000)
            must_reject("effective uid 0", lambda: validate_actual_scope(approved_args, 0))
            wrong_uuid = argparse.Namespace(**vars(approved_args))
            wrong_uuid.expected_uuid = "OTHER"
            must_reject("unapproved UUID", lambda: validate_actual_scope(wrong_uuid, 1000))
            direct_fd, direct_name = tempfile.mkstemp(
                prefix="polaris-linux-fs-preflight-selftest-", dir="/tmp")
            direct_preflight = Path(direct_name)
            hardlink = Path(direct_name + ".hardlink")
            symlink = Path(direct_name + ".symlink")
            try:
                with os.fdopen(direct_fd, "w", encoding="utf-8") as stream:
                    json.dump(host, stream)
                    stream.flush()
                    os.fsync(stream.fileno())
                assert _read_preflight(direct_preflight) == host
                os.chmod(direct_preflight, 0o644)
                must_reject("public direct preflight", lambda: _read_preflight(direct_preflight))
                os.chmod(direct_preflight, 0o600)
                os.link(direct_preflight, hardlink)
                must_reject("multiply linked direct preflight", lambda: _read_preflight(direct_preflight))
                hardlink.unlink()
                os.symlink(direct_preflight, symlink)
                must_reject("symlink final preflight component", lambda: _read_preflight(symlink))
            finally:
                hardlink.unlink(missing_ok=True)
                symlink.unlink(missing_ok=True)
                direct_preflight.unlink(missing_ok=True)
            preflight_file = Path(temp) / "host-preflight.json"
            preflight_file.write_text("{}", encoding="utf-8")
            os.chmod(preflight_file, 0o600)
            _verify_preflight_file_metadata(preflight_file.stat(), os.geteuid())
            must_reject("nested /tmp preflight path", lambda: _read_preflight(preflight_file))
            os.chmod(preflight_file, 0o644)
            must_reject("public preflight mode", lambda: _verify_preflight_file_metadata(
                preflight_file.stat(), os.geteuid()))
            os.chmod(preflight_file, 0o600)

            symlink = temp + "-link"
            os.symlink(temp, symlink)
            try:
                must_reject("symlink parent", lambda: open_component_walk(symlink, os.geteuid()))
            finally:
                os.unlink(symlink)
            must_reject("wrong owner", lambda: open_component_walk(temp, os.geteuid() + 1))
            must_reject("namespace mismatch", lambda: verify_preflight(
                temp, info, "mnt:[99999]", host, mount, expected, block, Block()))
            wrong_host = dict(host, findmnt={"filesystems": [dict(row, options="ro")]})
            must_reject("preflight mismatch", lambda: verify_preflight(
                temp, info, namespace, wrong_host, mount, expected, block, Block()))
            read_only = dict(mount, options="ro")
            read_only_host = dict(host, findmnt={"filesystems": [dict(row, options="ro")]})
            must_reject("read-only mount", lambda: verify_preflight(
                temp, info, namespace, read_only_host, read_only, expected, block, Block()))
            wrong_mount = dict(mount, target=temp)
            must_reject("deeper mount confusion", lambda: verify_preflight(
                temp, info, namespace, host, wrong_mount, expected, block, Block()))
            wrong_mount = dict(mount, target="/var")
            must_reject("non-ancestor mount", lambda: verify_preflight(
                temp, info, namespace, host, wrong_mount, expected, block, Block()))
            wrong_mount = dict(mount, maj_min="0:0")
            must_reject("parent device mismatch", lambda: verify_preflight(
                temp, info, namespace, host, wrong_mount, expected, block, Block()))
            for key, value in (("source", "/dev/wrong"), ("uuid", "WRONG"),
                               ("maj_min", "0:0")):
                bad = dict(mount, **{key: value})
                must_reject(key + " mismatch", lambda bad=bad: verify_device(bad, block, "USBSSD", Block()))
            duplicate = {"blockdevices": block["blockdevices"] + [dict(block["blockdevices"][0]) ]}
            must_reject("duplicate label", lambda: verify_device(mount, duplicate, "USBSSD", Block()))
            wrong_label = {"blockdevices": [dict(block["blockdevices"][0], label="OTHER")]}
            must_reject("wrong label", lambda: verify_device(mount, wrong_label, "USBSSD", Block()))
            absent_label = {"blockdevices": [dict(block["blockdevices"][0], label=None)]}
            assert verify_device(mount, absent_label, "USBSSD", Block()) is None
            name = f".polaris-linux-fs-probe-{os.getpid()}-{os.urandom(16).hex()}"
            os.mkdir(name, dir_fd=parent_fd)
            try:
                must_reject("child collision", lambda: _create_owned_child(parent_fd, name))
                assert os.stat(name, dir_fd=parent_fd).st_ino
            finally:
                os.rmdir(name, dir_fd=parent_fd)
            name = f".polaris-linux-fs-probe-{os.getpid()}-{os.urandom(16).hex()}"
            child_fd, child_info = _create_owned_child(parent_fd, name)
            try:
                fd = os.open("unexpected", os.O_CREAT | os.O_EXCL | os.O_WRONLY,
                             0o600, dir_fd=child_fd)
                os.close(fd)
                must_reject("unexpected cleanup entry", lambda: _validate_cleanup(
                    parent_fd, info, child_fd, child_info, name, {}, os.geteuid()))
                os.unlink("unexpected", dir_fd=child_fd)
            finally:
                os.close(child_fd)
                os.rmdir(name, dir_fd=parent_fd)
            report = _empty_report("selftest-host-record", namespace, dict(mount, target=MOUNTPOINT),
                                   info, mount["options"])
            report["mount"] = dict(mount, target=MOUNTPOINT, label="USBSSD")
            report["preflight"]["result"] = "pass"
            report["parent"] = PARENT
            probe_fixture(parent_fd, info, temp, report)
            assert report["cleanup"]["state"] == "removed", report["cleanup"]
            assert report["classification"] == CLASS_A, report["classification"]
            hashes = report["hash_observations"]
            assert hashes["collision_source_before"] == hashes["collision_source_after"]
            assert hashes["collision_destination_before"] == hashes["collision_destination_after"]
            assert hashes["moved_destination_after"] == hashes["collision_source_before"]
            validate_report(report, schema_path)
            result_path = Path(temp) / "result.json"
            _write_result(result_path, report, Path(temp))
            before = hashlib.sha256(result_path.read_bytes()).hexdigest()
            must_reject("result overwrite", lambda: _write_result(result_path, report, Path(temp)))
            assert hashlib.sha256(result_path.read_bytes()).hexdigest() == before
            invalid = json.loads(json.dumps(report))
            del invalid["host_execution_record_id"]
            must_reject("schema missing evidence", lambda: validate_report(invalid, schema_path))
            invalid = json.loads(json.dumps(report))
            invalid["capabilities"]["no_clobber_collision"]["result"] = "invented"
            must_reject("schema invalid evidence", lambda: validate_report(invalid, schema_path))
            invalid = json.loads(json.dumps(report))
            invalid["self_test_passed"] = 1
            must_reject("schema boolean spoof", lambda: validate_report(invalid, schema_path))
            invalid = json.loads(json.dumps(report))
            invalid["mount"]["uuid"] = None
            must_reject("successful preflight without live UUID", lambda: validate_report(invalid, schema_path))
            invalid = json.loads(json.dumps(report))
            invalid["preflight"]["host_mount"]["source"] = None
            must_reject("successful preflight without host source", lambda: validate_report(invalid, schema_path))
            invalid = json.loads(json.dumps(report))
            invalid["preflight"]["host_mount"]["options"] = ""
            must_reject("successful preflight with empty host options", lambda: validate_report(invalid, schema_path))
            retained = json.loads(json.dumps(report))
            retained["cleanup"] = {
                "state": "retained",
                "retained_child": PARENT + "/.polaris-linux-fs-probe-1-" + "a" * 32,
                "error": "unexpected entry"}
            retained["classification"] = CLASS_C
            validate_report(retained, schema_path)
            invalid = json.loads(json.dumps(retained))
            invalid["cleanup"]["retained_child"] = None
            must_reject("retained without path", lambda: validate_report(invalid, schema_path))
            invalid = json.loads(json.dumps(retained))
            invalid["cleanup"]["retained_child"] = "/tmp/other-child"
            must_reject("retained path outside parent", lambda: validate_report(invalid, schema_path))
            invalid = json.loads(json.dumps(report))
            invalid["cleanup"]["retained_child"] = retained["cleanup"]["retained_child"]
            must_reject("removed with retained path", lambda: validate_report(invalid, schema_path))
            mode_fail = json.loads(json.dumps(report))
            mode_fail["capabilities"]["file_mode"]["result"] = "fail"
            mode_fail["capabilities"]["directory_mode"]["result"] = "fail"
            mode_fail["mode_observations"]["effective_file"] = "0755"
            mode_fail["mode_observations"]["effective_directory"] = "0755"
            assert classify(mode_fail) == CLASS_P
            mode_fail["classification"] = CLASS_P
            validate_report(mode_fail, schema_path)
            unexpected_file = json.loads(json.dumps(mode_fail))
            unexpected_file["mode_observations"]["effective_file"] = "0644"
            assert classify(unexpected_file) == CLASS_C
            unexpected_file["classification"] = CLASS_C
            validate_report(unexpected_file, schema_path)
            unexpected_directory = json.loads(json.dumps(mode_fail))
            unexpected_directory["mode_observations"]["effective_directory"] = "0700"
            assert classify(unexpected_directory) == CLASS_C
            unexpected_directory["classification"] = CLASS_C
            validate_report(unexpected_directory, schema_path)
            owner_only_failure = json.loads(json.dumps(mode_fail))
            owner_only_options = "rw,fmask=0177,dmask=0077"
            owner_only_failure["mount"]["options"] = owner_only_options
            owner_only_failure["preflight"]["host_mount"]["options"] = owner_only_options
            owner_only_failure["mode_observations"]["current_mask_options"] = _mask_options(owner_only_options)
            assert classify(owner_only_failure) == CLASS_B
            owner_only_failure["classification"] = CLASS_B
            validate_report(owner_only_failure, schema_path)
            mode_io = json.loads(json.dumps(mode_fail))
            mode_io["capabilities"]["file_mode"]["errno"] = errno.EIO
            assert classify(mode_io) == CLASS_C
            mode_io["classification"] = CLASS_C
            validate_report(mode_io, schema_path)
            def fail_chmod(_fd: int, _mode: int) -> None:
                raise OSError(errno.EIO, "injected mode I/O failure")
            io_report = _empty_report("selftest-host-record", namespace,
                                      dict(mount, target=MOUNTPOINT), info, mount["options"])
            io_report["mount"] = dict(mount, target=MOUNTPOINT, label="USBSSD")
            io_report["preflight"]["result"] = "pass"
            probe_fixture(parent_fd, info, temp, io_report, chmod=fail_chmod)
            assert io_report["cleanup"]["state"] == "removed"
            assert io_report["capabilities"]["file_mode"]["errno"] == errno.EIO
            assert io_report["classification"] == CLASS_C
            validate_report(io_report, schema_path)
            no_rename = json.loads(json.dumps(report))
            no_rename["capabilities"]["no_clobber_success"] = capability(
                "no_clobber_success", "fail", errno.EOPNOTSUPP)
            assert classify(no_rename) == CLASS_B
            unknown = json.loads(json.dumps(report))
            unknown["cleanup"]["state"] = "retained"
            assert classify(unknown) == CLASS_C
        finally:
            os.close(parent_fd)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--parent")
    parser.add_argument("--expected-mount-namespace")
    parser.add_argument("--expected-uuid")
    parser.add_argument("--expected-label")
    parser.add_argument("--expected-fstype")
    parser.add_argument("--expected-mountpoint")
    parser.add_argument("--host-execution-record-id")
    parser.add_argument("--host-preflight", type=Path,
                        help="Saved separate host-channel JSON: execution_record_id, mount_namespace, findmnt")
    parser.add_argument("--schema", type=Path, required=True)
    parser.add_argument("--result", type=Path)
    args = parser.parse_args()
    try:
        if args.self_test:
            self_test(args.schema)
            print("self-test passed: descriptor fixtures, preflight rejection, A/P/B/C and schema")
            return 0
        required = ("parent", "expected_mount_namespace", "expected_uuid", "expected_label",
                    "expected_fstype", "expected_mountpoint", "host_execution_record_id",
                    "host_preflight", "result")
        for field in required:
            if not getattr(args, field):
                parser.error(f"--{field.replace('_', '-')} is required for a USB probe")
        result = run_actual(args)
        print(json.dumps(result, indent=2, sort_keys=True))
        return 0 if result["classification"] != CLASS_C else 2
    except (OSError, ValueError, AssertionError, subprocess.SubprocessError) as exc:
        parser.exit(1, f"probe failed: {exc}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
