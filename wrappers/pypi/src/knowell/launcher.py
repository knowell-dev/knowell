"""Download, verify, cache and run the `know` binary. Standard library only.

Writes nothing to stdout itself: stdout carries the protocol when `know` runs as an MCP
stdio server (`uvx knowell mcp`).
"""

from __future__ import annotations

import hashlib
import os
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
import zipfile
from importlib import metadata
from pathlib import Path
from urllib.parse import urlparse

DEFAULT_BASE = "https://github.com/knowell-dev/knowell/releases/download"
VERSION_RE = re.compile(r"^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$")
DIGEST_RE = re.compile(r"^[0-9a-f]{64}$")
LOOPBACK = {"127.0.0.1", "localhost", "::1"}


class LauncherError(Exception):
    """A user-facing failure; the message says what to do."""


def resolve_target(system: str, machine: str, glibc: bool = True) -> tuple[str, str, str]:
    """Return (target triple, archive extension, executable name)."""
    cpu = {"x86_64": "x86_64", "amd64": "x86_64", "arm64": "aarch64", "aarch64": "aarch64"}.get(machine.lower())
    if cpu is None:
        raise LauncherError(f"unsupported CPU architecture: {machine}")
    if system == "Linux":
        return f"{cpu}-unknown-linux-{'gnu' if glibc else 'musl'}", "tar.gz", "know"
    if system == "Darwin":
        return f"{cpu}-apple-darwin", "tar.gz", "know"
    if system == "Windows":
        return f"{cpu}-pc-windows-msvc", "zip", "know.exe"
    raise LauncherError(f"unsupported operating system: {system}")


def parse_sums(text: str) -> dict[str, str]:
    sums: dict[str, str] = {}
    for number, raw in enumerate(text.splitlines(), start=1):
        line = raw.strip()
        if not line:
            continue
        parts = line.split(None, 1)
        if len(parts) != 2:
            raise LauncherError(f"SHA256SUMS line {number} is malformed")
        digest, name = parts[0].lower(), parts[1].lstrip("*").strip()
        if not DIGEST_RE.match(digest):
            raise LauncherError(f"SHA256SUMS line {number} has an invalid digest")
        if sums.setdefault(name, digest) != digest:
            raise LauncherError(f"SHA256SUMS lists {name} twice with different digests")
    return sums


def verify_file(path: Path, name: str, sums: dict[str, str]) -> None:
    expected = sums.get(name)
    if expected is None:
        raise LauncherError(f"SHA256SUMS has no entry for {name}")
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    if digest.hexdigest() != expected:
        raise LauncherError(f"checksum mismatch for {name}: expected {expected}, got {digest.hexdigest()}")


def cache_root(env: dict[str, str] | None = None, system: str | None = None, home: Path | None = None) -> Path:
    env = os.environ if env is None else env
    system = system or platform.system()
    home = home or Path.home()
    if env.get("KNOWELL_CACHE_DIR"):
        return Path(env["KNOWELL_CACHE_DIR"])
    if system == "Windows":
        return Path(env.get("LOCALAPPDATA") or home / "AppData" / "Local") / "knowell" / "pypi"
    if system == "Darwin":
        return home / "Library" / "Caches" / "knowell"
    return Path(env.get("XDG_CACHE_HOME") or home / ".cache") / "knowell"


def download(href: str, dest: Path) -> None:
    url = urlparse(href)
    if url.scheme != "https" and not (url.scheme == "http" and url.hostname in LOOPBACK):
        raise LauncherError(f"refusing to download over {url.scheme or 'an unknown scheme'}")
    request = urllib.request.Request(href, headers={"User-Agent": "knowell-pypi-launcher"})
    try:
        with urllib.request.urlopen(request, timeout=60) as response, dest.open("wb") as out:
            shutil.copyfileobj(response, out)
    except OSError as err:
        raise LauncherError(f"download failed for {href}: {err}") from err


def ensure_binary(version: str, env: dict[str, str] | None = None, system: str | None = None,
                  machine: str | None = None, glibc: bool = True) -> Path:
    """Return the cached `know` for `version`, downloading and verifying it if needed."""
    env = dict(os.environ) if env is None else env
    if not VERSION_RE.match(version):
        raise LauncherError(f"invalid version: {version}")
    system = system or platform.system()
    target, ext, exe_name = resolve_target(system, machine or platform.machine(), glibc)
    final_dir = cache_root(env, system) / version / target
    final_exe = final_dir / exe_name
    if final_exe.is_file():
        return final_exe

    base = f"{env.get('KNOWELL_DOWNLOAD_BASE', DEFAULT_BASE).rstrip('/')}/v{version}"
    name = f"knowell-{version}-{target}"
    final_dir.parent.mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix=f".{target}-", dir=final_dir.parent))
    try:
        print(f"knowell: downloading {name}.{ext} (first run only) ...", file=sys.stderr)
        archive = work / f"{name}.{ext}"
        download(f"{base}/{archive.name}", archive)
        download(f"{base}/SHA256SUMS", work / "SHA256SUMS")
        verify_file(archive, archive.name, parse_sums((work / "SHA256SUMS").read_text(encoding="utf-8")))

        unpacked = work / "x"
        unpacked.mkdir()
        if ext == "zip":
            with zipfile.ZipFile(archive) as zf:
                zf.extractall(unpacked)
        else:
            with tarfile.open(archive) as tf:
                tf.extractall(unpacked, filter="data")
        exe = unpacked / name / exe_name
        if not exe.is_file():
            raise LauncherError(f"the archive does not contain {name}/{exe_name}")
        if system != "Windows":
            exe.chmod(0o755)
        staged = work / "final"
        staged.mkdir()
        exe.rename(staged / exe_name)
        try:
            staged.rename(final_dir)
        except OSError:
            if not final_exe.is_file():  # a concurrent run may have won the race
                raise
    finally:
        shutil.rmtree(work, ignore_errors=True)
    return final_exe


def main() -> None:
    binary = os.environ.get("KNOWELL_BIN")
    try:
        if not binary:
            try:
                default_version = metadata.version("knowell")
            except metadata.PackageNotFoundError:
                default_version = "0.0.0"
            version = os.environ.get("KNOWELL_BINARY_VERSION", default_version)
            if version == "0.0.0":
                raise LauncherError(
                    "this is an unreleased development copy of the launcher; "
                    "set KNOWELL_BIN or KNOWELL_BINARY_VERSION"
                )
            binary = str(ensure_binary(version))
        raise SystemExit(subprocess.call([binary, *sys.argv[1:]]))
    except LauncherError as err:
        print(f"knowell: {err}", file=sys.stderr)
        raise SystemExit(1) from err
    except KeyboardInterrupt:
        raise SystemExit(130) from None
