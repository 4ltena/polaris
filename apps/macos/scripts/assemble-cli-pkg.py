#!/usr/bin/env python3
"""Build a local macOS CLI preview package from an already-built executable.

The package installs only Polaris, not third-party models or toolchains.
It does not sign, notarize, publish, or install the package.
"""

import argparse
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib


PACKAGE_ID = "com.polaris.cli.preview"


def architecture(binary: Path) -> str:
    result = subprocess.run(["/usr/bin/lipo", "-archs", str(binary)],
                            check=True, capture_output=True, text=True)
    slices = result.stdout.strip().split()
    if len(slices) != 1 or slices[0] not in {"arm64", "x86_64"}:
        raise ValueError("CLI preview requires one supported macOS architecture")
    return slices[0]


def binary_version(binary: Path) -> str:
    result = subprocess.run([str(binary), "--version"], check=True, capture_output=True,
                            text=True, timeout=10)
    return result.stdout.strip()


def build_package(binary: Path, output: Path, repository: Path) -> Path:
    binary = binary.resolve(strict=True)
    output = output.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise ValueError("the Polaris CLI must be an executable file")
    arch = architecture(binary)
    version = tomllib.loads((repository / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    if not isinstance(version, str) or not version:
        raise ValueError("workspace package version is missing")
    if binary_version(binary) != f"polaris {version}":
        raise ValueError("CLI version does not match the workspace package version")
    if output.exists():
        raise FileExistsError(f"refusing to replace existing package: {output}")
    if not output.parent.is_dir():
        raise ValueError("output directory does not exist")
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()

    with tempfile.TemporaryDirectory(prefix="polaris-cli-pkg-") as temporary:
        root = Path(temporary)
        target = root / "payload/usr/local/bin/polaris"
        target.parent.mkdir(parents=True)
        shutil.copy2(binary, target)
        target.chmod(0o755)
        subprocess.run(["/usr/bin/xattr", "-cr", str(root / "payload")], check=True)
        if hashlib.sha256(target.read_bytes()).hexdigest() != digest:
            raise ValueError("copied CLI digest changed")
        scripts = root / "scripts"
        scripts.mkdir()
        preinstall = scripts / "preinstall"
        preinstall.write_text(
            "#!/bin/sh\n"
            "set -eu\n"
            "target=/usr/local/bin/polaris\n"
            "if [ -L \"$target\" ] || [ -d \"$target\" ]; then\n"
            "  echo 'Polaris: an existing command occupies /usr/local/bin/polaris' >&2\n"
            "  exit 1\n"
            "fi\n"
            "if [ -e \"$target\" ]; then\n"
            "  existing=$(/usr/bin/shasum -a 256 \"$target\" | /usr/bin/cut -d ' ' -f1)\n"
            f"  if [ \"$existing\" != '{digest}' ]; then\n"
            "    echo 'Polaris: an existing command occupies /usr/local/bin/polaris' >&2\n"
            "    exit 1\n"
            "  fi\n"
            "fi\n",
            encoding="utf-8",
        )
        preinstall.chmod(0o755)
        command = ["/usr/bin/pkgbuild", "--root", str(root / "payload"),
                   "--scripts", str(scripts), "--identifier", PACKAGE_ID,
                   "--version", version, "--install-location", "/", str(output)]
        subprocess.run(command, check=True, env={**os.environ, "COPYFILE_DISABLE": "1"})
    return output


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    repository = Path(__file__).resolve().parents[3]
    package = build_package(args.binary, args.output, repository)
    print(f"Created {package}")
    print(f"Install with: sudo installer -pkg '{package}' -target /")


if __name__ == "__main__":
    main()
