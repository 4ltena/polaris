import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("generate-readonly-runtime-lock.py")


class RuntimeLockTests(unittest.TestCase):
    def run_generator(self, root, architecture, output, merge=None, version="fixture"):
        tree = root / f"tree-{architecture}"
        python = tree / "runtime/bin/python3.11"
        python.parent.mkdir(parents=True, exist_ok=True)
        python.write_bytes(architecture.encode())
        python.chmod(0o755)
        image = root / f"runtime-{architecture}.dmg"
        image.write_bytes(f"image-{architecture}".encode())
        command = [
            sys.executable, str(SCRIPT), "--tree-root", str(tree), "--image", str(image),
            "--version", version, "--architecture", architecture,
            "--bundled-resource", image.name, "--output", str(output),
        ]
        if merge is not None:
            command.extend(["--merge-existing", str(merge)])
        subprocess.run(command, check=True)

    def test_generates_complete_lock_and_rejects_missing_python(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            tree = root / "tree"
            python = tree / "runtime/bin/python3.11"
            python.parent.mkdir(parents=True)
            python.write_bytes(b"python")
            python.chmod(0o755)
            (tree / "runtime/lib").mkdir(parents=True)
            (tree / "runtime/lib/module.py").write_bytes(b"module")
            image = root / "runtime.dmg"
            image.write_bytes(b"image")
            output = root / "lock.json"
            subprocess.run([
                sys.executable, str(SCRIPT), "--tree-root", str(tree), "--image", str(image),
                "--version", "fixture", "--architecture", "arm64",
                "--bundled-resource", "runtime.dmg", "--output", str(output),
            ], check=True)
            lock = json.loads(output.read_text())
            self.assertEqual(lock["schema_version"], 2)
            self.assertEqual(len(lock["images"][0]["tree_manifest"]["files"]), 2)
            python.unlink()
            failed = subprocess.run([
                sys.executable, str(SCRIPT), "--tree-root", str(tree), "--image", str(image),
                "--version", "fixture", "--architecture", "arm64",
                "--source-url", "https://example.invalid/runtime.dmg", "--output", str(output),
            ], capture_output=True)
            self.assertNotEqual(failed.returncode, 0)

    def test_rejects_symlink_and_non_https_source(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            tree = root / "tree"
            python = tree / "runtime/bin/python3.11"
            python.parent.mkdir(parents=True)
            python.write_bytes(b"python")
            python.chmod(0o755)
            (tree / "alias").symlink_to(python)
            image = root / "runtime.dmg"
            image.write_bytes(b"image")
            result = subprocess.run([
                sys.executable, str(SCRIPT), "--tree-root", str(tree), "--image", str(image),
                "--version", "fixture", "--architecture", "arm64",
                "--source-url", "http://example.invalid/runtime.dmg",
                "--output", str(root / "lock.json"),
            ], capture_output=True)
            self.assertNotEqual(result.returncode, 0)

    def test_merge_preserves_other_architecture_and_replaces_same_architecture(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            lock = root / "lock.json"
            self.run_generator(root, "arm64", lock)
            self.run_generator(root, "x86_64", root / "merged.json", merge=lock)
            merged = json.loads((root / "merged.json").read_text())
            self.assertEqual(
                [image["architecture"] for image in merged["images"]],
                ["arm64", "x86_64"],
            )
            self.run_generator(
                root, "arm64", root / "replaced.json", merge=root / "merged.json",
                version="replacement",
            )
            replaced = json.loads((root / "replaced.json").read_text())
            self.assertEqual(len(replaced["images"]), 2)
            arm = next(image for image in replaced["images"] if image["architecture"] == "arm64")
            self.assertEqual(arm["version"], "replacement")

    def test_merge_rejects_duplicate_existing_entries(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            tree = root / "tree"
            python = tree / "runtime/bin/python3.11"
            python.parent.mkdir(parents=True)
            python.write_bytes(b"python")
            python.chmod(0o755)
            image = root / "runtime.dmg"
            image.write_bytes(b"image")
            existing = root / "existing.json"
            duplicate = {"id": "python-e5-cat", "platform": "macos", "architecture": "arm64"}
            existing.write_text(json.dumps({"schema_version": 2, "images": [duplicate, duplicate]}))
            output = root / "lock.json"
            result = subprocess.run([
                sys.executable, str(SCRIPT), "--tree-root", str(tree),
                "--image", str(image), "--version", "fixture", "--architecture", "arm64",
                "--bundled-resource", "runtime.dmg", "--merge-existing", str(existing),
                "--output", str(output),
            ], capture_output=True)
            self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
