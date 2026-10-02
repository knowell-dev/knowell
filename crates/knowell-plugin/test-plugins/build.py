#!/usr/bin/env python3
"""Rebuilds the test plugin components in `prebuilt/`.

    python crates/knowell-plugin/test-plugins/build.py

Builds the standalone test-plugin workspace for `wasm32-wasip2` through the
repository's build lock (into the shared `target/` directory, so cargo's own
lock is shared too) and copies each component into `prebuilt/`, printing its
size and SHA-256. Requires `rustup target add wasm32-wasip2`.
"""

from __future__ import annotations

import hashlib
import os
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
TARGET_DIR = os.path.join(REPO, "target")
PLUGINS = ["toy_endpoints", "hostile"]
MAX_COMMITTED_BYTES = 200 * 1024


def main() -> int:
    cmd = [
        sys.executable,
        os.path.join(REPO, "scripts", "buildlock.py"),
        "cargo",
        "build",
        "--release",
        "--locked",
        "--target",
        "wasm32-wasip2",
        "--manifest-path",
        os.path.join(HERE, "Cargo.toml"),
        "--target-dir",
        TARGET_DIR,
    ]
    status = subprocess.call(cmd)
    if status != 0:
        return status
    out_dir = os.path.join(HERE, "prebuilt")
    os.makedirs(out_dir, exist_ok=True)
    for name in PLUGINS:
        src = os.path.join(TARGET_DIR, "wasm32-wasip2", "release", f"{name}.wasm")
        dst = os.path.join(out_dir, f"{name.replace('_', '-')}.wasm")
        shutil.copyfile(src, dst)
        with open(dst, "rb") as f:
            data = f.read()
        digest = hashlib.sha256(data).hexdigest()
        note = "" if len(data) <= MAX_COMMITTED_BYTES else "  (larger than 200 KiB!)"
        print(f"{os.path.basename(dst)}: {len(data)} bytes, sha256 {digest}{note}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
