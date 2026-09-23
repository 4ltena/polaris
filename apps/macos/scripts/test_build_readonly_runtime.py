import hashlib
import importlib.metadata
import importlib.util
import io
import json
from pathlib import Path
import platform
import sys
import tarfile
import tempfile
import unittest
import subprocess


SCRIPT = Path(__file__).with_name("build-readonly-runtime.py")
SPEC = importlib.util.spec_from_file_location("readonly_runtime_builder", SCRIPT)
builder = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = builder
SPEC.loader.exec_module(builder)
REPOSITORY = Path(__file__).resolve().parents[3]
SOURCE_LOCK = (
    REPOSITORY
    / "apps/macos/Sources/PolarisDesktop/Resources/readonly-runtime-sources-v2.json"
)


class ReadonlyRuntimeBuilderTests(unittest.TestCase):
    def executable_script(self, path, body):
        path.write_text(body, encoding="utf-8")
        path.chmod(0o755)

    def test_production_source_lock_fixes_both_architectures(self):
        arm = builder.load_source(SOURCE_LOCK, "arm64")
        intel = builder.load_source(SOURCE_LOCK, "x86_64")
        self.assertEqual(arm.python_version, "3.11.9")
        self.assertEqual(intel.python_version, "3.11.9")
        self.assertEqual(arm.sha256, "c4e2f7774421bcb381245945e132419b529399dfa4a56059acda1493751fa377")
        self.assertEqual(intel.sha256, "c8680f90137e36b54b3631271ccdfe5de363e7d563d8df87c53e11b956a00e04")
        self.assertEqual(arm.packages["torch"], "2.9.1")
        self.assertEqual(intel.packages["torch"], "2.2.2")

    def test_requirements_are_exactly_pinned_and_hashed(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            requirements = root / "requirements.txt"
            requirements.write_text(
                f"numpy==2.4.6 --hash=sha256:{'a' * 64}\n"
                f"torch==2.9.1 --hash=sha256:{'b' * 64}\n",
                encoding="utf-8",
            )
            self.assertEqual(
                builder.validate_requirements(requirements, {"numpy": "2.4.6", "torch": "2.9.1"}),
                {"numpy": "2.4.6", "torch": "2.9.1"},
            )
            requirements.write_text("numpy==2.4.6\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                builder.validate_requirements(requirements, {"numpy": "2.4.6"})

    def test_safe_extract_and_materialize_remove_aliases(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "python.tar.gz"
            with tarfile.open(archive, "w:gz") as package:
                directory = tarfile.TarInfo("python/bin")
                directory.type = tarfile.DIRTYPE
                package.addfile(directory)
                data = b"fixture python"
                executable = tarfile.TarInfo("python/bin/python3.11")
                executable.mode = 0o755
                executable.size = len(data)
                package.addfile(executable, io.BytesIO(data))
                alias = tarfile.TarInfo("python/bin/python3")
                alias.type = tarfile.SYMTYPE
                alias.linkname = "python3.11"
                package.addfile(alias)
            extracted = root / "extracted"
            builder.extract_archive(archive, extracted)
            self.assertTrue((extracted / "python/bin/python3").is_symlink())
            sealed = root / "sealed"
            builder.materialize_tree(extracted / "python", sealed)
            self.assertFalse((sealed / "bin/python3").is_symlink())
            self.assertEqual((sealed / "bin/python3").read_bytes(), data)
            self.assertEqual((sealed / "bin/python3.11").read_bytes(), data)

    def test_extract_rejects_link_outside_archive(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "bad.tar.gz"
            with tarfile.open(archive, "w:gz") as package:
                alias = tarfile.TarInfo("python/bin/python3.11")
                alias.type = tarfile.SYMTYPE
                alias.linkname = "../../../outside"
                package.addfile(alias)
            with self.assertRaises(ValueError):
                builder.extract_archive(archive, root / "extracted")

    def test_archive_hash_must_match_before_use(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "python.tar.gz"
            archive.write_bytes(b"fixture")
            source = builder.RuntimeSource(
                runtime_id="python-e5-cat", release_version="fixture", architecture="arm64",
                python_version="3.11.9", archive_filename="cached.tar.gz", archive_root="python",
                url="https://example.invalid/python.tar.gz", sha256=hashlib.sha256(b"other").hexdigest(),
                packages={"numpy": "2.4.6"},
            )
            with self.assertRaises(ValueError):
                builder.verified_archive(source, archive, root / "cache", False)

    @unittest.skipUnless(sys.platform == "darwin", "the release builder is macOS-only")
    def test_local_fixture_runs_the_complete_release_pipeline(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive_root = root / "archive/python/bin"
            archive_root.mkdir(parents=True)
            python = archive_root / "python3.11"
            python.write_bytes(Path(sys.executable).resolve().read_bytes())
            python.chmod(0o755)
            archive = root / "python.tar.gz"
            with tarfile.open(archive, "w:gz") as package:
                package.add(root / "archive/python", arcname="python")

            pip_version = importlib.metadata.version("pip")
            source_lock = root / "sources.json"
            source_lock.write_text(json.dumps({
                "schema_version": 1,
                "runtime_id": "python-e5-cat",
                "release_version": "fixture",
                "sources": [{
                    "platform": "macos", "architecture": "arm64",
                    "python_version": platform.python_version(), "archive_filename": archive.name,
                    "archive_root": "python", "url": "https://example.invalid/python.tar.gz",
                    "sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
                    "packages": {"pip": pip_version},
                }],
            }), encoding="utf-8")
            requirements = root / "requirements.txt"
            requirements.write_text(
                f"pip=={pip_version} --hash=sha256:{'a' * 64}\n", encoding="utf-8")
            wheelhouse = root / "wheels"
            wheelhouse.mkdir()
            uv = root / "uv"
            codesign = root / "codesign"
            hdiutil = root / "hdiutil"
            self.executable_script(uv, "#!/bin/sh\nexit 0\n")
            self.executable_script(codesign, "#!/bin/sh\nexit 0\n")
            self.executable_script(hdiutil, """#!/usr/bin/env python3
import pathlib, plistlib, shutil, sys
args = sys.argv[1:]
if args[0] == "create":
    source = pathlib.Path(args[args.index("-srcfolder") + 1])
    image = pathlib.Path(args[-1])
    image.write_bytes(b"fixture dmg")
    shutil.copytree(source, image.with_suffix(".mount"))
elif args[0] == "attach":
    image = pathlib.Path(args[-1])
    plistlib.dump({"system-entities": [
        {"dev-entry": "/dev/disk999"},
        {"dev-entry": "/dev/disk999s1", "mount-point": str(image.with_suffix(".mount"))},
    ]}, sys.stdout.buffer)
elif args[0] != "detach":
    raise SystemExit(2)
""")
            output = root / "output"
            lock = root / "production-lock.json"
            result = subprocess.run([
                sys.executable, str(SCRIPT), "--architecture", "arm64",
                "--source-lock", str(source_lock), "--python-archive", str(archive),
                "--requirements", str(requirements), "--wheelhouse", str(wheelhouse),
                "--uv", str(uv), "--signing-identity", "fixture identity",
                "--output-directory", str(output), "--lock-output", str(lock),
                "--source-url", "https://example.invalid/runtime.dmg",
                "--hdiutil", str(hdiutil), "--codesign", str(codesign),
            ], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            generated = json.loads(lock.read_text())
            self.assertEqual(generated["schema_version"], 2)
            self.assertEqual(generated["images"][0]["architecture"], "arm64")
            self.assertTrue((output / "runtime-tree/runtime/bin/python3.11").is_file())
            self.assertTrue((output / "python-e5-cat-fixture-arm64.dmg").is_file())


if __name__ == "__main__":
    unittest.main()
