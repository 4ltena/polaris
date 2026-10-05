#!/usr/bin/env python3
"""Check local Linux directory publication without modifying the project.

Retain the bounded fixture on success and failure. The original filesystem
probe supplies the native no-replace and create-new file primitives unchanged.
"""

import argparse
import errno
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import stat
import sys

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "filesystem_probe", Path(__file__).with_name("probe-linux-filesystem.py")
)
filesystem = importlib.util.module_from_spec(spec)
spec.loader.exec_module(filesystem)

DIR_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
MAX_ENTRIES = 16
MAX_BYTES = 64 * 1024


class Refusal(ValueError):
    pass


def budget(entries, byte_count):
    if not 0 <= entries <= MAX_ENTRIES or not 0 <= byte_count <= MAX_BYTES:
        raise Refusal("fixture_bound")


def pin(path):
    path = os.fspath(path)
    if os.fsdecode(os.fsencode(path)) != path or not path.startswith("/"):
        raise Refusal("unsafe_path")
    path.encode("utf-8", errors="strict")
    if os.path.normpath(path) != path:
        raise Refusal("unsafe_path")
    fd = os.open("/", DIR_FLAGS)
    try:
        for component in path.split("/")[1:]:
            if not component:
                raise Refusal("unsafe_path")
            child = os.open(component, DIR_FLAGS, dir_fd=fd)
            os.close(fd)
            fd = child
        return fd
    except BaseException:
        os.close(fd)
        raise


def identity(info):
    return info.st_dev, info.st_ino


def verify(fd, path):
    other = pin(path)
    try:
        if identity(os.fstat(fd)) != identity(os.fstat(other)):
            raise Refusal("raced_identity")
    finally:
        os.close(other)


def mount_root(project_fd):
    fd = os.dup(project_fd)
    try:
        for _ in range(4096):
            current = os.fstat(fd)
            parent = os.open("..", DIR_FLAGS, dir_fd=fd)
            above = os.fstat(parent)
            if identity(current) == identity(above) or current.st_dev != above.st_dev:
                os.close(parent)
                return identity(current)
            os.close(fd)
            fd = parent
        raise Refusal("ancestor_bound")
    finally:
        os.close(fd)


def validate_candidate(candidate, project_fd, expected_device):
    if stat.S_ISLNK(os.lstat(candidate).st_mode):
        raise Refusal("parent_is_symlink")
    fd = pin(candidate)
    try:
        info = os.fstat(fd)
        if identity(info) == mount_root(project_fd):
            raise Refusal("parent_is_filesystem_root")
        if identity(info) == identity(os.fstat(project_fd)):
            raise Refusal("parent_is_project_root")
        with os.scandir(fd) as entries:
            nonempty = next(entries, None) is not None
        if info.st_uid == os.geteuid() and nonempty:
            raise Refusal("parent_nonempty_caller_owned")
        if info.st_dev != expected_device:
            raise Refusal("parent_on_different_device")
        if info.st_uid != os.geteuid() or stat.S_IMODE(info.st_mode) != 0o700:
            raise Refusal("unsafe_owner_or_mode")
        verify(fd, candidate)
        return fd
    except BaseException:
        os.close(fd)
        raise


def child(parent_fd, name):
    os.mkdir(name, 0o700, dir_fd=parent_fd)
    fd = os.open(name, DIR_FLAGS, dir_fd=parent_fd)
    info = os.fstat(fd)
    assert identity(info) == identity(os.stat(name, dir_fd=parent_fd, follow_symlinks=False))
    assert info.st_uid == os.geteuid() and stat.S_IMODE(info.st_mode) == 0o700
    os.fsync(parent_fd)
    return fd


def file(directory, name, data):
    fd, info = filesystem._create_file(directory, name, data)
    try:
        assert info.st_uid == os.geteuid() and stat.S_IMODE(info.st_mode) == 0o600
        assert info.st_nlink == 1
    finally:
        os.close(fd)
    return info


