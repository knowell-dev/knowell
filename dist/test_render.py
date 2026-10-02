"""Offline tests for dist/render.py. Run: python -m unittest discover -s dist -v"""

import json
import re
import shutil
import tempfile
import unittest
from pathlib import Path

import render

VERSION = "1.2.3"


def fake_sums(version: str = VERSION, skip: tuple[str, ...] = ()) -> str:
    lines = []
    for index, key in enumerate(render.PLATFORMS):
        if key in skip:
            continue
        digest = f"{index + 1:x}" * 64
        lines.append(f"{digest}  {render.archive_name(version, key)}")
    lines.append(f"{'f' * 64}  knowell_{version}_amd64.deb")
    return "\n".join(lines) + "\n"


class TempDirCase(unittest.TestCase):
    def tmp(self) -> Path:
        path = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, path, True)
        return path


class ParseSums(unittest.TestCase):
    def test_parses_text_and_binary_marker(self):
        sums = render.parse_sums(f"{'a' * 64}  one.zip\n{'B' * 64} *two.zip\n\n")
        self.assertEqual(sums, {"one.zip": "a" * 64, "two.zip": "b" * 64})

    def test_rejects_bad_digest(self):
        with self.assertRaises(render.RenderError):
            render.parse_sums("xyz  file\n")

    def test_rejects_conflicting_duplicates(self):
        with self.assertRaises(render.RenderError):
            render.parse_sums(f"{'a' * 64}  f\n{'b' * 64}  f\n")

    def test_rejects_missing_name(self):
        with self.assertRaises(render.RenderError):
            render.parse_sums("a" * 64 + "\n")


class Rendering(TempDirCase):
    def render_all(self, sums_text: str, version: str = VERSION) -> Path:
        out = self.tmp()
        sums = render.parse_sums(sums_text)
        for name in render.OUTPUTS:
            render.render_output(name, version, sums, "knowell-dev/knowell", out, "2030-01-02")
        return out

    def test_all_outputs_render_without_markers(self):
        out = self.render_all(fake_sums())
        files = [p for p in out.rglob("*") if p.is_file()]
        self.assertEqual(len(files), 5)
        for path in files:
            text = path.read_text(encoding="utf-8")
            self.assertNotIn("@@", text, path)
            self.assertIn(VERSION, text, path)
            self.assertNotIn("\r", text, path)

    def test_homebrew_has_urls_and_hashes(self):
        out = self.render_all(fake_sums())
        formula = (out / "Formula/knowell.rb").read_text(encoding="utf-8")
        self.assertIn(
            "https://github.com/knowell-dev/knowell/releases/download/v1.2.3/"
            "knowell-1.2.3-aarch64-apple-darwin.tar.gz",
            formula,
        )
        self.assertEqual(len(re.findall(r'sha256 "[0-9a-f]{64}"', formula)), 4)

    def test_scoop_manifest_is_valid_json(self):
        out = self.render_all(fake_sums())
        manifest = json.loads((out / "bucket/knowell.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["version"], VERSION)
        self.assertEqual(manifest["bin"], "know.exe")
        self.assertEqual(
            manifest["architecture"]["arm64"]["extract_dir"],
            "knowell-1.2.3-aarch64-pc-windows-msvc",
        )

    def test_winget_hashes_are_uppercase(self):
        out = self.render_all(fake_sums())
        installer = (out / "winget/Knowell.Knowell.installer.yaml").read_text(encoding="utf-8")
        hashes = re.findall(r"InstallerSha256: (\S+)", installer)
        self.assertEqual(len(hashes), 2)
        for value in hashes:
            self.assertRegex(value, r"^[0-9A-F]{64}$")
        self.assertIn("ReleaseDate: 2030-01-02", installer)

    def test_missing_archive_is_an_error(self):
        sums = render.parse_sums(fake_sums(skip=("WINDOWS_ARM64",)))
        with self.assertRaises(render.RenderError):
            render.render_output("scoop", VERSION, sums, "knowell-dev/knowell", self.tmp(), "2030-01-02")

    def test_unknown_placeholder_is_an_error(self):
        with self.assertRaises(render.RenderError):
            render.render_text("x @@NOPE@@", {}, set())

    def test_bad_version_and_repo_rejected(self):
        sums = render.parse_sums(fake_sums())
        for bad in ("v1.2.3", "1.2", "1.2.3; rm -rf /"):
            with self.assertRaises(render.RenderError):
                render.values_for(bad, sums, "knowell-dev/knowell", ["LINUX_X64"], "2030-01-02")
        with self.assertRaises(render.RenderError):
            render.values_for(VERSION, sums, "no-slash", ["LINUX_X64"], "2030-01-02")

    def test_prerelease_version_accepted(self):
        sums = render.parse_sums(fake_sums("1.0.0-rc.1"))
        out = self.tmp()
        render.render_output("homebrew", "1.0.0-rc.1", sums, "knowell-dev/knowell", out, "2030-01-02")
        self.assertIn("v1.0.0-rc.1", (out / "Formula/knowell.rb").read_text(encoding="utf-8"))


class Check(TempDirCase):
    def test_complete_and_incomplete(self):
        self.assertEqual(render.check_assets(VERSION, render.parse_sums(fake_sums())), [])
        missing = render.check_assets(VERSION, render.parse_sums(fake_sums(skip=("MACOS_X64",))))
        self.assertEqual(missing, [render.archive_name(VERSION, "MACOS_X64")])

    def test_cli_exit_codes(self):
        tmp = self.tmp()
        sums = tmp / "SHA256SUMS"
        sums.write_text(fake_sums(), encoding="utf-8")
        out = str(tmp / "o")
        self.assertEqual(render.main(["check", "--version", VERSION, "--sums", str(sums)]), 0)
        self.assertEqual(render.main(["--version", VERSION, "--sums", str(sums), "--out", out]), 0)
        self.assertEqual(render.main(["--version", "bad", "--sums", str(sums), "--out", out]), 2)


if __name__ == "__main__":
    unittest.main()
