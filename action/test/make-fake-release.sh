#!/usr/bin/env bash
# Build a fake Linux x86_64 release tree for the composite-action job in CI:
#   make-fake-release.sh DIR VERSION
# Result: DIR/v<VERSION>/{knowell-<VERSION>-x86_64-unknown-linux-gnu.tar.gz,SHA256SUMS}
# The archive holds the fake `know` from test/bin.
set -euo pipefail
dir="${1:?usage: make-fake-release.sh DIR VERSION}"
version="${2:?usage: make-fake-release.sh DIR VERSION}"
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
name="knowell-$version-x86_64-unknown-linux-gnu"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name" "$dir/v$version"
cp "$here/bin/know" "$stage/$name/know"
tar -czf "$dir/v$version/$name.tar.gz" -C "$stage" "$name"
(cd "$dir/v$version" && sha256sum "$name.tar.gz" >SHA256SUMS)
