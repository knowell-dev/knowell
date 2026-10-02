"""Offline tests for dist/package_archive.py."""

import shutil
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path

import package_archive as pa


class Packaging(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.root = self.tmp / "repo"
        self.root.mkdir()
        for name in pa.EXTRA_FILES:
            (self.root / name).write_text(f"{name} text\n")
        self.binary = self.tmp / "know-bin"
        self.binary.write_bytes(b"\x7fELF fake")

    def test_tar_layout_modes_and_reproducibility(self):
        first = pa.build("1.2.3", "x86_64-unknown-linux-gnu", self.binary, self.tmp / "a", self.root)
        second = pa.build("1.2.3", "x86_64-unknown-linux-gnu", self.binary, self.tmp / "b", self.root)
        self.assertEqual(first.name, "knowell-1.2.3-x86_64-unknown-linux-gnu.tar.gz")
        self.assertEqual(first.read_bytes(), second.read_bytes())
        with tarfile.open(first) as tf:
            members = {m.name: m for m in tf.getmembers()}
        top = "knowell-1.2.3-x86_64-unknown-linux-gnu"
        self.assertEqual(
            sorted(members), [f"{top}/LICENSE-APACHE", f"{top}/LICENSE-MIT", f"{top}/README.md", f"{top}/know"]
        )
        self.assertEqual(members[f"{top}/know"].mode, 0o755)
        self.assertEqual(members[f"{top}/README.md"].mode, 0o644)
        self.assertTrue(all(m.uid == 0 and m.mtime == 0 for m in members.values()))

    def test_windows_gets_zip_with_exe(self):
        archive = pa.build("1.2.3", "aarch64-pc-windows-msvc", self.binary, self.tmp / "o", self.root)
        self.assertEqual(archive.name, "knowell-1.2.3-aarch64-pc-windows-msvc.zip")
        with zipfile.ZipFile(archive) as zf:
            names = zf.namelist()
        self.assertIn("knowell-1.2.3-aarch64-pc-windows-msvc/know.exe", names)
        self.assertEqual(len(names), 4)

    def test_bad_input(self):
        with self.assertRaises(pa.PackageError):
            pa.build("v1.2.3", "x86_64-unknown-linux-gnu", self.binary, self.tmp, self.root)
        with self.assertRaises(pa.PackageError):
            pa.build("1.2.3", "../evil", self.binary, self.tmp, self.root)
        with self.assertRaises(pa.PackageError):
            pa.build("1.2.3", "x86_64-unknown-linux-gnu", self.tmp / "missing", self.tmp, self.root)
        (self.root / "LICENSE-MIT").unlink()
        with self.assertRaises(pa.PackageError):
            pa.build("1.2.3", "x86_64-unknown-linux-gnu", self.binary, self.tmp, self.root)

    def test_names_match_render_expectations(self):
        import render

        for key, (target, _ext) in render.PLATFORMS.items():
            archive = pa.build("1.2.3", target, self.binary, self.tmp / key, self.root)
            self.assertEqual(archive.name, render.archive_name("1.2.3", key))


if __name__ == "__main__":
    unittest.main()
