#!/usr/bin/env python3
"""ローカル実機確認用の未配布.appを組み立てる。起動や署名は行わない。"""

import argparse
import hashlib
import os
import stat
import json
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tempfile


CLI_MANIFEST_NAME = "polaris-cli.json"
ENTRY_MANIFEST_NAME = "mcp-entry-binaries-v2.json"
SUPPORTED_MACOS_ARCHITECTURES = frozenset({"arm64", "arm64e", "x86_64"})


def copy_helper(source, destination):
    if not source.is_absolute():
        raise RuntimeError("service helperは絶対パスで指定してください。")
    fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or not info.st_mode & 0o111:
            raise RuntimeError("service helperは実行可能な通常ファイルが必要です。")
        with destination.open("xb") as output:
            shutil.copyfileobj(stream, output)
        destination.chmod(0o755)


def package_execution_helper(source, contents):
    destination = contents / "Helpers" / "polaris-execution-helper"
    copy_helper(source, destination)
    # Describe the packaged bytes, not a later read of the original build path.
    digest = hashlib.sha256()
    with destination.open("rb") as stream:
        for chunk in iter(lambda: stream.read(65536), b""):
            digest.update(chunk)
    manifest = {"schema_version": 1, "sha256": digest.hexdigest()}
    with (contents / "Resources" / "execution-helper.json").open("x", encoding="utf-8") as stream:
        json.dump(manifest, stream, sort_keys=True)
        stream.write("\n")


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(65536), b""):
            digest.update(chunk)
    return digest.hexdigest()


def macho_architectures(path):
    """Return the declared slices of a packaged macOS executable.

    The check intentionally runs after copying into the temporary app.  A
    build-path replacement therefore cannot make the manifest describe bytes
    other than the bytes that will be published in the app.
    """
    result = subprocess.run(
        ["/usr/bin/lipo", "-archs", str(path)],
        check=False,
        capture_output=True,
        text=True,
    )
    architectures = tuple(result.stdout.split())
    if result.returncode != 0 or not architectures:
        raise RuntimeError("polaris CLIがmacOS実行物として検証できません。")
    if any(architecture not in SUPPORTED_MACOS_ARCHITECTURES for architecture in architectures):
        raise RuntimeError("polaris CLIに未対応のmacOSアーキテクチャが含まれています。")
    return architectures


def package_polaris_cli(source, contents):
    """Package the explicit CLI build and bind its manifest to copied bytes."""
    destination = contents / "Helpers" / "polaris"
    copy_helper(source, destination)
    manifest = {
        "schema_version": 1,
        "relative_path": "Contents/Helpers/polaris",
        "architectures": list(macho_architectures(destination)),
        "sha256": sha256_file(destination),
    }
    with (contents / "Resources" / CLI_MANIFEST_NAME).open("x", encoding="utf-8") as stream:
        json.dump(manifest, stream, sort_keys=True)
        stream.write("\n")



def package_entry_manifest(contents, with_execution_helper):
    """Bind every packaged production entry without temporary build anchors."""
    entries = [
        ("cli", "Contents/Helpers/polaris"),
        ("desktop_app", "Contents/MacOS/PolarisDesktop"),
        ("desktop_service", "Contents/Helpers/polaris-desktop-service"),
    ]
    if with_execution_helper:
        entries.append(("execution_helper", "Contents/Helpers/polaris-execution-helper"))
    expected = {relative for _, relative in entries}
    actual = {
        str(path.relative_to(contents.parent))
        for directory in [contents / "MacOS", contents / "Helpers"]
        for path in directory.iterdir()
    }
    if actual != expected:
        raise RuntimeError("同梱production entryの集合が一致しません。")
    identities = []
    for kind, relative in entries:
        path = contents.parent / relative
        info = path.lstat()
        if (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1
                or info.st_uid != os.geteuid() or info.st_mode & 0o7022
                or not info.st_mode & 0o111 or not 0 < info.st_size <= 256 * 1024 * 1024):
            raise RuntimeError("同梱production entryのidentityを検証できません。")
        identities.append({"kind": kind, "relative_path": relative, "sha256": sha256_file(path)})
    manifest = {"protocol_version": 2, "entries": identities}
    with (contents / "Resources" / ENTRY_MANIFEST_NAME).open("x", encoding="utf-8") as stream:
        json.dump(manifest, stream, separators=(",", ":"))
        stream.write("\n")


def validate_helper_argument(source):
    if not source.is_absolute() or source.is_symlink() or not source.is_file() or not os.access(source, os.X_OK):
        raise RuntimeError("既存のhelper実行物を絶対パスで明示的に指定してください。")


