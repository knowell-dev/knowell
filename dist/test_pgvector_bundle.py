"""Offline tests for dist/pgvector_bundle.py."""

import hashlib
import json
import shutil
import tarfile
import tempfile
import unittest
from pathlib import Path

import pgvector_bundle as pb

PINS = json.loads((Path(pb.__file__).parent / "pg-binaries.json").read_text(encoding="utf-8"))


class TempDirCase(unittest.TestCase):
    def tmp(self) -> Path:
        path = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, path, True)
        return path


class Pins(TempDirCase):
    def test_every_pinned_target_has_a_digest(self):
        for major, entry in PINS["majors"].items():
            self.assertIn(major, ("17", "18"))
            self.assertTrue(entry["version"].startswith(f"{major}."))
            self.assertEqual(len(entry["targets"]), 5)
            for target in entry["targets"].values():
                self.assertRegex(target["sha256"], r"^[0-9a-f]{64}$")
                self.assertIn(entry["version"], target["asset"])

    def test_lookup_and_unknown(self):
        asset = pb.pg_asset("18", "x86_64-pc-windows-msvc", PINS)
        self.assertTrue(asset["url"].startswith("https://github.com/theseus-rs/postgresql-binaries/"))
        self.assertTrue(asset["asset"].endswith(".zip"))
        with self.assertRaises(pb.BundleError):
            pb.pg_asset("16", "x86_64-unknown-linux-gnu", PINS)
        with self.assertRaises(pb.BundleError):
            pb.pg_asset("17", "aarch64-pc-windows-msvc", PINS)

    def test_download_refuses_http_and_bad_digest_length(self):
        with self.assertRaises(pb.BundleError):
            pb.download_verified("http://example.invalid/x", "a" * 64, self.tmp() / "x")
        with self.assertRaises(pb.BundleError):
            pb.download_verified("https://example.invalid/x", "abc", self.tmp() / "x")


class Package(TempDirCase):
    def installed_tree(self, version="0.8.7", ext="so", with_sql=True) -> Path:
        root = self.tmp()
        (root / "lib").mkdir()
        (root / "share" / "extension").mkdir(parents=True)
        (root / "lib" / f"vector.{ext}").write_bytes(b"binary")
        (root / "share" / "extension" / "vector.control").write_text("comment = 'x'\n")
        if with_sql:
            (root / "share" / "extension" / f"vector--{version}.sql").write_text("-- sql\n")
            (root / "share" / "extension" / f"vector--0.8.6--{version}.sql").write_text("-- up\n")
        return root

    def source_tree(self) -> Path:
        src = self.tmp()
        (src / "LICENSE").write_text("licence text\n")
        return src

    def run_package(self, root, target="x86_64-unknown-linux-gnu"):
        out = self.tmp()
        archive, manifest = pb.package(
            root, self.source_tree(), target, "17", "17.11.0", "0.8.7", "https://example/src.tgz", "a" * 64, out
        )
        return archive, manifest

    def test_layout_and_manifest(self):
        archive, manifest_path = self.run_package(self.installed_tree())
        with tarfile.open(archive) as tf:
            names = sorted(m.name for m in tf.getmembers() if m.isfile())
            self.assertEqual(
                names,
                [
                    "LICENSE-pgvector",
                    "lib/vector.so",
                    "manifest.json",
                    "share/extension/vector--0.8.6--0.8.7.sql",
                    "share/extension/vector--0.8.7.sql",
                    "share/extension/vector.control",
                ],
            )
            inner = json.loads(tf.extractfile("manifest.json").read())
        self.assertEqual(inner, json.loads(manifest_path.read_text()))
        self.assertEqual(inner["pg_major"], 17)
        self.assertEqual(inner["pgvector_version"], "0.8.7")
        self.assertEqual(inner["target"], "x86_64-unknown-linux-gnu")
        lib_entry = next(f for f in inner["files"] if f["path"] == "lib/vector.so")
        self.assertEqual(lib_entry["sha256"], hashlib.sha256(b"binary").hexdigest())

    def test_archive_is_reproducible(self):
        a, _ = self.run_package(self.installed_tree())
        b, _ = self.run_package(self.installed_tree())
        self.assertEqual(pb.sha256_file(a), pb.sha256_file(b))

    def test_platform_library_names(self):
        self.assertEqual(pb.library_name("aarch64-apple-darwin"), "vector.dylib")
        self.assertEqual(pb.library_name("x86_64-pc-windows-msvc"), "vector.dll")
        archive, _ = self.run_package(self.installed_tree(ext="dll"), "x86_64-pc-windows-msvc")
        with tarfile.open(archive) as tf:
            self.assertIn("lib/vector.dll", tf.getnames())

    def test_missing_pieces_are_errors(self):
        with self.assertRaises(pb.BundleError):
            self.run_package(self.installed_tree(with_sql=False))
        with self.assertRaises(pb.BundleError):
            self.run_package(self.installed_tree(ext="dylib"))  # wrong library for the target

    def test_version_pin_mismatch_is_an_error(self):
        with self.assertRaises(pb.BundleError):
            self.run_package(self.installed_tree(version="0.8.6"))


if __name__ == "__main__":
    unittest.main()
