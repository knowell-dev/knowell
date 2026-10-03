"""End-to-end test of scripts/install.sh against a fake local release (no network).

Needs `sh`, `curl`, `awk` and a sha256 tool on PATH; skipped otherwise. `uname` is shimmed
so the test runs the Linux code path on any host.
"""

import hashlib
import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent.parent / "scripts" / "install.sh"
TARGET = "x86_64-unknown-linux-gnu"
VERSION = "9.8.7"
HAVE_TOOLS = all(shutil.which(t) for t in ("sh", "curl", "awk"))
SHELL = shutil.which("sh")


def posix(path: Path) -> str:
    """Path in the form a POSIX shell (including Git Bash on Windows) accepts."""
    text = path.resolve().as_posix()
    if len(text) > 2 and text[1] == ":":
        return f"/{text[0].lower()}{text[2:]}"
    return text


@unittest.skipUnless(HAVE_TOOLS, "sh/curl/awk not available")
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
        ldd = self.shim / "ldd"
        ldd.write_text('#!/bin/sh\nprintf "ldd (GNU libc) 2.35\\n"\n', newline="\n")
        ldd.chmod(0o755)
        getconf = self.shim / "getconf"
        getconf.write_text('#!/bin/sh\nexit 1\n', newline="\n")
        getconf.chmod(0o755)
        self.make_release(b"#!/bin/sh\necho know fake\n")

    def make_release(self, binary: bytes, sums_digest: str | None = None, target: str = TARGET):
        engine_digest = sums_digest or hashlib.sha256(binary).hexdigest()
        engine_name = f"{engine_digest}.knowell-{VERSION}-{target}-engine"
        launcher = b"#!/bin/sh\necho launcher fake\n"
        launcher_digest = hashlib.sha256(launcher).hexdigest()
        launcher_name = f"{launcher_digest}.knowell-{VERSION}-{target}-launcher"
        (self.release / engine_name).write_bytes(binary)
        (self.release / launcher_name).write_bytes(launcher)
        (self.release / "SHA256SUMS").write_text(f"{engine_digest}  {engine_name}\n{launcher_digest}  {launcher_name}\n", newline="\n")

    def run_script(self, *args, extra_env=None):
        env = dict(os.environ)
        env["PATH"] = f"{posix(self.shim)}{os.pathsep}{env['PATH']}"
        # Windows curl.exe wants file:///C:/..., POSIX curl wants file:///abs/path.
        env["KNOWELL_DOWNLOAD_BASE"] = "file:///" + (self.root / "dl").resolve().as_posix().lstrip("/")
        env["HOME"] = posix(self.root / "home")
        env.pop("KNOWELL_INSTALL_DIR", None)
        env.update(extra_env or {})
        return subprocess.run([SHELL, str(SCRIPT), *args], capture_output=True, text=True, env=env, check=False)

    def test_libc_requires_an_explicit_gnu_or_musl_identity(self):
        ldd = self.shim / "ldd"
        getconf = self.shim / "getconf"
        musl = "x86_64-unknown-linux-musl"
        for label, ldd_output, getconf_output, target in (
                ("gnu-ldd", "ldd (GNU libc) 2.35", "", TARGET),
                ("gnu-getconf", "KNOWELL_CANARY_UNKNOWN", "glibc 2.35", TARGET),
                ("musl", "musl libc (x86_64)", "", musl)):
            ldd.write_text(f"#!/bin/sh\nprintf '%s\\n' '{ldd_output}'\n", newline="\n")
            getconf.write_text(f"#!/bin/sh\nprintf '%s\\n' '{getconf_output}'\n", newline="\n")
            self.make_release(b"KNOWELL_CANARY_ENGINE", target=target)
            destination = self.root / label
            result = self.run_script("--version", VERSION, "--install-dir", posix(destination), "--attestation", "skip")
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(json.loads((destination / "install.json").read_text())["target"], target)
        for label in ("missing", "unrecognized"):
            restricted = self.root / label
            restricted.mkdir()
            for tool in ("curl", "awk", "head", "grep"):
                actual = shutil.which(tool)
                self.assertIsNotNone(actual)
                shim = restricted / tool
                shim.write_text(f"#!/bin/sh\nexec '{posix(Path(actual))}' \"$@\"\n", newline="\n")
                shim.chmod(0o755)
            uname = restricted / "uname"
            uname.write_text('#!/bin/sh\ncase "$1" in -s) echo Linux;; -m) echo x86_64;; esac\n', newline="\n")
            uname.chmod(0o755)
            if label == "unrecognized":
                for tool in ("ldd", "getconf"):
                    shim = restricted / tool
                    shim.write_text('#!/bin/sh\necho KNOWELL_CANARY_UNKNOWN\n', newline="\n")
                    shim.chmod(0o755)
            destination = self.root / (label + "-install")
            result = self.run_script("--version", VERSION, "--install-dir", posix(destination),
                                     "--attestation", "skip", extra_env={"PATH": posix(restricted)})
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("cannot identify Linux libc", result.stderr)
            self.assertNotIn("Downloading", result.stdout)
            self.assertFalse(destination.exists())

    def test_installs_and_verifies(self):
        dest = self.root / "bin"
        result = self.run_script("--version", VERSION, "--install-dir", posix(dest), "--attestation", "skip")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Checksum verified", result.stdout)
        self.assertTrue((dest / "know").is_file())
        self.assertIn(b"launcher fake", (dest / "know").read_bytes())
        runtime = dest / "versions" / VERSION / TARGET / "know"
        self.assertIn(b"know fake", runtime.read_bytes())
        receipt = json.loads((dest / "install.json").read_text())
        image = json.loads((dest / "current.json").read_text())
        launcher = json.loads((dest / "launcher.json").read_text())
        self.assertEqual(receipt, {"format_version": 1, "owner": "direct", "target": TARGET, "launcher_protocol": 1})
        self.assertEqual(image["sha256"], hashlib.sha256(runtime.read_bytes()).hexdigest())
        self.assertEqual(image["size"], runtime.stat().st_size)
        self.assertEqual(launcher["sha256"], hashlib.sha256((dest / "know").read_bytes()).hexdigest())

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
        for args in (["--version", "1.0"], ["--version", "0.9.0"], ["--version", "1.0.0-rc.1"], ["--bogus"], ["--attestation", "maybe"]):
            result = self.run_script(*args)
            self.assertNotEqual(result.returncode, 0, args)

    def test_invalid_semver_is_rejected_before_download(self):
        for version in ("1.1.0-rc.01", "18446744073709551616.1.0", "1.1.0+build", "01.1.0"):
            result = self.run_script("--version", version, "--attestation", "skip")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("invalid version", result.stderr)
            self.assertNotIn("Downloading", result.stdout)

    @unittest.skipUnless(os.name == "posix", "requires native POSIX symlink semantics; exercised by Linux release CI")
    def test_default_dir_uses_home(self):
        result = self.run_script("--version", VERSION, "--attestation", "skip")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        installed = self.root / "home" / ".local" / "share" / "knowell" / "install" / "know"
        self.assertTrue(installed.is_file())
        entrypoint = self.root / "home" / ".local" / "bin" / "know"
        self.assertTrue(entrypoint.is_file())
        self.assertEqual(entrypoint.resolve(), installed.resolve())

    def test_emulated_symlink_is_rejected_before_publication(self):
        shim = self.shim / "ln"
        shim.write_text('#!/bin/sh\ncp "$2" "$3"\n', newline="\n")
        shim.chmod(0o755)
        result = self.run_script("--version", VERSION, "--attestation", "skip")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("emulates symbolic links", result.stderr)
        self.assertFalse((self.root / "home" / ".local" / "share" / "knowell" / "install").exists())
        self.assertFalse((self.root / "home" / ".local" / "bin" / "know").exists())

    def test_existing_installation_and_unmanaged_command_are_never_overwritten(self):
        dest = self.root / "existing"
        dest.mkdir()
        binary = dest / "know"
        binary.write_bytes(b"KNOWELL_CANARY_EXISTING_BINARY")
        result = self.run_script("--version", VERSION, "--install-dir", posix(dest), "--attestation", "skip")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("already exists", result.stderr)
        self.assertEqual(binary.read_bytes(), b"KNOWELL_CANARY_EXISTING_BINARY")

    def test_competing_bootstrap_lock_is_not_removed(self):
        dest = self.root / "install"
        lock = self.root / "install.bootstrap-lock"
        lock.mkdir()
        result = self.run_script("--version", VERSION, "--install-dir", posix(dest), "--attestation", "skip")
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(lock.is_dir())
        self.assertFalse(dest.exists())

    @unittest.skipUnless(os.name == "posix", "requires native POSIX ownership and mode semantics")
    def test_shared_parent_is_refused_without_changing_its_permissions(self):
        parent = self.root / "shared"
        parent.mkdir(mode=0o777)
        parent.chmod(0o777)
        result = self.run_script("--version", VERSION, "--install-dir", posix(parent / "install"), "--attestation", "skip")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("private dedicated subdirectory", result.stderr)
        self.assertEqual(parent.stat().st_mode & 0o777, 0o777)
        self.assertFalse((parent / "install").exists())
        self.assertFalse((parent / "install.bootstrap-lock").exists())

    def test_duplicate_component_or_missing_launcher_installs_nothing(self):
        sums = self.release / "SHA256SUMS"
        original = sums.read_text()
        for content in (original + original.splitlines()[0] + "\n", original.splitlines()[0] + "\n"):
            sums.write_text(content)
            dest = self.root / "install"
            result = self.run_script("--version", VERSION, "--install-dir", posix(dest), "--attestation", "skip")
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(dest.exists())
            self.assertFalse((self.root / "install.bootstrap-lock").exists())

    def test_unknown_length_download_is_bounded_even_if_curl_ignores_max_filesize(self):
        payload = self.root / "oversized"
        payload.write_bytes(b"K" * (2 * 1024 * 1024))
        curl = self.shim / "curl"
        curl.write_text(f"#!/bin/sh\ncat '{posix(payload)}'\nexit 0\n", newline="\n")
        curl.chmod(0o755)
        actual_head = shutil.which("head")
        self.assertIsNotNone(actual_head)
        observed = self.root / "observed"
        head = self.shim / "head"
        head.write_text(f"#!/bin/sh\n'{posix(Path(actual_head))}' \"$@\" | tee '{posix(observed)}'\n", newline="\n")
        head.chmod(0o755)
        destination = self.root / "install"
        result = self.run_script("--version", VERSION, "--install-dir", posix(destination), "--attestation", "skip")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("byte limit", result.stderr)
        self.assertEqual(observed.stat().st_size, 1024 * 1024 + 1)
        self.assertFalse(destination.exists())
        self.assertFalse((self.root / "install.bootstrap-lock").exists())

    def test_curl_failure_is_not_hidden_by_a_successful_bounded_consumer(self):
        sums = posix(self.release / "SHA256SUMS")
        curl = self.shim / "curl"
        # A server failure after complete-looking metadata must still stop bootstrap.
        scripts = (f"cat '{sums}'\nexit 22\n",
                   f"for arg do case \"$arg\" in */SHA256SUMS) cat '{sums}'; exit 0 ;; esac; done\nprintf 'KNOWELL_CANARY_TRUNCATED'\nexit 18\n")
        for contents in scripts:
            curl.write_text("#!/bin/sh\n" + contents, newline="\n")
            curl.chmod(0o755)
            destination = self.root / "install"
            result = self.run_script("--version", VERSION, "--install-dir", posix(destination), "--attestation", "skip")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("download failed or was truncated", result.stderr)
            self.assertFalse(destination.exists())
            self.assertFalse((self.root / "install.bootstrap-lock").exists())


if __name__ == "__main__":
    unittest.main()
