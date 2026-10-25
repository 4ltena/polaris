import hashlib
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import tomllib
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).with_name("assemble-cli-pkg.py")
SPEC = importlib.util.spec_from_file_location("cli_package_assembly", SCRIPT)
assembly = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(assembly)
VERIFY_SPEC = importlib.util.spec_from_file_location("cli_package_verification", SCRIPT.with_name("verify-cli-pkg.py"))
verification = importlib.util.module_from_spec(VERIFY_SPEC)
VERIFY_SPEC.loader.exec_module(verification)
REPOSITORY = SCRIPT.resolve().parents[3]
WORKSPACE_VERSION = tomllib.loads((REPOSITORY / "Cargo.toml").read_text())["workspace"]["package"]["version"]


class CLIPackageAssemblyTests(unittest.TestCase):
    def test_package_contains_cli_and_refuses_unrelated_command(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "polaris"
            binary.write_bytes(b"preview CLI")
            binary.chmod(0o755)
            package = root / f"polaris-cli-preview-{WORKSPACE_VERSION}-arm64.pkg"
            with patch.object(assembly, "architecture", return_value="arm64"), \
                 patch.object(assembly, "binary_version", return_value=f"polaris {WORKSPACE_VERSION}"):
                assembly.build_package(binary, package, REPOSITORY)
            payload = subprocess.run(["/usr/sbin/pkgutil", "--payload-files", str(package)],
                                     check=True, capture_output=True, text=True).stdout
            self.assertIn("usr/local/bin/polaris", payload)
            regular_files = [line for line in payload.splitlines()
                             if line.startswith("./") and not line.endswith(("/usr", "/local", "/bin"))
                             and line != "./usr/local/bin/polaris"
                             and not Path(line).name.startswith("._")]
            self.assertEqual(regular_files, [])
            expanded = root / "expanded"
            subprocess.run(["/usr/sbin/pkgutil", "--expand", str(package), str(expanded)],
                           check=True, capture_output=True)
            preinstall = (expanded / "Scripts/preinstall").read_text(encoding="utf-8")
            self.assertIn(hashlib.sha256(binary.read_bytes()).hexdigest(), preinstall)
            self.assertIn("existing command occupies", preinstall)
            command = root / "installed-command"
            fixture = root / "preinstall-fixture"
            fixture.write_text(preinstall.replace("target=/usr/local/bin/polaris",
                                                  f"target={command}"), encoding="utf-8")
            self.assertEqual(subprocess.run(["/bin/sh", str(fixture)], capture_output=True).returncode, 0)
            command.write_bytes(b"unrelated executable")
            self.assertNotEqual(subprocess.run(["/bin/sh", str(fixture)], capture_output=True).returncode, 0)
            command.write_bytes(binary.read_bytes())
            self.assertEqual(subprocess.run(["/bin/sh", str(fixture)], capture_output=True).returncode, 0)
            command.unlink()
            command.symlink_to(binary)
            self.assertNotEqual(subprocess.run(["/bin/sh", str(fixture)], capture_output=True).returncode, 0)
            with self.assertRaises(FileExistsError):
                with patch.object(assembly, "architecture", return_value="arm64"), \
                     patch.object(assembly, "binary_version", return_value=f"polaris {WORKSPACE_VERSION}"):
                    assembly.build_package(binary, package, REPOSITORY)

    def test_rejects_binary_from_another_version(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "polaris"
            binary.write_bytes(b"wrong CLI")
            binary.chmod(0o755)
            with patch.object(assembly, "architecture", return_value="arm64"), \
                 patch.object(assembly, "binary_version", return_value="polaris 0.11.0"):
                with self.assertRaisesRegex(ValueError, "version does not match"):
                    assembly.build_package(binary, root / "wrong.pkg", REPOSITORY)

    def test_verifies_packaged_command_and_rejects_changed_source(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "polaris"
            binary.write_text(
                f"#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'polaris {WORKSPACE_VERSION}'; "
                "else echo help; fi\n", encoding="utf-8")
            binary.chmod(0o755)
            package = root / f"polaris-cli-preview-{WORKSPACE_VERSION}-arm64.pkg"
            with patch.object(assembly, "architecture", return_value="arm64"):
                assembly.build_package(binary, package, REPOSITORY)
            self.assertEqual(verification.verify(package, binary),
                             hashlib.sha256(binary.read_bytes()).hexdigest())
            binary.write_text(binary.read_text() + "# changed after packaging\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "differs"):
                verification.verify(package, binary)

    def test_payload_allowlist_rejects_traversal(self):
        self.assertTrue(verification.allowed_payload_path("./usr/local/bin/polaris"))
        self.assertTrue(verification.allowed_payload_path("./usr/local/bin/._polaris"))
        self.assertFalse(verification.allowed_payload_path("./usr/local/bin/../other"))
        self.assertFalse(verification.allowed_payload_path("./usr/local/bin/unrelated"))

    def test_rejects_intel_binary_with_arm64_package_name(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "polaris"
            binary.write_bytes(b"Intel CLI")
            binary.chmod(0o755)
            package = root / f"polaris-cli-preview-{WORKSPACE_VERSION}-arm64.pkg"
            with patch.object(assembly, "architecture", return_value="x86_64"), \
                 patch.object(assembly, "binary_version", return_value=f"polaris {WORKSPACE_VERSION}"):
                with self.assertRaisesRegex(ValueError, "filename must match"):
                    assembly.build_package(binary, package, REPOSITORY)
            self.assertFalse(package.exists())


if __name__ == "__main__":
    unittest.main()
