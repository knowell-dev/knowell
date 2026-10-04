#!/usr/bin/env python3
"""Generate bounded raw updater assets and unsigned TUF target input, never signing keys.

Raw engine/launcher assets use digest-prefixed names for TUF consistent snapshots.
Logical target names are exactly v<version>/knowell-<version>-<target>-<component>[.exe].
Compatibility is reviewed source data, never inferred from the product version.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import stat
import sys
from pathlib import Path

import render

MAX_BINARY_BYTES = 512 * 1024 * 1024
MAX_JSON_BYTES = 1024 * 1024
VERSION_RE = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?")
TARGETS = tuple(value[0] for value in render.PLATFORMS.values())
OPTIONAL_TARGETS = ("x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl")
RANGES = ("config", "index", "jobs", "protocol", "launcher")


class UpdateAssetError(Exception):
    """Rejected release input; messages do not echo untrusted content."""


def check_version(version: str, allow_unreleased: bool = False) -> str:
    """Return the explicit channel for strict SemVer without build metadata."""
    if not isinstance(version, str) or len(version) > 128:
        raise UpdateAssetError("version must be a bounded semantic version")
    match = VERSION_RE.fullmatch(version)
    if match is None:
        raise UpdateAssetError("version must be a semantic version without build metadata")
    if any(int(match.group(index)) > 0xFFFFFFFFFFFFFFFF for index in (1, 2, 3)):
        raise UpdateAssetError("semantic version components exceed the 64-bit version contract")
    prerelease = match.group(4)
    if prerelease and any(part.isdigit() and len(part) > 1 and part.startswith("0") for part in prerelease.split(".")):
        raise UpdateAssetError("numeric prerelease identifiers must not have leading zeroes")
    if int(match.group(1)) == 0 and not allow_unreleased:
        raise UpdateAssetError("public releases start at stable 1.0.0")
    if version.startswith("1.0.0-") and not allow_unreleased:
        raise UpdateAssetError("no prerelease precedes the first stable 1.0.0 release")
    return "preview" if prerelease else "stable"


def read_json(path: Path) -> object:
    """Read a bounded regular JSON file and reject duplicate object keys."""
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_size > MAX_JSON_BYTES:
        raise UpdateAssetError("manifest must be a bounded regular file")

    def object_pairs(pairs: list[tuple[str, object]]) -> dict:
        result = {}
        for key, value in pairs:
            if key in result:
                raise UpdateAssetError("manifest contains duplicate object keys")
            result[key] = value
        return result

    try:
        return json.loads(path.read_bytes(), object_pairs_hook=object_pairs)
    except (ValueError, UnicodeError) as error:
        raise UpdateAssetError("manifest is not valid UTF-8 JSON") from error


def validate_contract(value: object) -> dict:
    """Reject missing, unknown, reversed and noninteger compatibility ranges."""
    if not isinstance(value, dict) or set(value) != {"schema", *RANGES}:
        raise UpdateAssetError("compatibility must declare schema, config, index, jobs, protocol and launcher")
    for name in ("schema", *RANGES):
        limits = value[name]
        keys = {"read_min", "read_max", "write_min", "write_max"} if name == "schema" else {"min", "max"}
        if not isinstance(limits, dict) or set(limits) != keys:
            raise UpdateAssetError("compatibility range has missing or unknown fields")
        if any(type(number) is not int or not 1 <= number <= 0xFFFFFFFF for number in limits.values()):
            raise UpdateAssetError("compatibility versions must be positive 32-bit integers")
        pairs = (("read_min", "read_max"), ("write_min", "write_max")) if name == "schema" else (("min", "max"),)
        if any(limits[lower] > limits[upper] for lower, upper in pairs):
            raise UpdateAssetError("compatibility range is reversed")
    return value


def compatibility(path: Path, migrations: Path) -> dict:
    """Validate exact format ranges against the reviewed SQL migration sequence."""
    value = validate_contract(read_json(path))
    versions = []
    for path in migrations.iterdir():
        match = re.fullmatch(r"([0-9]+)_[A-Za-z0-9_]+\.sql", path.name)
        if match is None or not path.is_file() or path.is_symlink():
            raise UpdateAssetError("migration directory has an unexpected entry")
        versions.append(int(match.group(1)))
    if not versions or sorted(versions) != list(range(1, len(versions) + 1)):
        raise UpdateAssetError("SQL migrations must be a complete consecutive sequence")
    schema = value["schema"]
    current = max(versions)
    if schema["write_min"] != current or schema["write_max"] != current or not schema["read_min"] <= current <= schema["read_max"]:
        raise UpdateAssetError("reviewed schema compatibility does not match the current migration")
    return value


def target_name(version: str, target: str, component: str) -> str:
    """Construct the only permitted logical binary target path."""
    if target not in (*TARGETS, *OPTIONAL_TARGETS) or component not in ("engine", "launcher"):
        raise UpdateAssetError("unsupported target or component")
    suffix = ".exe" if "windows" in target else ""
    return f"v{version}/knowell-{version}-{target}-{component}{suffix}"


def artifact(version: str, target: str, component: str, source: Path, out: Path) -> dict:
    """Copy a nonempty regular binary to its immutable digest-prefixed asset name."""
    info = source.lstat()
    if not stat.S_ISREG(info.st_mode) or not 0 < info.st_size <= MAX_BINARY_BYTES:
        raise UpdateAssetError("binary must be a nonempty bounded regular file")
    digest = hashlib.sha256()
    with source.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    name = target_name(version, target, component)
    sha256 = digest.hexdigest()
    destination = out / f"{sha256}.{Path(name).name}"
    if destination.exists():
        raise UpdateAssetError("raw output already exists; use a fresh staging directory")
    shutil.copyfile(source, destination)
    destination.chmod(0o755)
    if source.stat().st_size != info.st_size or destination.stat().st_size != info.st_size:
        raise UpdateAssetError("binary changed while packaging")
    copied_digest = hashlib.sha256()
    with destination.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            copied_digest.update(chunk)
    if copied_digest.hexdigest() != sha256:
        raise UpdateAssetError("binary changed while packaging")
    return {"name": name, "sha256": sha256, "size": info.st_size}


def write_json(path: Path, value: object) -> None:
    """Write deterministic UTF-8 JSON into a new staging file."""
    with path.open("xb") as stream:
        stream.write((json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8"))


def validate_binary_info(path: Path, version: str, target: str, reviewed: dict) -> None:
    """Require the engine's stateless handshake to agree with its reviewed release contract."""
    value = read_json(path)
    fields = {"format_version", "version", "target", "schema", *RANGES}
    if not isinstance(value, dict) or set(value) != fields or type(value["format_version"]) is not int or value["format_version"] != 1 or value["version"] != version or value["target"] != target:
        raise UpdateAssetError("engine handshake differs from its declared release identity")
    for name in ("schema", *RANGES):
        number = value[name]
        if type(number) is not int or not 1 <= number <= 0xFFFFFFFF:
            raise UpdateAssetError("engine handshake has an invalid format version")
        lower, upper = ("write_min", "write_max") if name == "schema" else ("min", "max")
        if not reviewed[name][lower] <= number <= reviewed[name][upper]:
            raise UpdateAssetError("engine handshake differs from reviewed compatibility")


