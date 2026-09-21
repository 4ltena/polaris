import importlib.util
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
            self.assertTrue((destination / assembly.RESOURCE_BUNDLE).is_dir())

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
