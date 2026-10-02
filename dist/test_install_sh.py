"""End-to-end test of scripts/install.sh against a fake local release (no network).

Needs `sh`, `tar`, `curl` and a sha256 tool on PATH; skipped otherwise. `uname` is shimmed
so the test runs the Linux code path on any host.
"""

import hashlib
import io
import os
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent.parent / "scripts" / "install.sh"
TARGET = "x86_64-unknown-linux-gnu"
VERSION = "9.8.7"
HAVE_TOOLS = all(shutil.which(t) for t in ("sh", "tar", "curl", "awk"))


def posix(path: Path) -> str:
    """Path in the form a POSIX shell (including Git Bash on Windows) accepts."""
    text = path.resolve().as_posix()
    if len(text) > 2 and text[1] == ":":
        return f"/{text[0].lower()}{text[2:]}"
    return text


@unittest.skipUnless(HAVE_TOOLS, "sh/tar/curl/awk not available")
class InstallScript(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root, True)
        self.release = self.root / "dl" / f"v{VERSION}"
        self.release.mkdir(parents=True)
        self.shim = self.root / "shim"
        self.shim.mkdir()
        uname = self.shim / "uname"
        uname.write_text('#!/bin/sh\ncase "$1" in -s) echo Linux;; -m) echo x86_64;; *) echo Linux;; esac\n', newline="\n")
        uname.chmod(0o755)
        self.make_release(b"#!/bin/sh\necho know fake\n")

    def make_release(self, binary: bytes, sums_digest: str | None = None):
        name = f"knowell-{VERSION}-{TARGET}"
        archive = self.release / f"{name}.tar.gz"
        with tarfile.open(archive, "w:gz") as tf:
            info = tarfile.TarInfo(f"{name}/know")
            info.size = len(binary)
            info.mode = 0o755
            tf.addfile(info, io.BytesIO(binary))
        digest = sums_digest or hashlib.sha256(archive.read_bytes()).hexdigest()
        (self.release / "SHA256SUMS").write_text(f"{digest}  {archive.name}\n", newline="\n")

    def run_script(self, *args, extra_env=None):
        env = dict(os.environ)
        env["PATH"] = f"{posix(self.shim)}{os.pathsep}{env['PATH']}"
        # Windows curl.exe wants file:///C:/..., POSIX curl wants file:///abs/path.
        env["KNOWELL_DOWNLOAD_BASE"] = "file:///" + (self.root / "dl").resolve().as_posix().lstrip("/")
        env["HOME"] = posix(self.root / "home")
        env.pop("KNOWELL_INSTALL_DIR", None)
        env.update(extra_env or {})
        return subprocess.run(["sh", str(SCRIPT), *args], capture_output=True, text=True, env=env, check=False)

    def test_installs_and_verifies(self):
        dest = self.root / "bin"
        result = self.run_script("--version", VERSION, "--install-dir", posix(dest), "--attestation", "skip")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Checksum verified", result.stdout)
        self.assertTrue((dest / "know").is_file())
        self.assertIn(b"know fake", (dest / "know").read_bytes())

    def test_checksum_mismatch_installs_nothing(self):
        self.make_release(b"tampered", sums_digest="0" * 64)
        dest = self.root / "bin"
        result = self.run_script("--version", f"v{VERSION}", "--install-dir", posix(dest), "--attestation", "skip")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertFalse((dest / "know").exists())

    def write_gh(self, *lines):
        gh = self.shim / "gh"
        gh.write_text(chr(10).join(["#!/bin/sh", *lines, ""]), newline=chr(10))
        gh.chmod(0o755)

    def test_attestation_require_without_login_fails(self):
        self.write_gh("exit 1")  # installed but not authenticated
        dest = self.root / "bin"
        result = self.run_script("--version", VERSION, "--install-dir", posix(dest), "--attestation", "require")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("gh", result.stderr)
        self.assertFalse((dest / "know").exists())

    def test_attestation_auto_without_login_continues(self):
        self.write_gh("exit 1")
        result = self.run_script("--version", VERSION, "--install-dir", posix(self.root / "bin"))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Skipping attestation", result.stdout)

    def test_failed_attestation_installs_nothing(self):
        self.write_gh('[ "$1" = auth ] && exit 0', "exit 1")  # logged in, verification fails
        dest = self.root / "bin"
        result = self.run_script("--version", VERSION, "--install-dir", posix(dest))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("attestation verification failed", result.stderr)
        self.assertFalse((dest / "know").exists())

    def test_bad_arguments(self):
        for args in (["--version", "1.0"], ["--bogus"], ["--attestation", "maybe"]):
            result = self.run_script(*args)
            self.assertNotEqual(result.returncode, 0, args)

    def test_default_dir_uses_home(self):
        result = self.run_script("--version", VERSION, "--attestation", "skip")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue((self.root / "home" / ".local" / "bin" / "know").is_file())


if __name__ == "__main__":
    unittest.main()