def copy_runtime_tree(source, destination):
    """明示的なビルド入力を別inodeへコピーし、symlinkや特殊ファイルは拒否する。"""
    if not source.is_dir() or source.is_symlink():
        raise RuntimeError("同梱資源には通常のディレクトリが必要です。")
    destination.mkdir()
    for entry in sorted(source.iterdir()):
        if entry.name in {".DS_Store", "__pycache__"}:
            continue
        info = entry.lstat()
        if stat.S_ISDIR(info.st_mode):
            copy_runtime_tree(entry, destination / entry.name)
        elif stat.S_ISREG(info.st_mode):
            # ビルド入力は共有linkでも、新規出力へbyteをコピーして共有を切る。
            # 実行時のworkspace/catalog readerの複数link拒否は変更しない。
            shutil.copyfile(entry, destination / entry.name, follow_symlinks=False)
            (destination / entry.name).chmod(0o755 if info.st_mode & 0o111 else 0o644)
        else:
            raise RuntimeError("同梱資源のlinkまたは特殊ファイルを拒否しました。")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--service-helper", type=Path, required=True)
    parser.add_argument("--execution-helper", type=Path)
    parser.add_argument("--polaris-executable", type=Path, required=True)
    parser.add_argument("--destination", type=Path)
    parser.add_argument("--scratch-path", type=Path)
    parser.add_argument("--disable-swiftpm-sandbox", action="store_true")
    arguments = parser.parse_args()
    # Validate before spending the Swift build slot. No Cargo build or discovery.
    validate_helper_argument(arguments.service_helper)
    validate_helper_argument(arguments.polaris_executable)
    if arguments.execution_helper is not None:
        validate_helper_argument(arguments.execution_helper)
    package = Path(__file__).resolve().parent.parent
    scratch = arguments.scratch_path or package / ".build"
    if not scratch.is_absolute() or scratch.is_symlink():
        raise RuntimeError("scratch出力はsymlinkでない絶対パスを指定してください。")
    scratch.mkdir(parents=True, exist_ok=True)
    destination = arguments.destination or scratch / "PolarisDesktop.app"
    if not destination.is_absolute():
        raise RuntimeError("出力先は絶対パスで指定してください。")
    if destination.exists() or destination.is_symlink():
        raise RuntimeError("既存の .build/PolarisDesktop.app を別の場所へ移してから再実行してください。")

    command = ["swift", "build", "--package-path", str(package),
               "--scratch-path", str(scratch), "--configuration", "debug", "--jobs", "1"]
    if arguments.disable_swiftpm_sandbox:
        command.append("--disable-sandbox")
    subprocess.run(command + ["--product", "PolarisDesktop"], check=True)
    binary_directory = Path(subprocess.check_output(command + ["--show-bin-path"], text=True).strip())
    executable = binary_directory / "PolarisDesktop"
    resource_name = "PolarisDesktop_PolarisDesktop.bundle"
    resources = binary_directory / resource_name
    if not executable.is_file() or not resources.is_dir():
        raise RuntimeError("ビルド済み実行ファイルまたはSwiftPMリソースbundleが見つかりません。")

    # ビルドされた版情報と同じ値をInfo.plistへ渡す。
    release = json.loads((resources / "release.json").read_text(encoding="utf-8"))
    version = release["version"]
    if not isinstance(version, str) or not version:
        raise RuntimeError("リリース情報の版番号を確認できません。")
    info = {
        "CFBundleDevelopmentRegion": "ja",
        "CFBundleIdentifier": "com.polaris.desktop.local",
        "CFBundleExecutable": "PolarisDesktop",
        "CFBundleName": "PolarisDesktop",
        "CFBundleDisplayName": "polaris",
        "CFBundlePackageType": "APPL",
        "CFBundleInfoDictionaryVersion": "6.0",
        "CFBundleShortVersionString": version,
        "CFBundleVersion": version,
        "LSMinimumSystemVersion": "13.0",
        "NSHighResolutionCapable": True,
        "NSPrincipalClass": "NSApplication",
        "NSSpeechRecognitionUsageDescription": "音声を端末内で文字に変換し、会話の下書きへ追加します。",
        "NSMicrophoneUsageDescription": "音声入力ボタンを押したときに、下書き用の音声を録音します。",
    }

    # 既存.appを上書きせず、組立てが全部成功してから完成先へ移す。
    with tempfile.TemporaryDirectory(prefix="native-app-", dir=scratch) as temporary:
        app = Path(temporary) / "PolarisDesktop.app"
        contents = app / "Contents"
        (contents / "MacOS").mkdir(parents=True)
        (contents / "Resources").mkdir()
        (contents / "Helpers").mkdir()
        copy_helper(arguments.service_helper, contents / "Helpers" / "polaris-desktop-service")
        package_polaris_cli(arguments.polaris_executable, contents)
        if arguments.execution_helper is not None:
            package_execution_helper(arguments.execution_helper, contents)
            repository = package.parent.parent
            # 同梱資源はGit管理する正本から取得し、ビルド元の個人設定を混入させない。
            copy_runtime_tree(repository / "skills", contents / "Resources" / "skills")
            copy_runtime_tree(repository / "agents", contents / "Resources" / "agents")
        shutil.copy2(executable, contents / "MacOS" / "PolarisDesktop")
        package_entry_manifest(contents, arguments.execution_helper is not None)
        # SwiftPM生成accessorの優先探索先はBundle.main.bundleURL直下。
        # ビルドディレクトリへのフォールバックに依存しない配置を保持する。
        shutil.copytree(resources, app / resource_name)
        with (contents / "Info.plist").open("wb") as stream:
            plistlib.dump(info, stream)
        subprocess.run(["/usr/bin/plutil", "-lint", str(contents / "Info.plist")], check=True)
        app.rename(destination)

    print("ローカル確認用アプリを作成しました：", destination)
    print("隔離起動例：")
    print(f'open -n "{destination}" --args --settings-path /tmp/polaris-native-review/settings.json')


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"アプリの組立てに失敗しました: {error}", file=sys.stderr)
        sys.exit(1)
