import importlib.util
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("assembly", Path(__file__).with_name("assemble-app.py"))
assembly = importlib.util.module_from_spec(spec)
spec.loader.exec_module(assembly)
REPOSITORY = Path(__file__).resolve().parents[3]


class HelperAssemblyTests(unittest.TestCase):
    def assemble_checkout(self, root, with_local_skills=False, configuration=None):
        repository = root / "source"
        package = repository / "apps/macos"
        binary_directory = package / ".build/bin"
        resources = binary_directory / "PolarisDesktop_PolarisDesktop.bundle"
        resources.mkdir(parents=True)
        (resources / "release.json").write_text('{"version": "0.12.0"}')
        (binary_directory / "PolarisDesktop").write_bytes(b"compiled native executable")
        shutil.copytree(REPOSITORY / "skills", repository / "skills")
        shutil.copytree(REPOSITORY / "agents", repository / "agents")
        if with_local_skills:
            local = repository / ".polaris/skills"
            (local / "verify-a-change").mkdir(parents=True)
            (local / "verify-a-change/SKILL.md").write_text("private local override")
            (local / "private-only").mkdir()
            (local / "private-only/SKILL.md").write_text("local notes must stay local")
        else:
            self.assertFalse((repository / ".polaris").exists())
        service = root / "service-helper"
        execution = root / "execution-helper"
        for helper in [service, execution]:
            helper.write_bytes(b"compiled " + helper.name.encode())
            helper.chmod(0o700)
        destination = root / "PolarisDesktop.app"
        arguments = [
            "assemble-app.py", "--service-helper", str(service),
            "--execution-helper", str(execution), "--destination", str(destination),
        ]
        if configuration is not None:
            arguments += ["--configuration", configuration]
        # Stub external commands. Exercise the real resource copy, manifest,
        # metadata and final publication paths against a clean source fixture.
        with patch.object(assembly, "__file__", str(package / "scripts/assemble-app.py")), \
                patch.object(assembly.sys, "argv", arguments), \
                patch.object(assembly.subprocess, "run") as run, \
                patch.object(assembly.subprocess, "check_output", return_value=str(binary_directory)) as output:
            assembly.main()
        expected = configuration or "debug"
        for command in [run.call_args_list[0].args[0], output.call_args.args[0]]:
            self.assertEqual(command[command.index("--configuration") + 1], expected)
        return destination

    def test_release_build_packages_matching_resources(self):
        with tempfile.TemporaryDirectory() as directory:
            app = self.assemble_checkout(Path(directory), configuration="release")
            self.assert_bundled_catalogs(app)
            self.assertTrue((app / "PolarisDesktop_PolarisDesktop.bundle/release.json").is_file())

    def assert_bundled_catalogs(self, app):
        for catalog in ["skills", "agents"]:
            expected = {
                path.relative_to(REPOSITORY / catalog): path.read_bytes()
                for path in (REPOSITORY / catalog).rglob("*") if path.is_file()
            }
            destination = app / "Contents/Resources" / catalog
            actual = {
                path.relative_to(destination): path.read_bytes()
                for path in destination.rglob("*") if path.is_file()
            }
            self.assertTrue(expected)
            self.assertEqual(actual, expected)

    def test_execution_app_assembles_without_project_local_skills(self):
        with tempfile.TemporaryDirectory() as directory:
            app = self.assemble_checkout(Path(directory))
            self.assert_bundled_catalogs(app)
            with (app / "Contents/Info.plist").open("rb") as stream:
                self.assertEqual(plistlib.load(stream)["CFBundleShortVersionString"], "0.12.0")
            helper = app / "Contents/Helpers/polaris-execution-helper"
            self.assertEqual(helper.read_bytes(), b"compiled execution-helper")
            manifest = json.loads((app / "Contents/Resources/execution-helper.json").read_text())
            self.assertEqual(manifest["sha256"], hashlib.sha256(helper.read_bytes()).hexdigest())

    def test_local_overrides_do_not_change_the_packaged_catalogs(self):
        with tempfile.TemporaryDirectory() as directory:
            app = self.assemble_checkout(Path(directory), with_local_skills=True)
            self.assert_bundled_catalogs(app)

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
