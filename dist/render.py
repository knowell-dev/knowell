#!/usr/bin/env python3
"""Render package-manager manifests from templates, a version and SHA256SUMS.

Usage:
    python dist/render.py --version 1.0.0 --sums SHA256SUMS --out OUTDIR \
        [--repo knowell-dev/knowell] [--only homebrew,scoop,winget] \
        [--release-date YYYY-MM-DD]
    python dist/render.py check --version 1.0.0 --sums SHA256SUMS

Templates use `@@NAME@@` placeholders. Rendering is strict: a placeholder with no
value, or a platform checksum that no template uses, is an error, so a release can
never publish a manifest with a hole in it. Standard library only; no network.
"""

from __future__ import annotations

import argparse
import datetime
import re
import sys
from pathlib import Path

DIST = Path(__file__).resolve().parent
TEMPLATES = DIST / "templates"

# Release platforms: key -> (target triple, archive extension).
PLATFORMS = {
    "LINUX_X64": ("x86_64-unknown-linux-gnu", "tar.gz"),
    "LINUX_ARM64": ("aarch64-unknown-linux-gnu", "tar.gz"),
    "MACOS_X64": ("x86_64-apple-darwin", "tar.gz"),
    "MACOS_ARM64": ("aarch64-apple-darwin", "tar.gz"),
    "WINDOWS_X64": ("x86_64-pc-windows-msvc", "zip"),
    "WINDOWS_ARM64": ("aarch64-pc-windows-msvc", "zip"),
}

# Rendered outputs: template -> destination, and the platforms each one needs.
OUTPUTS = {
    "homebrew": {
        "templates": {"homebrew/knowell.rb.tmpl": "Formula/knowell.rb"},
        "platforms": ["LINUX_X64", "LINUX_ARM64", "MACOS_X64", "MACOS_ARM64"],
    },
    "scoop": {
        "templates": {"scoop/knowell.json.tmpl": "bucket/knowell.json"},
        "platforms": ["WINDOWS_X64", "WINDOWS_ARM64"],
    },
    "winget": {
        "templates": {
            "winget/Knowell.Knowell.yaml.tmpl": "winget/Knowell.Knowell.yaml",
            "winget/Knowell.Knowell.installer.yaml.tmpl": "winget/Knowell.Knowell.installer.yaml",
            "winget/Knowell.Knowell.locale.en-US.yaml.tmpl": "winget/Knowell.Knowell.locale.en-US.yaml",
        },
        "platforms": ["WINDOWS_X64", "WINDOWS_ARM64"],
    },
}

VERSION_RE = re.compile(r"^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$")
REPO_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
SHA_RE = re.compile(r"^[0-9a-f]{64}$")
PLACEHOLDER_RE = re.compile(r"@@([A-Z0-9_]+)@@")


class RenderError(Exception):
    """A release input or template is wrong; the message says what to fix."""


def archive_name(version: str, key: str) -> str:
    target, ext = PLATFORMS[key]
    return f"knowell-{version}-{target}.{ext}"


def archive_dir(version: str, key: str) -> str:
    """Top-level directory inside the archive (the archive name without extension)."""
    target, _ = PLATFORMS[key]
    return f"knowell-{version}-{target}"


def parse_sums(text: str) -> dict[str, str]:
    """Parse `sha256sum` output into {filename: lowercase hex digest}."""
    sums: dict[str, str] = {}
    for number, raw in enumerate(text.splitlines(), start=1):
        line = raw.strip()
        if not line:
            continue
        parts = line.split(None, 1)
        if len(parts) != 2:
            raise RenderError(f"SHA256SUMS line {number} is malformed")
        digest, name = parts[0].lower(), parts[1].lstrip("*").strip()
        if not SHA_RE.match(digest):
            raise RenderError(f"SHA256SUMS line {number} has an invalid digest")
        if name in sums and sums[name] != digest:
            raise RenderError(f"SHA256SUMS lists {name} twice with different digests")
        sums[name] = digest
    return sums


def check_version(version: str) -> None:
    if not VERSION_RE.match(version):
        raise RenderError(f"version {version!r} is not a plain semantic version (no leading v)")


