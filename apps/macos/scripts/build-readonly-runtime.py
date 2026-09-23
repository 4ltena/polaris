#!/usr/bin/env python3
"""Build, sign and lock one architecture of the read-only Python runtime.

The build consumes a SHA-pinned python-build-standalone archive and a local
wheelhouse described by a fully hashed requirements file. It never resolves a
package version during the release build. The generated lock is derived from
the final signed DMG after mounting it read-only.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import plistlib
import re
import shutil
import shlex
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path, PurePosixPath


MAX_ARCHIVE_BYTES = 1024 * 1024 * 1024
HASH = re.compile(r"--hash=sha256:([0-9a-f]{64})")
PIN = re.compile(r"^([A-Za-z0-9_.-]+)==([^\s;]+)(?:\s+.*)?$")
MACH_O_MAGICS = {
    b"\xfe\xed\xfa\xce", b"\xce\xfa\xed\xfe", b"\xfe\xed\xfa\xcf", b"\xcf\xfa\xed\xfe",
    b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca", b"\xca\xfe\xba\xbf", b"\xbf\xba\xfe\xca",
}


@dataclass(frozen=True)
class RuntimeSource:
    runtime_id: str
    release_version: str
    architecture: str
    python_version: str
    archive_filename: str
    archive_root: str
    url: str
    sha256: str
    packages: dict[str, str]


def sha256_file(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def physical_file(path: Path, executable: bool = False) -> None:
    metadata = path.lstat()
    if path.is_symlink() or not stat.S_ISREG(metadata.st_mode):
        raise ValueError(f"physical file required: {path}")
    if executable and metadata.st_mode & 0o111 == 0:
        raise ValueError(f"executable file required: {path}")


def load_source(path: Path, architecture: str) -> RuntimeSource:
    document = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(document, dict) or document.get("schema_version") != 1:
        raise ValueError("runtime source lock must use schema version 1")
    sources = document.get("sources")
    matches = [
        item for item in sources if isinstance(item, dict)
        and item.get("platform") == "macos" and item.get("architecture") == architecture
    ] if isinstance(sources, list) else []
    if len(matches) != 1:
        raise ValueError("runtime source lock must contain exactly one architecture entry")
    item = matches[0]
    required = (
        "python_version", "archive_filename", "archive_root", "url", "sha256", "packages",
    )
    if any(key not in item for key in required):
        raise ValueError("runtime source entry is incomplete")
    packages = item["packages"]
    if not isinstance(packages, dict) or not packages or any(
        not isinstance(name, str) or not isinstance(version, str) or not name or not version
        for name, version in packages.items()
    ):
        raise ValueError("runtime packages must be a nonempty version map")
    url = item["url"]
    digest = item["sha256"]
    if not isinstance(url, str) or not url.startswith("https://"):
        raise ValueError("python source URL must use https")
    if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest):
        raise ValueError("python source SHA-256 is invalid")
    runtime_id = document.get("runtime_id")
    release_version = document.get("release_version")
    if not isinstance(runtime_id, str) or not runtime_id or "/" in runtime_id:
        raise ValueError("runtime source id is invalid")
    if not isinstance(release_version, str) or not release_version or "/" in release_version:
        raise ValueError("runtime source release version is invalid")
    return RuntimeSource(
        runtime_id=runtime_id,
        release_version=release_version,
        architecture=architecture,
        python_version=str(item["python_version"]),
        archive_filename=str(item["archive_filename"]),
        archive_root=str(item["archive_root"]),
        url=url,
        sha256=digest,
        packages={str(name).lower().replace("_", "-"): str(version) for name, version in packages.items()},
    )


def logical_requirement_lines(path: Path) -> list[str]:
    physical_file(path)
    logical: list[str] = []
    current = ""
    for raw in path.read_text(encoding="utf-8").splitlines():
        stripped = raw.strip()
        if not stripped or stripped.startswith("#"):
            continue
        current = f"{current} {stripped}".strip()
        if current.endswith("\\"):
            current = current[:-1].rstrip()
            continue
        logical.append(current)
        current = ""
    if current:
        raise ValueError("requirements ends with a continuation")
    return logical


def validate_requirements(path: Path, expected: dict[str, str]) -> dict[str, str]:
    pins: dict[str, str] = {}
    for line in logical_requirement_lines(path):
        match = PIN.fullmatch(line)
        tokens = shlex.split(line)
        hashes = [token for token in tokens[1:] if HASH.fullmatch(token)]
        if match is None or not hashes or len(hashes) != len(tokens) - 1:
            raise ValueError(f"every requirement must be exactly pinned and SHA-256 hashed: {line}")
        name = match.group(1).lower().replace("_", "-")
        version = match.group(2)
        if name in pins:
            raise ValueError(f"duplicate requirement: {name}")
        pins[name] = version
    for name, version in expected.items():
        if pins.get(name) != version:
            raise ValueError(f"runtime package pin mismatch: {name}=={version}")
    return pins


def verified_archive(source: RuntimeSource, provided: Path | None, cache: Path, allow_download: bool) -> Path:
    cache.mkdir(parents=True, exist_ok=True)
    destination = cache / source.archive_filename
    candidate = provided or destination
    if candidate.exists():
        physical_file(candidate)
        if sha256_file(candidate) != source.sha256:
            raise ValueError("python archive SHA-256 does not match the source lock")
        return candidate
    if not allow_download:
        raise ValueError("pinned python archive is unavailable; provide it or enable the verified download")
    temporary = destination.with_name(f".{destination.name}.tmp-{os.getpid()}")
    request = urllib.request.Request(source.url, headers={"User-Agent": "Polaris runtime builder/1"})
    digest = hashlib.sha256()
    total = 0
    try:
        with urllib.request.urlopen(request, timeout=60) as response, temporary.open("xb") as output:
            for block in iter(lambda: response.read(1024 * 1024), b""):
                total += len(block)
                if total > MAX_ARCHIVE_BYTES:
                    raise ValueError("python archive exceeds the release bound")
                digest.update(block)
                output.write(block)
        if digest.hexdigest() != source.sha256:
            raise ValueError("downloaded python archive SHA-256 does not match the source lock")
        os.replace(temporary, destination)
    finally:
        temporary.unlink(missing_ok=True)
    return destination


def member_path(root: Path, name: str) -> Path:
    path = PurePosixPath(name)
    if path.is_absolute() or not path.parts or any(part in ("", ".", "..") for part in path.parts):
        raise ValueError(f"unsafe archive path: {name}")
    destination = root.joinpath(*path.parts)
    if not destination.resolve(strict=False).is_relative_to(root.resolve()):
        raise ValueError(f"archive path escapes destination: {name}")
    return destination


def extract_archive(archive: Path, destination: Path) -> None:
    destination.mkdir()
    with tarfile.open(archive, "r:gz") as package:
        members = package.getmembers()
        if len(members) > 100_000:
            raise ValueError("python archive contains too many entries")
        total = sum(member.size for member in members if member.isreg())
        if total > 16 * 1024 * 1024 * 1024:
            raise ValueError("python archive expands beyond the release bound")
        names: set[str] = set()
        for member in members:
            if member.name in names:
                raise ValueError(f"duplicate archive member: {member.name}")
            names.add(member.name)
            member_path(destination, member.name)
            if not (member.isdir() or member.isreg() or member.issym() or member.islnk()):
                raise ValueError(f"special archive member is not allowed: {member.name}")
        for member in members:
            if member.isdir():
                member_path(destination, member.name).mkdir(parents=True, exist_ok=True)
        for member in members:
            if not member.isreg():
                continue
            target = member_path(destination, member.name)
            target.parent.mkdir(parents=True, exist_ok=True)
            stream = package.extractfile(member)
            if stream is None:
                raise ValueError(f"archive file could not be read: {member.name}")
            with stream, target.open("xb") as output:
                shutil.copyfileobj(stream, output)
            target.chmod(0o755 if member.mode & 0o111 else 0o644)
        for member in members:
            if not (member.issym() or member.islnk()):
                continue
            target = member_path(destination, member.name)
            target.parent.mkdir(parents=True, exist_ok=True)
            linked = (target.parent / member.linkname) if member.issym() else member_path(destination, member.linkname)
            if not linked.resolve(strict=False).is_relative_to(destination.resolve()):
                raise ValueError(f"archive link escapes destination: {member.name}")
            if member.issym():
                target.symlink_to(member.linkname)
            else:
                if not linked.is_file() or linked.is_symlink():
                    raise ValueError(f"archive hard link target is unavailable: {member.name}")
                shutil.copyfile(linked, target)
                target.chmod(linked.stat().st_mode & 0o777)


def materialize_tree(source: Path, destination: Path) -> None:
    physical_root = source.resolve(strict=True)
    destination.mkdir()

    def copy_entry(current: Path, output: Path, ancestors: frozenset[Path]) -> None:
        resolved = current.resolve(strict=True)
        if not resolved.is_relative_to(physical_root):
            raise ValueError(f"runtime alias escapes its source tree: {current}")
        metadata = resolved.stat()
        if stat.S_ISDIR(metadata.st_mode):
            if resolved in ancestors:
                raise ValueError(f"runtime directory alias is cyclic: {current}")
            output.mkdir()
            next_ancestors = ancestors | {resolved}
            for child in sorted(resolved.iterdir(), key=lambda item: item.name):
                copy_entry(child, output / child.name, next_ancestors)
            output.chmod(0o755)
        elif stat.S_ISREG(metadata.st_mode):
            shutil.copyfile(resolved, output, follow_symlinks=False)
            output.chmod(0o755 if metadata.st_mode & 0o111 else 0o644)
        else:
            raise ValueError(f"runtime contains a special file: {current}")

    for child in sorted(source.iterdir(), key=lambda item: item.name):
        copy_entry(child, destination / child.name, frozenset({physical_root}))


def remove_generated_cache(root: Path) -> None:
    for directory in sorted(root.rglob("__pycache__"), reverse=True):
        if directory.is_symlink():
            raise ValueError("runtime cache path must not be an alias")
        shutil.rmtree(directory)
    for suffix in ("*.pyc", "*.pyo"):
        for path in root.rglob(suffix):
            path.unlink()


def run(command: list[str], *, environment: dict[str, str] | None = None, capture: bool = False) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        command, check=True, env=environment,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.PIPE if capture else None,
    )


def preflight(python: Path, source: RuntimeSource) -> None:
    imports = {"sentencepiece": "sentencepiece"}
    payload = json.dumps({"python": source.python_version, "packages": source.packages, "imports": imports})
    code = """
