#!/usr/bin/env python3
"""Machine-wide build lock: runs one build command at a time.

Several agents and terminals may share one developer machine; parallel Rust
builds can exhaust its memory. Every cargo / npm build goes through this
wrapper:

    python scripts/buildlock.py cargo test -p knowell-core

The lock is a file in the user's home directory created with O_EXCL. The
holder refreshes its mtime every few seconds; a lock that has not been
refreshed for STALE_SECONDS is considered abandoned and is taken over.
CARGO_BUILD_JOBS is forced to 4 for the child process.

Set KNOWELL_BUILDLOCK to share one lock file with other projects.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import threading
import time

LOCK_PATH = os.environ.get("KNOWELL_BUILDLOCK") or os.path.join(
    os.path.expanduser("~"), ".knowell-build.lock"
)
HEARTBEAT_SECONDS = 5
STALE_SECONDS = 60
POLL_SECONDS = 2
JOBS = "4"


def _try_acquire(info: dict) -> bool:
    try:
        fd = os.open(LOCK_PATH, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
    except FileExistsError:
        return False
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        json.dump(info, f)
    return True


def _holder() -> str:
    try:
        with open(LOCK_PATH, encoding="utf-8") as f:
            info = json.load(f)
        return f"pid {info.get('pid')} running `{info.get('cmd')}` in {info.get('cwd')}"
    except (OSError, ValueError):
        return "unknown holder"


def _break_if_stale() -> None:
    try:
        age = time.time() - os.path.getmtime(LOCK_PATH)
    except OSError:
        return
    if age > STALE_SECONDS:
        try:
            os.remove(LOCK_PATH)
            print(f"[buildlock] removed stale lock ({age:.0f}s without heartbeat)", file=sys.stderr)
        except OSError:
            pass


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2
    cmd = sys.argv[1:]
    info = {"pid": os.getpid(), "cmd": " ".join(cmd), "cwd": os.getcwd(), "started": time.time()}

    waited = 0.0
    announced = False
    while not _try_acquire(info):
        if not announced:
            print(f"[buildlock] waiting: {_holder()}", file=sys.stderr, flush=True)
            announced = True
        _break_if_stale()
        time.sleep(POLL_SECONDS)
        waited += POLL_SECONDS
    if announced:
        print(f"[buildlock] acquired after {waited:.0f}s", file=sys.stderr, flush=True)

    stop = threading.Event()

    def heartbeat() -> None:
        while not stop.wait(HEARTBEAT_SECONDS):
            try:
                os.utime(LOCK_PATH, None)
            except OSError:
                pass

    beat = threading.Thread(target=heartbeat, daemon=True)
    beat.start()
    env = dict(os.environ, CARGO_BUILD_JOBS=JOBS)
    exe = shutil.which(cmd[0]) or cmd[0]
    # .cmd/.bat launchers (npm, npx) need cmd.exe on Windows; real executables do not.
    use_shell = os.name == "nt" and exe.lower().endswith((".cmd", ".bat"))
    try:
        return subprocess.call([exe, *cmd[1:]], env=env, shell=use_shell)
    except KeyboardInterrupt:
        return 130
    finally:
        stop.set()
        try:
            os.remove(LOCK_PATH)
        except OSError:
            pass


if __name__ == "__main__":
    sys.exit(main())
