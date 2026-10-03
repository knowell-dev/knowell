"""Windows bootstrap tests use synthetic bytes and never modify the user's PATH."""

import hashlib
import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent.parent / "scripts" / "install.ps1"
POWERSHELL = shutil.which("pwsh") or shutil.which("powershell")
VERSION = "9.8.7"
TARGET = "x86_64-pc-windows-msvc"


@unittest.skipUnless(os.name == "nt" and POWERSHELL, "Windows PowerShell integration requires Windows")
class InstallPowerShell(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root, True)
        self.release = self.root / "downloads" / f"v{VERSION}"
        self.release.mkdir(parents=True)
        self.engine = b"KNOWELL_CANARY_SYNTHETIC_ENGINE"
        self.launcher = b"KNOWELL_CANARY_SYNTHETIC_LAUNCHER"
        sums = []
        for component, data in (("engine", self.engine), ("launcher", self.launcher)):
            digest = hashlib.sha256(data).hexdigest()
            name = f"{digest}.knowell-{VERSION}-{TARGET}-{component}.exe"
            (self.release / name).write_bytes(data)
            sums.append(f"{digest}  {name}\n")
        (self.release / "SHA256SUMS").write_text("".join(sums))

    def run_script(self, destination, *extra, version=VERSION):
        env = dict(os.environ)
        env.update(KNOWELL_DOWNLOAD_BASE=(self.root / "downloads").as_uri(),
                   KNOWELL_NO_PATH="1", PROCESSOR_ARCHITECTURE="AMD64", PROCESSOR_ARCHITEW6432="AMD64")
        return subprocess.run([POWERSHELL, "-NoProfile", "-NonInteractive", "-File", str(SCRIPT),
                               "-Version", version, "-InstallDir", str(destination), "-Attestation", "skip", *extra],
                              capture_output=True, text=True, env=env, timeout=60, check=False)

    def test_private_immutable_install_and_receipts(self):
        destination = self.root / "installation"
        result = self.run_script(destination)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((destination / "know.exe").read_bytes(), self.launcher)
        runtime = destination / "versions" / VERSION / TARGET / "know.exe"
        self.assertEqual(runtime.read_bytes(), self.engine)
        image = json.loads((destination / "current.json").read_text())
        self.assertEqual(image["format_version"], 1)
        self.assertEqual(image["sha256"], hashlib.sha256(self.engine).hexdigest())
        self.assertEqual(image["size"], len(self.engine))
        self.assertEqual(json.loads((destination / "install.json").read_text())["owner"], "direct")
        acl = subprocess.run([POWERSHELL, "-NoProfile", "-NonInteractive", "-Command",
                              "$acl = Get-Acl -LiteralPath $env:KNOWELL_CANARY_ACL_PATH; if (-not $acl.AreAccessRulesProtected) { exit 1 }; if ($acl.Access | Where-Object { $_.IdentityReference.Value -eq 'Everyone' }) { exit 2 }"],
                             env={**os.environ, "KNOWELL_CANARY_ACL_PATH": str(destination)},
                             capture_output=True, text=True, timeout=30, check=False)
        self.assertEqual(acl.returncode, 0, acl.stderr)

    def test_existing_root_is_refused_without_touching_bytes(self):
        destination = self.root / "installation"
        destination.mkdir()
        binary = destination / "know.exe"
        binary.write_bytes(b"KNOWELL_CANARY_UNMANAGED")
        result = self.run_script(destination)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("already exists", result.stderr)
        self.assertEqual(binary.read_bytes(), b"KNOWELL_CANARY_UNMANAGED")

    def test_tampered_component_installs_nothing(self):
        runtime = next(self.release.glob("*-engine.exe"))
        runtime.write_bytes(b"KNOWELL_CANARY_TAMPERED")
        destination = self.root / "installation"
        result = self.run_script(destination)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertFalse(destination.exists())
        self.assertFalse(Path(str(destination) + ".bootstrap-lock").exists())

    def test_duplicate_component_is_rejected(self):
        sums = self.release / "SHA256SUMS"
        sums.write_text(sums.read_text() * 2)
        destination = self.root / "installation"
        result = self.run_script(destination)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("exactly one", result.stderr)
        self.assertFalse(destination.exists())

    def test_public_gate_and_invalid_semver_before_download(self):
        for version in ("0.9.0", "1.0.0-rc.1", "1.1.0-rc.01", "18446744073709551616.1.0", "1.1.0+build"):
            destination = self.root / "installation"
            result = self.run_script(destination, version=version)
            self.assertNotEqual(result.returncode, 0)
            self.assertNotIn("Downloading", result.stdout)
            self.assertFalse(destination.exists())


if __name__ == "__main__":
    unittest.main()
