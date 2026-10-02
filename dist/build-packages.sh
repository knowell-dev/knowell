#!/usr/bin/env bash
# Build the .deb and .rpm for one Linux target from an already-built `know` binary.
#
#   dist/build-packages.sh <target-triple> <path-to-know> <output-dir>
#
# Run from the repository root with cargo-deb and cargo-generate-rpm installed. Packaging
# metadata lives in dist/packaging/*.toml and is appended to crates/knowell/Cargo.toml of
# this checkout only (CI checkouts are ephemeral; the committed manifest is not changed
# by anyone else). No compilation happens here, so the arm64 package can be assembled on
# an x86-64 host.
set -euo pipefail

if [ $# -ne 3 ]; then
  echo "usage: $0 <target-triple> <path-to-know> <output-dir>" >&2
  exit 2
fi
target=$1
binary=$2
out=$3

case "$target" in
  *-unknown-linux-gnu) ;;
  *) echo "error: .deb/.rpm are built for linux-gnu targets only, not $target" >&2; exit 2 ;;
esac
[ -f "$binary" ] || { echo "error: binary not found: $binary" >&2; exit 2; }

manifest=crates/knowell/Cargo.toml
if ! grep -q '^\[package\.metadata\.deb\]' "$manifest"; then
  printf '\n' >> "$manifest"
  cat dist/packaging/cargo-deb.toml dist/packaging/cargo-generate-rpm.toml >> "$manifest"
fi

install -D -m 0755 "$binary" "target/$target/release/know"
mkdir -p "$out"
# --no-strip: the release binary is already stripped, and a foreign-arch binary cannot be
# stripped by the host's tools anyway.
cargo deb --no-build --no-strip --target "$target" -p knowell -o "$out/"
cargo generate-rpm -p crates/knowell --target "$target" -o "$out/"
