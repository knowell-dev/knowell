"""Offline tests for the (unpublished) PyPI launcher scaffold."""

import hashlib
import http.server
import io
import shutil
import sys
import tarfile
import tempfile
import threading
import unittest
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "src"))

from knowell import launcher  # noqa: E402

VERSION = "9.8.7"


class Platform(unittest.TestCase):
    def test_mapping(self):
        cases = [
            (("Linux", "x86_64", True), ("x86_64-unknown-linux-gnu", "tar.gz", "know")),
            (("Linux", "aarch64", False), ("aarch64-unknown-linux-musl", "tar.gz", "know")),
            (("Darwin", "arm64", True), ("aarch64-apple-darwin", "tar.gz", "know")),
            (("Darwin", "x86_64", True), ("x86_64-apple-darwin", "tar.gz", "know")),
            (("Windows", "AMD64", True), ("x86_64-pc-windows-msvc", "zip", "know.exe")),
            (("Windows", "ARM64", True), ("aarch64-pc-windows-msvc", "zip", "know.exe")),
        ]
        for args, expected in cases:
            self.assertEqual(launcher.resolve_target(*args), expected, args)

    def test_unsupported(self):
        with self.assertRaises(launcher.LauncherError):
            launcher.resolve_target("FreeBSD", "x86_64")
        with self.assertRaises(launcher.LauncherError):
            launcher.resolve_target("Linux", "riscv64")


class Checksums(unittest.TestCase):
    def test_parse_and_verify(self):
        tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp, True)
        f = tmp / "a.bin"
        f.write_bytes(b"hello")
        good = hashlib.sha256(b"hello").hexdigest()
        sums = launcher.parse_sums(f"{good.upper()} *a.bin\n")
        launcher.verify_file(f, "a.bin", sums)
        with self.assertRaises(launcher.LauncherError):
            launcher.verify_file(f, "a.bin", {"a.bin": "0" * 64})
        with self.assertRaises(launcher.LauncherError):
            launcher.verify_file(f, "missing", sums)
        for bad in ("nodigest", "xyz  f", f"{'a' * 64}  f\n{'b' * 64}  f"):
            with self.assertRaises(launcher.LauncherError):
                launcher.parse_sums(bad)


class Download(unittest.TestCase):
    def serve(self, tamper=False):
        root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, root, True)
        name = f"knowell-{VERSION}-x86_64-unknown-linux-gnu"
        rel = root / "srv" / f"v{VERSION}"
        rel.mkdir(parents=True)
        buf = io.BytesIO()
        with tarfile.open(fileobj=buf, mode="w:gz") as tf:
            data = b"#!/bin/sh\necho fake\n"
            info = tarfile.TarInfo(f"{name}/know")
            info.size = len(data)
            tf.addfile(info, io.BytesIO(data))
        (rel / f"{name}.tar.gz").write_bytes(buf.getvalue())
        digest = "0" * 64 if tamper else hashlib.sha256(buf.getvalue()).hexdigest()
        (rel / "SHA256SUMS").write_text(f"{digest}  {name}.tar.gz\n")

        directory = str(root / "srv")

        class Handler(http.server.SimpleHTTPRequestHandler):
            def __init__(self, *args, **kwargs):
                super().__init__(*args, directory=directory, **kwargs)

            def log_message(self, *args):
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        env = {"KNOWELL_DOWNLOAD_BASE": f"http://127.0.0.1:{server.server_address[1]}",
               "KNOWELL_CACHE_DIR": str(root / "cache")}
        return env, root / "cache"

    def test_download_verify_cache(self):
        env, cache = self.serve()
        exe = launcher.ensure_binary(VERSION, env, "Linux", "x86_64")
        self.assertEqual(exe, cache / VERSION / "x86_64-unknown-linux-gnu" / "know")
        self.assertIn(b"fake", exe.read_bytes())
        # Cached: a dead server is not contacted.
        env["KNOWELL_DOWNLOAD_BASE"] = "http://127.0.0.1:1"
        self.assertEqual(launcher.ensure_binary(VERSION, env, "Linux", "x86_64"), exe)
        self.assertEqual([p.name for p in (cache / VERSION).iterdir()], ["x86_64-unknown-linux-gnu"])

    def test_tampered_download_installs_nothing(self):
        env, cache = self.serve(tamper=True)
        with self.assertRaises(launcher.LauncherError):
            launcher.ensure_binary(VERSION, env, "Linux", "x86_64")
        self.assertEqual(list((cache / VERSION).iterdir()) if (cache / VERSION).exists() else [], [])

    def test_refuses_plain_http_and_bad_version(self):
        env = {"KNOWELL_DOWNLOAD_BASE": "http://example.invalid", "KNOWELL_CACHE_DIR": tempfile.mkdtemp()}
        self.addCleanup(shutil.rmtree, env["KNOWELL_CACHE_DIR"], True)
        with self.assertRaises(launcher.LauncherError):
            launcher.ensure_binary(VERSION, env, "Linux", "x86_64")
        with self.assertRaises(launcher.LauncherError):
            launcher.ensure_binary("1.0; rm", env, "Linux", "x86_64")

    def test_zip_extraction_on_windows(self):
        root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, root, True)
        name = f"knowell-{VERSION}-x86_64-pc-windows-msvc"
        rel = root / "srv" / f"v{VERSION}"
        rel.mkdir(parents=True)
        archive = rel / f"{name}.zip"
        with zipfile.ZipFile(archive, "w") as zf:
            zf.writestr(f"{name}/know.exe", "fake")
        (rel / "SHA256SUMS").write_text(f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}\n")
        directory = str(root / "srv")

        class Handler(http.server.SimpleHTTPRequestHandler):
            def __init__(self, *args, **kwargs):
                super().__init__(*args, directory=directory, **kwargs)

            def log_message(self, *args):
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        env = {"KNOWELL_DOWNLOAD_BASE": f"http://127.0.0.1:{server.server_address[1]}",
               "KNOWELL_CACHE_DIR": str(root / "cache")}
        exe = launcher.ensure_binary(VERSION, env, "Windows", "AMD64")
        self.assertEqual(exe.name, "know.exe")
        self.assertEqual(exe.read_bytes(), b"fake")


class CacheRoot(unittest.TestCase):
    def test_conventions(self):
        home = Path("/h")
        self.assertEqual(launcher.cache_root({"KNOWELL_CACHE_DIR": "/x"}, "Linux", home), Path("/x"))
        self.assertEqual(launcher.cache_root({}, "Linux", home), home / ".cache" / "knowell")
        self.assertEqual(launcher.cache_root({}, "Darwin", home), home / "Library" / "Caches" / "knowell")
        self.assertEqual(launcher.cache_root({"LOCALAPPDATA": "/L"}, "Windows", home), Path("/L") / "knowell" / "pypi")


if __name__ == "__main__":
    unittest.main()
