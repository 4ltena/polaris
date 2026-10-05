import importlib.util
import hashlib
import json
from pathlib import Path
import plistlib
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("assemble-installer.py")
SPEC = importlib.util.spec_from_file_location("installer_assembly", SCRIPT)
assembly = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(assembly)


class InstallerAssemblyTests(unittest.TestCase):
    def payload(self, root):
        app = root / "Polaris.app"
        (app / "Contents/MacOS").mkdir(parents=True)
        (app / assembly.RESOURCE_BUNDLE).mkdir()
        binary = app / "Contents/MacOS/PolarisDesktop"
        binary.write_bytes(b"native executable")
        binary.chmod(0o755)
        cli = app / "Contents/Helpers/polaris"
        cli.parent.mkdir()
        cli.write_bytes(b"compiled polaris CLI")
        cli.chmod(0o755)
        (app / "Contents/Resources").mkdir()
        (app / "Contents/Resources" / assembly.CLI_MANIFEST_NAME).write_text(json.dumps({
            "schema_version": 1,
            "relative_path": assembly.CLI_RELATIVE_PATH,
            "architectures": ["arm64"],
            "sha256": hashlib.sha256(cli.read_bytes()).hexdigest(),
        }))
        with (app / "Contents/Info.plist").open("wb") as stream:
            plistlib.dump({
                "CFBundlePackageType": "APPL", "CFBundleIdentifier": "com.polaris.desktop.local",
                "CFBundleExecutable": "PolarisDesktop",
                "CFBundleShortVersionString": "1.0.0", "CFBundleVersion": "1.0.0",
                "LSMinimumSystemVersion": "13.0",
            }, stream)
        return app

    def test_installer_contains_valid_payload_and_uses_installer_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            payload = self.payload(root)
            destination = root / "Polaris Installer.app"
            assembly.assemble_installer(payload, destination)
            with (destination / "Contents/Info.plist").open("rb") as stream:
                info = plistlib.load(stream)
            self.assertEqual(info["CFBundleIdentifier"], "com.polaris.installer.local")
            self.assertEqual(info["CFBundleShortVersionString"], "1.0.0")
            self.assertEqual((destination / "Contents/Resources/Polaris.app/Contents/MacOS/PolarisDesktop").read_bytes(),
                             b"native executable")
            self.assertEqual((destination / "Contents/Resources/Polaris.app/Contents/Helpers/polaris").read_bytes(),
                             b"compiled polaris CLI")
            self.assertTrue((destination / assembly.RESOURCE_BUNDLE).is_dir())

    def test_rejects_modified_cli_payload(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            payload = self.payload(root)
            (payload / "Contents/Helpers/polaris").write_bytes(b"modified")
            with self.assertRaises(RuntimeError):
                assembly.assemble_installer(payload, root / "Polaris Installer.app")

    def test_existing_destination_is_not_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            payload = self.payload(root)
            destination = root / "Polaris Installer.app"
            destination.mkdir()
            with self.assertRaises(RuntimeError):
                assembly.assemble_installer(payload, destination)


if __name__ == "__main__":
    unittest.main()