def values_for(version: str, sums: dict[str, str], repo: str, platforms: list[str],
               release_date: str) -> dict[str, str]:
    """Build the placeholder map for the given platforms."""
    check_version(version)
    if not REPO_RE.match(repo):
        raise RenderError(f"repo {repo!r} is not of the form owner/name")
    tag = f"v{version}"
    values = {
        "VERSION": version,
        "TAG": tag,
        "REPO": repo,
        "RELEASE_DATE": release_date,
    }
    base = f"https://github.com/{repo}/releases/download/{tag}"
    for key in platforms:
        name = archive_name(version, key)
        digest = sums.get(name)
        if digest is None:
            raise RenderError(f"SHA256SUMS has no entry for {name}")
        values[f"URL_{key}"] = f"{base}/{name}"
        values[f"SHA_{key}"] = digest
        values[f"SHA_{key}_UPPER"] = digest.upper()
        values[f"DIR_{key}"] = archive_dir(version, key)
    return values


def render_text(template: str, values: dict[str, str], used: set[str]) -> str:
    def substitute(match: re.Match[str]) -> str:
        name = match.group(1)
        if name not in values:
            raise RenderError(f"template uses @@{name}@@ but no value was provided")
        used.add(name)
        return values[name]

    return PLACEHOLDER_RE.sub(substitute, template)


def render_output(name: str, version: str, sums: dict[str, str], repo: str, out: Path,
                  release_date: str) -> list[Path]:
    spec = OUTPUTS[name]
    values = values_for(version, sums, repo, spec["platforms"], release_date)
    used: set[str] = set()
    written: list[Path] = []
    for template_rel, dest_rel in spec["templates"].items():
        text = (TEMPLATES / template_rel).read_text(encoding="utf-8")
        rendered = render_text(text, values, used)
        if "@@" in rendered:
            raise RenderError(f"{template_rel}: unresolved marker left in output")
        dest = out / dest_rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        # Bytes, not text: keep Unix newlines on Windows too (validators expect them).
        dest.write_bytes(rendered.encode("utf-8"))
        written.append(dest)
    # Every platform we demanded a checksum for must actually land in the output.
    for key in spec["platforms"]:
        if f"SHA_{key}" not in used and f"SHA_{key}_UPPER" not in used:
            raise RenderError(f"{name}: templates never use a checksum for {key}")
    return written


def check_assets(version: str, sums: dict[str, str]) -> list[str]:
    """Return the archive names SHA256SUMS lacks (empty means complete)."""
    check_version(version)
    names = [archive_name(version, key) for key in PLATFORMS]
    return [name for name in names if name not in sums]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("command", nargs="?", default="render", choices=["render", "check"])
    parser.add_argument("--version", required=True, help="version without the leading v")
    parser.add_argument("--sums", required=True, type=Path, help="path to SHA256SUMS")
    parser.add_argument("--out", type=Path, help="output directory (render)")
    parser.add_argument("--repo", default="knowell-dev/knowell")
    parser.add_argument("--only", default=",".join(OUTPUTS), help="comma-separated subset of: " + ",".join(OUTPUTS))
    parser.add_argument("--release-date", default=None, help="YYYY-MM-DD (default: today, UTC)")
    args = parser.parse_args(argv)

    try:
        sums = parse_sums(args.sums.read_text(encoding="utf-8"))
        if args.command == "check":
            missing = check_assets(args.version, sums)
            if missing:
                print("missing from SHA256SUMS:", *missing, sep="\n  ", file=sys.stderr)
                return 1
            print("SHA256SUMS lists every platform archive")
            return 0
        if args.out is None:
            raise RenderError("--out is required")
        date = args.release_date or datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d")
        if not re.match(r"^\d{4}-\d{2}-\d{2}$", date):
            raise RenderError("--release-date must look like YYYY-MM-DD")
        for name in [n.strip() for n in args.only.split(",") if n.strip()]:
            if name not in OUTPUTS:
                raise RenderError(f"unknown output {name!r}")
            for path in render_output(name, args.version, sums, args.repo, args.out, date):
                print(f"rendered {path}")
    except (RenderError, OSError) as err:
        print(f"error: {err}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
