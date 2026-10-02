#!/usr/bin/env python3
"""Build a release archive: knowell-<version>-<target>.<tar.gz|zip>.

    python dist/package_archive.py --version 1.0.0 --target x86_64-unknown-linux-gnu \
        --binary target/x86_64-unknown-linux-gnu/release/know --out release/

The archive holds one top-level directory (the archive name without extension) with the
binary, both licence files and the README. Output is reproducible: fixed timestamps,
sorted entries, no owner information. Run from the repository root.
"""

from __future__ import annotations

import argparse
import gzip
import io
import re
import sys
import tarfile
import zipfile
from pathlib import Path

VERSION_RE = re.compile(r"^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$")
TARGET_RE = re.compile(r"^[a-z0-9_]+-[a-z0-9_]+-[a-z0-9_]+(-[a-z0-9_]+)?$")
EXTRA_FILES = ("LICENSE-MIT", "LICENSE-APACHE", "README.md")
FIXED_TIME = (1980, 1, 1, 0, 0, 0)  # earliest time a zip can store


class PackageError(Exception):
    """Bad input; the message says what to fix."""


def build(version: str, target: str, binary: Path, out: Path, root: Path = Path(".")) -> Path:
    if not VERSION_RE.match(version):
        raise PackageError(f"invalid version: {version}")
    if not TARGET_RE.match(target):
        raise PackageError(f"invalid target: {target}")
    if not binary.is_file():
        raise PackageError(f"binary not found: {binary}")
    windows = "windows" in target
    exe_name = "know.exe" if windows else "know"
    top = f"knowell-{version}-{target}"
    entries: list[tuple[str, Path, int]] = [(f"{top}/{exe_name}", binary, 0o755)]
    for name in EXTRA_FILES:
        path = root / name
        if not path.is_file():
            raise PackageError(f"missing {name} in {root.resolve()}")
        entries.append((f"{top}/{name}", path, 0o644))
    entries.sort(key=lambda e: e[0])

    out.mkdir(parents=True, exist_ok=True)
    if windows:
        archive = out / f"{top}.zip"
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as zf:
            for arcname, path, mode in entries:
                info = zipfile.ZipInfo(arcname, FIXED_TIME)
                info.external_attr = (0o100000 | mode) << 16
                info.compress_type = zipfile.ZIP_DEFLATED
                zf.writestr(info, path.read_bytes())
    else:
        archive = out / f"{top}.tar.gz"
        with archive.open("wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as gz:
            with tarfile.open(fileobj=gz, mode="w", format=tarfile.PAX_FORMAT) as tf:
                for arcname, path, mode in entries:
                    info = tarfile.TarInfo(arcname)
                    data = path.read_bytes()
                    info.size = len(data)
                    info.mode = mode
                    info.mtime = 0
                    info.uid = info.gid = 0
                    info.uname = info.gname = ""
                    tf.addfile(info, io.BytesIO(data))
    return archive


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--version", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--root", type=Path, default=Path("."), help="repository root holding LICENSE files and README.md")
    args = parser.parse_args(argv)
    try:
        print(build(args.version, args.target, args.binary, args.out, args.root))
    except (PackageError, OSError) as err:
        print(f"error: {err}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
