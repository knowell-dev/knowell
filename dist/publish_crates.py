#!/usr/bin/env python3
"""Publish the workspace crates to crates.io in dependency order.

New crates are rate limited by crates.io (a small burst, then roughly one every ten
minutes), so this publishes one crate at a time, skips versions that are already
published (making a re-run after a failure safe), and waits out rate-limit answers.

Usage: python dist/publish_crates.py [--print-order] [--dry-run]
Authentication comes from CARGO_REGISTRY_TOKEN in the environment (never an argument).
Run from the repository root.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
import urllib.error
import urllib.request

RATE_LIMIT_MARKERS = ("429", "too many requests", "rate limit")
RETRY_WAIT_SECONDS = 600
MAX_ATTEMPTS = 12


def publish_order(metadata: dict) -> list[tuple[str, str]]:
    """Topologically sort publishable workspace members: dependencies first.

    Returns [(name, version)]. Ties are broken by name so the order is deterministic.
    A member with `publish = false` (an empty `publish` list) is left out.
    """
    members = {
        pkg["name"]: pkg
        for pkg in metadata["packages"]
        if pkg["id"] in set(metadata["workspace_members"]) and pkg.get("publish") != []
    }
    deps = {
        name: {d["name"] for d in pkg["dependencies"] if d["name"] in members and d.get("kind") != "dev"}
        for name, pkg in members.items()
    }
    order: list[tuple[str, str]] = []
    done: set[str] = set()
    while len(done) < len(members):
        ready = sorted(n for n in members if n not in done and deps[n] <= done)
        if not ready:
            cycle = sorted(set(members) - done)
            raise SystemExit(f"dependency cycle among: {', '.join(cycle)}")
        for name in ready:
            order.append((name, members[name]["version"]))
            done.add(name)
    return order


def already_published(name: str, version: str) -> bool:
    request = urllib.request.Request(
        f"https://crates.io/api/v1/crates/{name}/{version}",
        headers={"User-Agent": "knowell-release (https://github.com/knowell-dev/knowell)"},
    )
    try:
        with urllib.request.urlopen(request, timeout=30):
            return True
    except urllib.error.HTTPError as err:
        if err.code == 404:
            return False
        raise


def is_rate_limited(output: str) -> bool:
    lowered = output.lower()
    return any(marker in lowered for marker in RATE_LIMIT_MARKERS)


def publish_one(name: str, extra: list[str]) -> None:
    for attempt in range(1, MAX_ATTEMPTS + 1):
        result = subprocess.run(
            ["cargo", "publish", "--locked", "-p", name, *extra],
            capture_output=True,
            text=True,
            check=False,
        )
        sys.stdout.write(result.stdout)
        sys.stderr.write(result.stderr)
        if result.returncode == 0:
            return
        if not is_rate_limited(result.stdout + result.stderr):
            raise SystemExit(f"cargo publish failed for {name}")
        print(f"rate limited publishing {name} (attempt {attempt}); waiting {RETRY_WAIT_SECONDS}s", flush=True)
        time.sleep(RETRY_WAIT_SECONDS)
    raise SystemExit(f"gave up on {name} after {MAX_ATTEMPTS} rate-limited attempts")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--print-order", action="store_true", help="print the publish order and exit")
    parser.add_argument("--dry-run", action="store_true", help="pass --dry-run to cargo publish")
    args = parser.parse_args()

    raw = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps", "--locked"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    order = publish_order(json.loads(raw))
    if args.print_order:
        for name, version in order:
            print(f"{name} {version}")
        return 0
    if not order:
        raise SystemExit("no publishable crates (is `publish = false` still set?)")
    for name, version in order:
        if not args.dry_run and already_published(name, version):
            print(f"{name} {version} is already on crates.io; skipping")
            continue
        print(f"publishing {name} {version}", flush=True)
        publish_one(name, ["--dry-run"] if args.dry_run else [])
    return 0


if __name__ == "__main__":
    sys.exit(main())
