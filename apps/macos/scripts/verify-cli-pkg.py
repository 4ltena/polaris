#!/usr/bin/env python3
"""Verify a locally built Polaris CLI package without installing it."""

import argparse
import hashlib
import os
from pathlib import Path, PurePosixPath
import subprocess
import tempfile
import xml.etree.ElementTree as ET


EXPECTED_ID = "com.polaris.cli.preview"
CLI_PATH = "./usr/local/bin/polaris"


def allowed_payload_path(path: str) -> bool:
    directories = {".", "./usr", "./usr/local", "./usr/local/bin"}
    if "\\" in path or ".." in PurePosixPath(path).parts:
        return False
    if path in directories | {CLI_PATH}:
        return True
    return (path.startswith("./") and Path(path).name.startswith("._")
            and str(PurePosixPath(path).parent) in {".", "usr", "usr/local", "usr/local/bin"})


def verify(package: Path, expected_cli: Path) -> str:
    package = package.resolve(strict=True)
    expected_cli = expected_cli.resolve(strict=True)
    if not package.is_file() or not expected_cli.is_file():
        raise ValueError("package and expected CLI must be regular files")
    expected_hash = hashlib.sha256(expected_cli.read_bytes()).hexdigest()
    expected_version = subprocess.run([str(expected_cli), "--version"], check=True,
                                      capture_output=True, text=True, timeout=10).stdout.strip()
    if not expected_version.startswith("polaris "):
        raise ValueError("expected executable did not identify as Polaris")
    payload_paths = subprocess.run(["/usr/sbin/pkgutil", "--payload-files", str(package)],
                                   check=True, capture_output=True, text=True).stdout.splitlines()
    if CLI_PATH not in payload_paths or any(not allowed_payload_path(path) for path in payload_paths):
        raise ValueError("package contains an unexpected payload path")

    with tempfile.TemporaryDirectory(prefix="polaris-cli-verify-") as temporary:
        root = Path(temporary)
        expanded = root / "expanded"
        installed = root / "isolated-root"
        installed.mkdir()
        subprocess.run(["/usr/sbin/pkgutil", "--expand", str(package), str(expanded)], check=True)
        info = ET.parse(expanded / "PackageInfo").getroot()
        if info.get("identifier") != EXPECTED_ID or info.get("install-location") != "/":
            raise ValueError("package identity or install location changed")
        if expected_version != f"polaris {info.get('version')}":
            raise ValueError("package version differs from its executable")
        gzip = subprocess.run(["/usr/bin/gzip", "-dc", str(expanded / "Payload")],
                              check=True, capture_output=True)
        subprocess.run(["/usr/bin/cpio", "-id", "--quiet"], input=gzip.stdout,
                       cwd=installed, check=True, capture_output=True)
        cli = installed / "usr/local/bin/polaris"
        if cli.is_symlink() or not cli.is_file() or not os.access(cli, os.X_OK):
            raise ValueError("package CLI is missing or not executable")
        if hashlib.sha256(cli.read_bytes()).hexdigest() != expected_hash:
            raise ValueError("packaged CLI differs from the expected executable")
        environment = {"HOME": str(root / "clean-home"),
                       "PATH": f"{cli.parent}:/usr/bin:/bin", "TERM": "xterm-256color"}
        version = subprocess.run(["/bin/sh", "-c", "command -v polaris && polaris --version"],
                                 check=True, capture_output=True, text=True, timeout=10,
                                 env=environment)
        if version.stdout.strip().splitlines() != [str(cli), expected_version]:
            raise ValueError("Polaris did not resolve to the packaged CLI")
        subprocess.run([str(cli), "--help"], check=True, capture_output=True,
                       text=True, timeout=10, env=environment)
    return expected_hash


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package", required=True, type=Path)
    parser.add_argument("--expected-cli", required=True, type=Path)
    args = parser.parse_args()
    digest = verify(args.package, args.expected_cli)
    print(f"Verified package CLI SHA-256: {digest}")


if __name__ == "__main__":
    main()
