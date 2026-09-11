#!/usr/bin/env python3
"""指定した構成の.appを組み立てる。起動・署名・配布は行わない。"""

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
    parser.add_argument("--toolchain-package", type=Path, action="append", default=[])
    parser.add_argument("--configuration", choices=["debug", "release"], default="debug")
    parser.add_argument("--destination", type=Path)
    arguments = parser.parse_args()
    # Validate before spending the Swift build slot. No Cargo build or discovery.
    validate_helper_argument(arguments.service_helper)
    if arguments.execution_helper is not None:
        validate_helper_argument(arguments.execution_helper)
    package = Path(__file__).resolve().parent.parent
    scratch = package / ".build"
    destination = arguments.destination or scratch / "PolarisDesktop.app"
    if not destination.is_absolute():
        raise RuntimeError("出力先は絶対パスで指定してください。")
    if destination.exists() or destination.is_symlink():
        raise RuntimeError("既存の .build/PolarisDesktop.app を別の場所へ移してから再実行してください。")

    command = ["swift", "build", "--package-path", str(package),
               "--scratch-path", str(scratch), "--configuration", arguments.configuration]
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
        if arguments.execution_helper is not None:
            package_execution_helper(arguments.execution_helper, contents)
            repository = package.parent.parent
            # 同梱資源はGit管理する正本から取得し、ビルド元の個人設定を混入させない。
            copy_runtime_tree(repository / "skills", contents / "Resources" / "skills")
            copy_runtime_tree(repository / "agents", contents / "Resources" / "agents")
            if arguments.toolchain_package:
                (contents / "Resources" / "toolchains").mkdir()
            for index, source in enumerate(arguments.toolchain_package):
                if not source.is_absolute() or not (source / "bin").is_dir():
                    raise RuntimeError("toolchainにはbinを持つ移設可能なpackageの絶対パスが必要です。")
                copy_runtime_tree(source, contents / "Resources" / "toolchains" / str(index))
        shutil.copy2(executable, contents / "MacOS" / "PolarisDesktop")
        # SwiftPM生成accessorの優先探索先はBundle.main.bundleURL直下。
        # ビルドディレクトリへのフォールバックに依存しない配置を保持する。
        shutil.copytree(resources, app / resource_name)
        with (contents / "Info.plist").open("wb") as stream:
            plistlib.dump(info, stream)
        subprocess.run(["/usr/bin/plutil", "-lint", str(contents / "Info.plist")], check=True)
        app.rename(destination)

    print("アプリを作成しました：", destination)
    print("隔離起動例：")
    print(f'open -n "{destination}" --args --settings-path /tmp/polaris-native-review/settings.json')


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"アプリの組立てに失敗しました: {error}", file=sys.stderr)
        sys.exit(1)