def build(version: str, target: str, binary: Path, launcher: Path, out: Path,
          compatibility_file: Path, migrations: Path, allow_unreleased: bool = False,
          binary_info: Path | None = None) -> Path:
    """Package one platform; the manifest alone does not authorize installation."""
    channel = check_version(version, allow_unreleased)
    reviewed = compatibility(compatibility_file, migrations)
    if binary_info is not None:
        validate_binary_info(binary_info, version, target, reviewed)
    out.mkdir(parents=True, exist_ok=True)
    engine = artifact(version, target, "engine", binary, out)
    bootstrap = artifact(version, target, "launcher", launcher, out)
    manifest = {"format_version": 1, "version": version, "target": target,
                "channel": channel, "compatibility": reviewed, "engine": engine,
                "launcher": bootstrap}
    path = out / f"knowell-{version}-{target}.update.json"
    write_json(path, manifest)
    return path


def assemble(version: str, assets: Path, out: Path, allow_unreleased: bool = False,
             compatibility_file: Path | None = None, migrations: Path | None = None,
             previous_input: Path | None = None) -> Path:
    """Verify all six required platforms and emit unsigned input for a TUF signer."""
    channel = check_version(version, allow_unreleased)
    repo = Path(__file__).resolve().parent.parent
    reviewed = compatibility(compatibility_file or repo / "dist/update-compatibility.json",
                             migrations or repo / "crates/knowell-store/migrations")
    targets = {}
    if previous_input is not None:
        previous = read_json(previous_input)
        if not isinstance(previous, dict) or set(previous) != {"format_version", "version", "targets"} or type(previous["format_version"]) is not int or previous["format_version"] != 1 or not isinstance(previous["targets"], dict):
            raise UpdateAssetError("previous unsigned input has an unsupported format")
        check_version(previous["version"], allow_unreleased)
        # This is an owner-reviewed signing input, never a substitute for loading
        # the previously trusted repository. The signer validates every entry.
        targets.update(previous["targets"])
    found = set()
    manifests = sorted(assets.glob(f"knowell-{version}-*.update.json"))
    for path in manifests:
        manifest = read_json(path)
        if not isinstance(manifest, dict) or set(manifest) != {"format_version", "version", "target", "channel", "compatibility", "engine", "launcher"}:
            raise UpdateAssetError("release manifest has missing or unknown fields")
        target = manifest["target"]
        if not isinstance(target, str) or target not in (*TARGETS, *OPTIONAL_TARGETS):
            raise UpdateAssetError("release manifest names an unsupported platform")
        if target in found or type(manifest["format_version"]) is not int or manifest["format_version"] != 1 or manifest["version"] != version or manifest["channel"] != channel:
            raise UpdateAssetError("release manifests disagree or duplicate a platform")
        if validate_contract(manifest["compatibility"]) != reviewed:
            raise UpdateAssetError("release compatibility does not match the reviewed source contract")
        found.add(target)
        for component in ("engine", "launcher"):
            item = manifest[component]
            expected = target_name(version, target, component)
            if not isinstance(item, dict) or set(item) != {"name", "sha256", "size"} or item["name"] != expected:
                raise UpdateAssetError("artifact identity is not canonical")
            digest = item["sha256"]
            size = item["size"]
            if not isinstance(digest, str) or re.fullmatch(r"[0-9a-f]{64}", digest) is None or type(size) is not int or not 0 < size <= MAX_BINARY_BYTES:
                raise UpdateAssetError("artifact digest or size is invalid")
            raw = assets / f"{digest}.{Path(expected).name}"
            info = raw.lstat()
            if not stat.S_ISREG(info.st_mode) or info.st_size != size:
                raise UpdateAssetError("raw artifact is missing or has the wrong size")
            actual = hashlib.sha256()
            with raw.open("rb") as stream:
                for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                    actual.update(chunk)
            if actual.hexdigest() != digest:
                raise UpdateAssetError("raw artifact digest does not match its manifest")
            if expected in targets:
                raise UpdateAssetError("historical target identities must not be overwritten")
            custom = {}
            if component == "engine":
                custom = {"knowell": {"format_version": 1, "version": version, "target": target,
                          "channel": channel, "component": component, "compatibility": manifest["compatibility"],
                          "launcher": manifest["launcher"], "revoked": False}}
            targets[expected] = {"length": size, "hashes": {"sha256": digest}, "custom": custom}
    if not set(TARGETS).issubset(found):
        raise UpdateAssetError("release lacks a required engine and launcher platform")
    out.mkdir(parents=True, exist_ok=True)
    path = out / "unsigned-update-targets.json"
    write_json(path, {"format_version": 1, "version": version, "targets": targets})
    return path


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    package = commands.add_parser("build")
    package.add_argument("--version", required=True)
    package.add_argument("--target", required=True)
    package.add_argument("--binary", required=True, type=Path)
    package.add_argument("--launcher", required=True, type=Path)
    package.add_argument("--binary-info", type=Path, help="stateless engine update --inspect-binary JSON handshake")
    package.add_argument("--compatibility", type=Path, default=Path("dist/update-compatibility.json"))
    package.add_argument("--migrations", type=Path, default=Path("crates/knowell-store/migrations"))
    package.add_argument("--out", required=True, type=Path)
    package.add_argument("--allow-unreleased", action="store_true", help="publication-free development dry runs only")
    gather = commands.add_parser("assemble")
    gather.add_argument("--version", required=True)
    gather.add_argument("--assets", required=True, type=Path)
    gather.add_argument("--out", required=True, type=Path)
    gather.add_argument("--allow-unreleased", action="store_true")
    gather.add_argument("--compatibility", type=Path, default=Path("dist/update-compatibility.json"))
    gather.add_argument("--migrations", type=Path, default=Path("crates/knowell-store/migrations"))
    gather.add_argument("--previous-input", type=Path, help="owner-reviewed cumulative unsigned signing input; never a trust root")
    validate = commands.add_parser("check-version")
    validate.add_argument("--version", required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == "build":
            print(build(args.version, args.target, args.binary, args.launcher, args.out,
                        args.compatibility, args.migrations, args.allow_unreleased, args.binary_info))
        elif args.command == "assemble":
            print(assemble(args.version, args.assets, args.out, args.allow_unreleased,
                           args.compatibility, args.migrations, args.previous_input))
        else:
            print(check_version(args.version))
    except (UpdateAssetError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
