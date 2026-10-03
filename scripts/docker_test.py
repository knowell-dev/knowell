#!/usr/bin/env python3
"""Runs the Linux test suite in Docker (deploy/test/compose.yml).

Every run copies the files git would see (tracked plus untracked, not ignored; so
uncommitted edits are included, while target/, node_modules and .env files are not)
into a Docker volume, then runs the command against throwaway PostgreSQL servers with
KNOWELL_TEST_STRICT=1. The host directory is never mounted into a container, so no
Docker file-sharing setting is needed and nothing in the working tree is written.

Run it through the machine-wide build lock, from anywhere in the repository:

    python scripts/buildlock.py python scripts/docker_test.py
    python scripts/buildlock.py python scripts/docker_test.py cargo test -p knowell-store --locked
    python scripts/buildlock.py python scripts/docker_test.py --panel
    python scripts/docker_test.py --down        # stop the databases, keep build caches

Without a command the Rust service runs `cargo test --workspace --locked`, and the panel
service runs check, lint, unit tests and build.
"""

from __future__ import annotations

import os
import subprocess
import sys
import tarfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
COMPOSE = ["docker", "compose", "-f", os.path.join(ROOT, "deploy", "test", "compose.yml")]


def _git(*args: str) -> bytes:
    return subprocess.run(["git", "-C", ROOT, *args], check=True, capture_output=True).stdout


def _source_files() -> list[tuple[str, int]]:
    """Relative paths git sees, with the mode to give them inside the container.

    Tracked files keep git's executable bit (Windows checkouts lose it on disk); untracked
    files are plain. Paths deleted in the working tree are skipped.
    """
    modes = {}
    for entry in _git("ls-files", "-s", "-z").split(b"\0"):
        if not entry:
            continue
        meta, path = entry.split(b"\t", 1)
        modes[path.decode("utf-8")] = 0o755 if meta.startswith(b"100755") else 0o644
    files = []
    for raw in _git("ls-files", "-z", "--cached", "--others", "--exclude-standard").split(b"\0"):
        if not raw:
            continue
        path = raw.decode("utf-8")
        if os.path.isfile(os.path.join(ROOT, path)):
            files.append((path, modes.get(path, 0o644)))
    return sorted(set(files))


def _sync() -> None:
    files = _source_files()
    proc = subprocess.Popen([*COMPOSE, "run", "--rm", "-T", "--no-deps", "sync"], stdin=subprocess.PIPE)
    if proc.stdin is None:
        raise SystemExit("docker_test: could not open the sync container's input")
    try:
        with tarfile.open(fileobj=proc.stdin, mode="w|", format=tarfile.PAX_FORMAT) as tar:
            for path, mode in files:
                info = tar.gettarinfo(os.path.join(ROOT, path), arcname=path)
                info.mode = mode
                info.uid = info.gid = 0
                info.uname = info.gname = ""
                with open(os.path.join(ROOT, path), "rb") as handle:
                    tar.addfile(info, handle)
    finally:
        proc.stdin.close()
    if proc.wait() != 0:
        raise SystemExit("docker_test: copying the source into the test volume failed")
    print(f"[docker_test] synced {len(files)} files", file=sys.stderr, flush=True)


def main() -> int:
    args = sys.argv[1:]
    if args == ["--down"]:
        return subprocess.call([*COMPOSE, "down"])
    service = "rust"
    if args[:1] == ["--panel"]:
        service, args = "panel", args[1:]
    _sync()
    return subprocess.call([*COMPOSE, "run", "--rm", service, *args])


if __name__ == "__main__":
    sys.exit(main())