def run(parent, project, other_parent):
    budget(13, 7)
    for entries, size in [(17, 0), (0, MAX_BYTES + 1)]:
        try:
            budget(entries, size)
        except Refusal:
            pass
        else:
            raise AssertionError("bound must reject before filesystem I/O")
    project_fd = pin(project)
    parent_fd = validate_candidate(parent, project_fd, os.fstat(project_fd).st_dev)
    other_fd = pin(other_parent)
    held = [project_fd, parent_fd, other_fd]
    try:
        other_info = os.fstat(other_fd)
        assert other_info.st_uid == os.geteuid() and stat.S_IMODE(other_info.st_mode) == 0o700
        assert other_info.st_dev != os.fstat(project_fd).st_dev
        verify(other_fd, other_parent)
        fixture = child(parent_fd, "directory-capability")
        held.append(fixture)
        source = child(fixture, "source")
        destination = child(fixture, "destination")
        nonempty = child(fixture, "nonempty")
        cross = child(other_fd, "directory-capability-cross-device")
        held.extend([source, destination, nonempty, cross])
        file(source, "payload", b"new")
        file(destination, "payload", b"old")
        file(nonempty, "sentinel", b"x")
        os.symlink("nonempty", "symlink", dir_fd=fixture)
        source_identity = identity(os.fstat(source))
        destination_identity = identity(os.fstat(destination))
        for fd in [source, destination, nonempty, fixture]:
            os.fsync(fd)
        try:
            filesystem._rename_noreplace(fixture, "source", "destination")
        except OSError as error:
            assert error.errno == errno.EEXIST
        else:
            raise AssertionError("occupied directory was replaced")
        assert identity(os.stat("destination", dir_fd=fixture)) == destination_identity
        assert os.stat("source", dir_fd=fixture).st_ino == source_identity[1]
        assert filesystem._read_hash(destination, "payload", 3) == hashlib.sha256(b"old").hexdigest()
        filesystem._rename_noreplace(fixture, "source", "published")
        os.fsync(fixture)
        assert "source" not in os.listdir(fixture)
        assert identity(os.stat("published", dir_fd=fixture, follow_symlinks=False)) == source_identity
        assert filesystem._read_hash(source, "payload", 3) == hashlib.sha256(b"new").hexdigest()
        cases = [
            ("parent_is_filesystem_root", Path("/home")),
            ("parent_is_project_root", project),
            ("parent_nonempty_caller_owned", parent / "directory-capability/nonempty"),
            ("parent_is_symlink", parent / "directory-capability/symlink"),
            ("parent_on_different_device", other_parent / "directory-capability-cross-device"),
        ]
        refused = {}
        for expected, candidate in cases:
            try:
                unexpected = validate_candidate(candidate, project_fd, os.fstat(project_fd).st_dev)
            except Refusal as error:
                assert str(error) == expected, (expected, str(error))
                refused[expected] = str(error)
            else:
                os.close(unexpected)
                raise AssertionError(expected)
        verify(parent_fd, parent)
        verify(project_fd, project)
        verify(other_fd, other_parent)
        assert set(os.listdir(fixture)) == {"published", "destination", "nonempty", "symlink"}
        assert set(os.listdir(source)) == set(os.listdir(destination)) == {"payload"}
        assert set(os.listdir(nonempty)) == {"sentinel"}
        return {
            "directory_no_replace": "pass",
            "collision_preserves_identity_and_bytes": "pass",
            "file_and_directory_sync": "pass",
            "owner_only_modes": "pass",
            "bounds": {"maximum_entries": MAX_ENTRIES, "maximum_bytes": MAX_BYTES},
            "parent_refusals": refused,
            "product_parent_validator": "not_run",
            "retained_fixture": str(parent / "directory-capability"),
        }
    finally:
        for fd in reversed(held):
            os.close(fd)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--parent", type=Path, required=True)
    parser.add_argument("--project", type=Path, required=True)
    parser.add_argument("--other-parent", type=Path, required=True)
    parser.add_argument("--result", type=Path, required=True)
    args = parser.parse_args()
    os.umask(0o077)
    report = run(args.parent, args.project, args.other_parent)
    fd = os.open(args.result, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "w") as output:
        json.dump(report, output, sort_keys=True)
        output.write("\n")
        output.flush()
        os.fsync(output.fileno())
    parent_fd = pin(args.result.parent)
    try:
        os.fsync(parent_fd)
    finally:
        os.close(parent_fd)
    print(json.dumps(report, sort_keys=True))


if __name__ == "__main__":
    main()
