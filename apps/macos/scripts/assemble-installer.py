#!/usr/bin/env python3
"""Polaris.appを内包する未署名のローカル確認用インストーラを組み立てる。"""

import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tempfile


RESOURCE_BUNDLE = "PolarisDesktop_PolarisDesktop.bundle"
CLI_MANIFEST_NAME = "polaris-cli.json"
CLI_RELATIVE_PATH = "Contents/Helpers/polaris"
SUPPORTED_MACOS_ARCHITECTURES = frozenset({"arm64", "arm64e", "x86_64"})


def load_app_assembler():
    path = Path(__file__).with_name("assemble-app.py")
    spec = importlib.util.spec_from_file_location("polaris_app_assembly", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(65536), b""):
            digest.update(chunk)
    return digest.hexdigest()


def validate_cli_payload(app):
    cli = app / CLI_RELATIVE_PATH
    manifest_path = app / "Contents" / "Resources" / CLI_MANIFEST_NAME
    if cli.is_symlink() or not cli.is_file() or manifest_path.is_symlink() or not manifest_path.is_file():
        raise RuntimeError("Polaris.appのCLI実行物または検証情報が不足しています。")
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise RuntimeError("Polaris.appのCLI検証情報を読めません。") from error
    architectures = manifest.get("architectures")
    expected_hash = manifest.get("sha256")
    if (manifest.get("schema_version") != 1
            or manifest.get("relative_path") != CLI_RELATIVE_PATH
            or not isinstance(architectures, list)
            or not architectures
            or any(not isinstance(item, str) or item not in SUPPORTED_MACOS_ARCHITECTURES for item in architectures)
            or not isinstance(expected_hash, str)
            or len(expected_hash) != 64
            or any(character not in "0123456789abcdef" for character in expected_hash)
            or sha256_file(cli) != expected_hash):
        raise RuntimeError("Polaris.appのCLI検証に失敗しました。")
    return cli


def validate_payload(app):
    if not app.is_absolute() or app.is_symlink() or not app.is_dir():
        raise RuntimeError("Polaris.appは絶対パスの通常ディレクトリで指定してください。")
    info_path = app / "Contents/Info.plist"
    with info_path.open("rb") as stream:
        info = plistlib.load(stream)
    executable = info.get("CFBundleExecutable")
    if (info.get("CFBundlePackageType") != "APPL"
            or info.get("CFBundleIdentifier") != "com.polaris.desktop.local"
            or not isinstance(executable, str)):
        raise RuntimeError("Polaris.appのメタデータを検証できません。")
    binary = app / "Contents/MacOS" / executable
    resources = app / RESOURCE_BUNDLE
    if not binary.is_file() or not resources.is_dir():
        raise RuntimeError("Polaris.appの実行物または資源が不足しています。")
    return info, binary, resources, validate_cli_payload(app)


def assemble_installer(payload, destination, bundle_id="com.polaris.installer.local"):
    info, binary, resource_bundle, _cli = validate_payload(payload)
    if not bundle_id.startswith("com.polaris.installer."):
        raise RuntimeError("インストーラのbundle IDを確認してください。")
    if not destination.is_absolute():
        raise RuntimeError("出力先は絶対パスで指定してください。")
    if destination.exists() or destination.is_symlink():
        raise RuntimeError("既存のインストーラを別の場所へ移してから再実行してください。")

    with tempfile.TemporaryDirectory(prefix="polaris-installer-", dir=destination.parent) as temporary:
        app = Path(temporary) / "Polaris Installer.app"
        contents = app / "Contents"
        (contents / "MacOS").mkdir(parents=True)
        (contents / "Resources").mkdir()
        shutil.copy2(binary, contents / "MacOS/PolarisInstaller")
        shutil.copytree(resource_bundle, app / RESOURCE_BUNDLE)
        shutil.copytree(payload, contents / "Resources/Polaris.app", symlinks=False)

        installer_info = {
            "CFBundleDevelopmentRegion": "ja",
            "CFBundleIdentifier": bundle_id,
            "CFBundleExecutable": "PolarisInstaller",
            "CFBundleName": "Polaris Installer",
            "CFBundleDisplayName": "Polarisインストーラ",
            "CFBundlePackageType": "APPL",
            "CFBundleInfoDictionaryVersion": "6.0",
            "CFBundleShortVersionString": info["CFBundleShortVersionString"],
            "CFBundleVersion": info["CFBundleVersion"],
            "LSMinimumSystemVersion": info.get("LSMinimumSystemVersion", "13.0"),
            "NSHighResolutionCapable": True,
            "NSPrincipalClass": "NSApplication",
        }
        with (contents / "Info.plist").open("wb") as stream:
            plistlib.dump(installer_info, stream)
        subprocess.run(["/usr/bin/plutil", "-lint", str(contents / "Info.plist")], check=True)
        app.rename(destination)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--service-helper", type=Path)
    parser.add_argument("--execution-helper", type=Path)
    parser.add_argument("--polaris-executable", type=Path)
    parser.add_argument("--payload-app", type=Path)
    parser.add_argument("--destination", type=Path, required=True)
    parser.add_argument("--bundle-id", default="com.polaris.installer.local")
    parser.add_argument("--disable-swiftpm-sandbox", action="store_true")
    arguments = parser.parse_args()
    if arguments.payload_app is None and arguments.service_helper is None:
        raise RuntimeError("service helperまたは組立て済みPolaris.appが必要です。")
    if arguments.payload_app is not None and (arguments.service_helper is not None
                                              or arguments.execution_helper is not None
                                              or arguments.polaris_executable is not None):
        raise RuntimeError("payload-appとhelper指定は同時に使えません。")
    if arguments.payload_app is None and arguments.polaris_executable is None:
        raise RuntimeError("組立てるCLI実行物を絶対パスで指定してください。")

    if arguments.payload_app is not None:
        assemble_installer(arguments.payload_app, arguments.destination, arguments.bundle_id)
    else:
        with tempfile.TemporaryDirectory(prefix="polaris-installer-payload-") as temporary:
            payload = Path(temporary) / "Polaris.app"
            assembler = load_app_assembler()
            original = sys.argv
            try:
                sys.argv = ["assemble-app.py", "--service-helper", str(arguments.service_helper),
                            "--polaris-executable", str(arguments.polaris_executable),
                            "--destination", str(payload)]
                if arguments.execution_helper is not None:
                    sys.argv.extend(["--execution-helper", str(arguments.execution_helper)])
                if arguments.disable_swiftpm_sandbox:
                    sys.argv.append("--disable-swiftpm-sandbox")
                assembler.main()
            finally:
                sys.argv = original
            assemble_installer(payload, arguments.destination, arguments.bundle_id)
    print("ローカル確認用インストーラを作成しました：", arguments.destination)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"インストーラの組立てに失敗しました: {error}", file=sys.stderr)
        sys.exit(1)