import importlib, importlib.metadata, json, platform, sys
expected = json.loads(sys.argv[1])
assert platform.python_version() == expected["python"]
for name, version in expected["packages"].items():
    assert importlib.metadata.version(name) == version, (name, importlib.metadata.version(name), version)
    importlib.import_module(expected["imports"].get(name, name.replace("-", "_")))
"""
    environment = {
        "PATH": "/usr/bin:/bin", "PYTHONNOUSERSITE": "1", "PYTHONSAFEPATH": "1",
        "TOKENIZERS_PARALLELISM": "false",
    }
    run([str(python), "-I", "-c", code, payload], environment=environment)


def is_macho(path: Path) -> bool:
    if not path.is_file() or path.is_symlink():
        return False
    with path.open("rb") as stream:
        return stream.read(4) in MACH_O_MAGICS


def sign_tree(root: Path, identity: str, codesign: Path) -> None:
    if not identity or identity == "-":
        raise ValueError("an explicit non-ad-hoc signing identity is required")
    binaries = sorted((path for path in root.rglob("*") if is_macho(path)), key=lambda path: len(path.parts), reverse=True)
    if not binaries:
        raise ValueError("runtime tree contains no Mach-O executable")
    for binary in binaries:
        run([str(codesign), "--force", "--options", "runtime", "--timestamp", "--sign", identity, str(binary)])
        run([str(codesign), "--verify", "--strict", "--verbose=2", str(binary)])


def attach_plist(data: bytes) -> tuple[str, Path]:
    document = plistlib.loads(data)
    entities = document.get("system-entities", []) if isinstance(document, dict) else []
    mounted = [item for item in entities if isinstance(item, dict) and item.get("mount-point")]
    devices = [item.get("dev-entry") for item in entities if isinstance(item, dict) and item.get("dev-entry")]
    if len(mounted) != 1 or not devices:
        raise ValueError("hdiutil did not return one mounted runtime volume")
    return str(devices[0]), Path(str(mounted[0]["mount-point"]))


@contextmanager
def mounted_image(image: Path, hdiutil: Path):
    result = run([
        str(hdiutil), "attach", "-plist", "-readonly", "-nobrowse", "-noautoopen", str(image),
    ], capture=True)
    device, mount = attach_plist(result.stdout)
    try:
        yield mount
    finally:
        run([str(hdiutil), "detach", device])


def publish_lock(generated: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(f".{destination.name}.tmp-{os.getpid()}")
    try:
        shutil.copyfile(generated, temporary)
        os.replace(temporary, destination)
    finally:
        temporary.unlink(missing_ok=True)


def main() -> int:
    script_directory = Path(__file__).resolve().parent
    resources = script_directory.parent / "Sources/PolarisDesktop/Resources"
    parser = argparse.ArgumentParser()
    parser.add_argument("--architecture", choices=("arm64", "x86_64"), required=True)
    parser.add_argument("--source-lock", type=Path, default=resources / "readonly-runtime-sources-v2.json")
    parser.add_argument("--python-archive", type=Path)
    parser.add_argument("--cache-directory", type=Path, default=Path.home() / ".polaris/build-cache/readonly-runtime-v2")
    parser.add_argument("--allow-download", action="store_true")
    parser.add_argument("--requirements", type=Path, required=True)
    parser.add_argument("--wheelhouse", type=Path, required=True)
    parser.add_argument("--uv", type=Path, required=True)
    parser.add_argument("--signing-identity", required=True)
    parser.add_argument("--output-directory", type=Path, required=True)
    parser.add_argument("--lock-output", type=Path, default=resources / "readonly-runtime-images-v2.json")
    delivery = parser.add_mutually_exclusive_group(required=True)
    delivery.add_argument("--source-url")
    delivery.add_argument("--bundled-resource")
    parser.add_argument("--hdiutil", type=Path, default=Path("/usr/bin/hdiutil"))
    parser.add_argument("--codesign", type=Path, default=Path("/usr/bin/codesign"))
    parser.add_argument("--lock-generator", type=Path, default=script_directory / "generate-readonly-runtime-lock.py")
    args = parser.parse_args()

    if not args.output_directory.is_absolute() or args.output_directory.exists() or args.output_directory.is_symlink():
        raise ValueError("output directory must be a new absolute path")
    if args.source_url and not args.source_url.startswith("https://"):
        raise ValueError("runtime delivery URL must use https")
    if args.bundled_resource and (args.bundled_resource.startswith("/") or ".." in Path(args.bundled_resource).parts):
        raise ValueError("bundled runtime path must stay inside Resources")
    physical_file(args.source_lock)
    physical_file(args.requirements)
    physical_file(args.uv, executable=True)
    physical_file(args.hdiutil, executable=True)
    physical_file(args.codesign, executable=True)
    physical_file(args.lock_generator)
    if not args.wheelhouse.is_dir() or args.wheelhouse.is_symlink():
        raise ValueError("wheelhouse must be a physical directory")

    source = load_source(args.source_lock, args.architecture)
    validate_requirements(args.requirements, source.packages)
    archive = verified_archive(source, args.python_archive, args.cache_directory, args.allow_download)
    args.output_directory.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".readonly-runtime-", dir=args.output_directory.parent) as temporary:
        work = Path(temporary)
        extracted = work / "extracted"
        extract_archive(archive, extracted)
        archive_root = member_path(extracted, source.archive_root)
        if not archive_root.is_dir():
            raise ValueError("python archive root is missing")
        install_tree = work / "install-tree"
        install_tree.mkdir()
        materialize_tree(archive_root, install_tree / "runtime")
        python = install_tree / "runtime/bin/python3.11"
        physical_file(python, executable=True)
        environment = {
            "PATH": "/usr/bin:/bin", "HOME": str(work / "home"),
            "UV_CACHE_DIR": str(work / "uv-cache"), "UV_NO_PROGRESS": "1",
            "PYTHONNOUSERSITE": "1", "PYTHONSAFEPATH": "1",
        }
        (work / "home").mkdir()
        run([
            str(args.uv), "pip", "install", "--python", str(python), "--no-index",
            "--find-links", str(args.wheelhouse), "--require-hashes", "--no-deps",
            "-r", str(args.requirements),
        ], environment=environment)
        remove_generated_cache(install_tree)
        package_record = {
            "schema_version": 1, "python_version": source.python_version,
            "architecture": source.architecture, "packages": source.packages,
        }
        (install_tree / "embedding-packages.json").write_text(
            json.dumps(package_record, indent=2, sort_keys=True) + "\n", encoding="utf-8",
        )
        preflight(python, source)

        sealed = work / "runtime-tree"
        materialize_tree(install_tree, sealed)
        sign_tree(sealed, args.signing_identity, args.codesign)
        preflight(sealed / "runtime/bin/python3.11", source)

        image_name = f"{source.runtime_id}-{source.release_version}-{source.architecture}.dmg"
        image = work / image_name
        run([
            str(args.hdiutil), "create", "-fs", "APFS", "-format", "UDRO", "-volname",
            "Polaris Runtime", "-srcfolder", str(sealed), str(image),
        ])
        physical_file(image)
        run([str(args.codesign), "--force", "--timestamp", "--sign", args.signing_identity, str(image)])
        run([str(args.codesign), "--verify", "--strict", "--verbose=2", str(image)])

        generated_lock = work / "readonly-runtime-images-v2.json"
        with mounted_image(image, args.hdiutil) as mounted:
            preflight(mounted / "runtime/bin/python3.11", source)
            command = [
                sys.executable, str(args.lock_generator), "--tree-root", str(mounted),
                "--image", str(image), "--version", source.release_version,
                "--architecture", source.architecture, "--output", str(generated_lock),
            ]
            if args.lock_output.exists():
                command.extend(["--merge-existing", str(args.lock_output)])
            if args.source_url:
                command.extend(["--source-url", args.source_url])
            else:
                command.extend(["--bundled-resource", args.bundled_resource])
            run(command)

        build_output = work / "output"
        build_output.mkdir()
        sealed.rename(build_output / "runtime-tree")
        image.rename(build_output / image.name)
        shutil.copyfile(generated_lock, build_output / generated_lock.name)
        build_output.rename(args.output_directory)
        publish_lock(args.output_directory / generated_lock.name, args.lock_output)
    print(f"read-only runtime build completed: {args.output_directory}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, KeyError, json.JSONDecodeError, tarfile.TarError, subprocess.CalledProcessError) as error:
        print(f"read-only runtime build failed: {error}", file=sys.stderr)
        raise SystemExit(1)
