#!/usr/bin/env python3
"""Helpers for the pgvector workflow: verified downloads and bundle packaging.

Subcommands (all offline-testable except the two fetchers):
    fetch-pg   --major 17 --target T --dest DIR   download + verify + extract PostgreSQL
    fetch-src  --url U --sha256 H --dest FILE     download + verify a source tarball
    package    --root DIR --src DIR --target T --pg-major 17 --pg-version V
               --pgvector-version V --source-url U --source-sha256 H --out DIR

The PostgreSQL binaries are the same theseus-rs/postgresql-binaries release assets the
managed mode installs; their pinned SHA-256 digests live in dist/pg-binaries.json.
A digest mismatch is fatal: nothing is built from an unverified download.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import sys
import tarfile
import urllib.request
import zipfile
from pathlib import Path

DIST = Path(__file__).resolve().parent
SHA_LEN = 64


class BundleError(Exception):
    """Something is missing, ambiguous or unverified; the message says what."""


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def download_verified(url: str, expected_sha256: str, dest: Path) -> None:
    """Download `url` to `dest`, failing (and deleting the file) on a digest mismatch."""
    if len(expected_sha256) != SHA_LEN:
        raise BundleError("expected digest must be a 64-character SHA-256")
    if not url.startswith("https://"):
        raise BundleError("downloads must use https")
    dest.parent.mkdir(parents=True, exist_ok=True)
    request = urllib.request.Request(url, headers={"User-Agent": "knowell-release"})
    with urllib.request.urlopen(request, timeout=300) as response, dest.open("wb") as out:
        shutil.copyfileobj(response, out)
    actual = sha256_file(dest)
    if actual != expected_sha256.lower():
        dest.unlink(missing_ok=True)
        raise BundleError(f"checksum mismatch for {url}: expected {expected_sha256}, got {actual}")


def pg_asset(major: str, target: str, pins: dict | None = None) -> dict:
    """Look up the pinned PostgreSQL asset for a major version and target triple."""
    pins = pins if pins is not None else json.loads((DIST / "pg-binaries.json").read_text(encoding="utf-8"))
    entry = pins["majors"].get(major)
    if entry is None:
        raise BundleError(f"no pinned PostgreSQL for major {major}")
    asset = entry["targets"].get(target)
    if asset is None:
        raise BundleError(f"no pinned PostgreSQL {major} binaries for target {target}")
    return {
        "version": entry["version"],
        "asset": asset["asset"],
        "sha256": asset["sha256"],
        "url": f"{pins['source']}/{entry['version']}/{asset['asset']}",
    }


def extract(archive: Path, dest: Path) -> Path:
    """Extract a .tar.gz or .zip and return the single top-level directory inside."""
    dest.mkdir(parents=True, exist_ok=True)
    if archive.name.endswith(".zip"):
        with zipfile.ZipFile(archive) as zf:
            zf.extractall(dest)  # zipfile strips absolute paths and ".." components
    else:
        with tarfile.open(archive) as tf:
            tf.extractall(dest, filter="data")  # refuses links and paths outside dest
    entries = [p for p in dest.iterdir()]
    if len(entries) == 1 and entries[0].is_dir():
        return entries[0]
    return dest


def find_one(root: Path, pattern: str) -> Path:
    """Find exactly one file matching `pattern` under `root` (layout-agnostic)."""
    matches = sorted(p for p in root.rglob(pattern) if p.is_file())
    if len(matches) != 1:
        raise BundleError(f"expected exactly one {pattern} under {root}, found {len(matches)}")
    return matches[0]


def library_name(target: str) -> str:
    if "windows" in target:
        return "vector.dll"
    if "apple" in target:
        return "vector.dylib"  # PostgreSQL 16+ uses .dylib on macOS
    return "vector.so"


def package(root: Path, src: Path, target: str, pg_major: str, pg_version: str,
            pgvector_version: str, source_url: str, source_sha256: str, out: Path) -> tuple[Path, Path]:
    """Collect the installed extension into the bundle layout and write the manifest.

    Layout: lib/vector.<so|dylib|dll>, share/extension/vector.control,
    share/extension/vector--*.sql, LICENSE-pgvector, manifest.json. Returns
    (archive, manifest) paths in `out`.
    """
    stage = out / f"stage-pg{pg_major}-{target}"
    if stage.exists():
        shutil.rmtree(stage)
    (stage / "lib").mkdir(parents=True)
    (stage / "share" / "extension").mkdir(parents=True)

    lib = find_one(root / "lib", library_name(target))
    control = find_one(root / "share", "vector.control")
    sqls = sorted(p for p in (root / "share").rglob("vector--*.sql") if p.is_file())
    if not sqls:
        raise BundleError("no vector--*.sql files were installed")
    expected = f"vector--{pgvector_version}.sql"
    if expected not in {p.name for p in sqls}:
        raise BundleError(f"{expected} is missing: the built version does not match the pin")

    shutil.copy2(lib, stage / "lib" / lib.name)
    shutil.copy2(control, stage / "share" / "extension" / control.name)
    for sql in sqls:
        shutil.copy2(sql, stage / "share" / "extension" / sql.name)
    licence = src / "LICENSE"
    if not licence.is_file():
        raise BundleError("pgvector source has no LICENSE file")
    shutil.copy2(licence, stage / "LICENSE-pgvector")

    files = sorted(p for p in stage.rglob("*") if p.is_file())
    manifest = {
        "schema": 1,
        "pgvector_version": pgvector_version,
        "pg_major": int(pg_major),
        "pg_version": pg_version,
        "target": target,
        "source": {"url": source_url, "sha256": source_sha256},
        "files": [
            {"path": p.relative_to(stage).as_posix(), "sha256": sha256_file(p), "size": p.stat().st_size}
            for p in files
        ],
    }
    manifest_text = json.dumps(manifest, indent=2, sort_keys=True) + "\n"
    (stage / "manifest.json").write_text(manifest_text, encoding="utf-8", newline="\n")

    base = f"pgvector-{pgvector_version}-pg{pg_major}-{target}"
    archive = out / f"{base}.tar.gz"
    with tarfile.open(archive, "w:gz") as tf:
        for path in sorted(stage.rglob("*")):
            info = tf.gettarinfo(str(path), arcname=path.relative_to(stage).as_posix())
            # Reproducible archives: no owner or timestamp from the build machine.
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            info.mtime = 0
            if path.is_file():
                with path.open("rb") as handle:
                    tf.addfile(info, handle)
            else:
                tf.addfile(info)
    manifest_out = out / f"{base}.manifest.json"
    manifest_out.write_text(manifest_text, encoding="utf-8", newline="\n")
    shutil.rmtree(stage)
    return archive, manifest_out


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("fetch-pg")
    p.add_argument("--major", required=True)
    p.add_argument("--target", required=True)
    p.add_argument("--dest", required=True, type=Path)

    p = sub.add_parser("fetch-src")
    p.add_argument("--url", required=True)
    p.add_argument("--sha256", required=True)
    p.add_argument("--dest", required=True, type=Path)

    p = sub.add_parser("package")
    p.add_argument("--root", required=True, type=Path)
    p.add_argument("--src", required=True, type=Path)
    p.add_argument("--target", required=True)
    p.add_argument("--pg-major", required=True)
    p.add_argument("--pg-version", required=True)
    p.add_argument("--pgvector-version", required=True)
    p.add_argument("--source-url", required=True)
    p.add_argument("--source-sha256", required=True)
    p.add_argument("--out", required=True, type=Path)

    args = parser.parse_args(argv)
    # Workflows capture stdout with $(...): never emit CRLF on Windows runners.
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(newline="\n")
    try:
        if args.cmd == "fetch-pg":
            asset = pg_asset(args.major, args.target)
            archive = args.dest / asset["asset"]
            download_verified(asset["url"], asset["sha256"], archive)
            root = extract(archive, args.dest / "root")
            archive.unlink()
            print(root)
        elif args.cmd == "fetch-src":
            download_verified(args.url, args.sha256, args.dest)
            print(args.dest)
        else:
            args.out.mkdir(parents=True, exist_ok=True)
            archive, manifest = package(
                args.root, args.src, args.target, args.pg_major, args.pg_version,
                args.pgvector_version, args.source_url, args.source_sha256, args.out,
            )
            print(archive)
            print(manifest)
    except (BundleError, OSError, tarfile.TarError, zipfile.BadZipFile) as err:
        print(f"error: {err}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
