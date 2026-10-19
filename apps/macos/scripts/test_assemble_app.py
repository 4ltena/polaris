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
    def assemble_checkout(self, root, with_local_skills=False, with_execution_helper=True, scratch=None):
        repository = root / "source"
        package = repository / "apps/macos"
        binary_directory = package / ".build/bin"
        resources = binary_directory / "PolarisDesktop_PolarisDesktop.bundle"
        resources.mkdir(parents=True)
        (resources / "release.json").write_text('{"version": "0.12.0"}')
        shutil.copy2(
            REPOSITORY / "apps/macos/Sources/PolarisDesktop/Resources/runtime-dependencies.json",
            resources / "runtime-dependencies.json",
        )
        (binary_directory / "PolarisDesktop").write_bytes(b"compiled native executable")
        (binary_directory / "PolarisDesktop").chmod(0o755)
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
        cli = root / "polaris"
        for helper in [service, execution, cli]:
            helper.write_bytes(b"compiled " + helper.name.encode())
            helper.chmod(0o700)
        destination = root / "PolarisDesktop.app"
        arguments = [
            "assemble-app.py", "--service-helper", str(service),
            "--polaris-executable", str(cli),
            "--destination", str(destination),
        ]
        if with_execution_helper:
            arguments.extend(["--execution-helper", str(execution)])
        if scratch is not None:
            arguments.extend(["--scratch-path", str(scratch)])
        # Stub compilation only. Exercise the real resource copy, manifest,
        # metadata and final publication paths against a clean source fixture.
        with patch.object(assembly, "__file__", str(package / "scripts/assemble-app.py")), \
                patch.object(assembly.sys, "argv", arguments), \
                patch.object(assembly.subprocess, "run") as run, \
                patch.object(assembly, "macho_architectures", return_value=("arm64",)), \
                patch.object(assembly.subprocess, "check_output", return_value=str(binary_directory)):
            assembly.main()
            if scratch is not None:
                build = run.call_args_list[0].args[0]
                self.assertEqual(build[build.index("--scratch-path") + 1], str(scratch))
                self.assertTrue(scratch.is_dir())
                self.assertFalse(list(scratch.glob("native-app-*")))
        return destination

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
            cli = app / "Contents/Helpers/polaris"
            self.assertEqual(cli.read_bytes(), b"compiled polaris")
            cli_manifest = json.loads((app / "Contents/Resources/polaris-cli.json").read_text())
            self.assertEqual(cli_manifest, {
                "architectures": ["arm64"],
                "relative_path": "Contents/Helpers/polaris",
                "schema_version": 1,
                "sha256": hashlib.sha256(cli.read_bytes()).hexdigest(),
            })
            self.assertFalse((app / "Contents/Resources/toolchains").exists())
            self.assertTrue((app / "PolarisDesktop_PolarisDesktop.bundle/runtime-dependencies.json").is_file())

    def test_entry_manifest_describes_all_packaged_entries_without_build_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            app = self.assemble_checkout(root)
            manifest = json.loads((app / "Contents/Resources" / assembly.ENTRY_MANIFEST_NAME).read_text())
            expected = [
                ("cli", "Contents/Helpers/polaris"),
                ("desktop_app", "Contents/MacOS/PolarisDesktop"),
                ("desktop_service", "Contents/Helpers/polaris-desktop-service"),
                ("execution_helper", "Contents/Helpers/polaris-execution-helper"),
            ]
            self.assertEqual(manifest, {"protocol_version": 2, "entries": [
                {"kind": kind, "relative_path": relative,
                 "sha256": hashlib.sha256((app / relative).read_bytes()).hexdigest()}
                for kind, relative in expected
            ]})
            self.assertNotIn(str(root), json.dumps(manifest))
            with self.assertRaises(FileExistsError):
                assembly.package_entry_manifest(app / "Contents", True)

    def test_entry_manifest_without_optional_helper_has_exact_required_set(self):
        with tempfile.TemporaryDirectory() as directory:
            app = self.assemble_checkout(Path(directory), with_execution_helper=False)
            manifest = json.loads((app / "Contents/Resources" / assembly.ENTRY_MANIFEST_NAME).read_text())
            self.assertEqual([entry["kind"] for entry in manifest["entries"]],
                             ["cli", "desktop_app", "desktop_service"])
            unknown = app / "Contents/Helpers/unknown"
            unknown.write_bytes(b"unexpected entry")
            unknown.chmod(0o755)
            with self.assertRaises(RuntimeError):
                assembly.package_entry_manifest(app / "Contents", False)

    def test_dedicated_scratch_is_used_for_local_assembly(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            scratch = root / "mcp-m1-build"
            app = self.assemble_checkout(root, scratch=scratch)
            self.assertTrue((app / "Contents/Resources" / assembly.ENTRY_MANIFEST_NAME).is_file())

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

    def test_cli_manifest_describes_the_packaged_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "polaris"
            source.write_bytes(b"fixed CLI")
            source.chmod(0o700)
            contents = root / "Contents"
            (contents / "Helpers").mkdir(parents=True)
            (contents / "Resources").mkdir()
            with patch.object(assembly, "macho_architectures", return_value=("arm64", "x86_64")):
                assembly.package_polaris_cli(source, contents)
            source.write_bytes(b"replacement CLI")
            packaged = contents / "Helpers/polaris"
            manifest = json.loads((contents / "Resources/polaris-cli.json").read_text())
            self.assertEqual(packaged.read_bytes(), b"fixed CLI")
            self.assertEqual(manifest["architectures"], ["arm64", "x86_64"])
            self.assertEqual(manifest["sha256"], hashlib.sha256(packaged.read_bytes()).hexdigest())

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
