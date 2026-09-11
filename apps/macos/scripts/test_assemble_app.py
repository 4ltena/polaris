import importlib.util
import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("assembly", Path(__file__).with_name("assemble-app.py"))
assembly = importlib.util.module_from_spec(spec)
spec.loader.exec_module(assembly)


class HelperAssemblyTests(unittest.TestCase):
    def test_runtime_copy_preserves_executable_and_rejects_alias(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "runtime"
            (source / "bin").mkdir(parents=True)
            (source / "bin" / "tool").write_bytes(b"portable tool")
            (source / "bin" / "tool").chmod(0o700)
            os.link(source / "bin/tool", root / "shared-build-input")
            assembly.copy_runtime_tree(source, root / "copy")
            self.assertEqual((root / "copy/bin/tool").read_bytes(), b"portable tool")
            self.assertEqual((root / "copy/bin/tool").stat().st_mode & 0o777, 0o755)
            self.assertEqual((root / "copy/bin/tool").stat().st_nlink, 1)
            (source / "alias").symlink_to(source / "bin/tool")
            with self.assertRaises(RuntimeError):
                assembly.copy_runtime_tree(source, root / "rejected")

    def test_execution_manifest_matches_packaged_bytes_without_source_path(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "explicit-build"
            source.write_bytes(b"fixed execution helper")
            source.chmod(0o700)
            contents = root / "Contents"
            (contents / "Helpers").mkdir(parents=True)
            (contents / "Resources").mkdir()
            assembly.package_execution_helper(source, contents)
            packaged = contents / "Helpers" / "polaris-execution-helper"
            source.write_bytes(b"new build")
            manifest = json.loads((contents / "Resources" / "execution-helper.json").read_text())
            self.assertEqual(packaged.read_bytes(), b"fixed execution helper")
            self.assertEqual(manifest, {"schema_version": 1, "sha256": hashlib.sha256(packaged.read_bytes()).hexdigest()})

    def test_explicit_executable_bytes_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "explicit-helper"
            destination = Path(directory) / "packaged-helper"
            source.write_bytes(b"test helper bytes")
            source.chmod(0o700)
            assembly.copy_helper(source, destination)
            self.assertEqual(destination.read_bytes(), source.read_bytes())
            self.assertEqual(destination.stat().st_mode & 0o777, 0o755)
            with self.assertRaises(FileExistsError):
                assembly.copy_helper(source, destination)

    def test_symlink_and_non_executable_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "source"
            destination = Path(directory) / "output"
            source.write_bytes(b"test")
            source.chmod(0o600)
            with self.assertRaises(RuntimeError):
                assembly.copy_helper(source, destination)
            alias = Path(directory) / "alias"
            alias.symlink_to(source)
            with self.assertRaises(OSError):
                assembly.copy_helper(alias, destination)
            self.assertFalse(destination.exists())


if __name__ == "__main__":
    unittest.main()
