#!/usr/bin/env bash
# Map the runner OS/arch to the release asset (names follow dist/package_archive.py).
#
# Input env:  KNOWELL_VERSION (X.Y.Z), RUNNER_OS (Linux|macOS|Windows), RUNNER_ARCH (X64|ARM64)
# Outputs:    target, name, archive, exe
#
# Linux uses the glibc build; the musl builds are experimental and not used here.
set -euo pipefail
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

version="${KNOWELL_VERSION:?KNOWELL_VERSION is required}"
is_semver "$version" || die "invalid version '$version'"

case "${RUNNER_ARCH:-}" in
  X64) cpu=x86_64 ;;
  ARM64) cpu=aarch64 ;;
  *) die "unsupported runner architecture '${RUNNER_ARCH:-}' (X64 and ARM64 are supported)" ;;
esac

case "${RUNNER_OS:-}" in
  Linux) target="$cpu-unknown-linux-gnu"; ext=tar.gz; exe=know ;;
  macOS) target="$cpu-apple-darwin"; ext=tar.gz; exe=know ;;
  Windows) target="$cpu-pc-windows-msvc"; ext=zip; exe=know.exe ;;
  *) die "unsupported runner OS '${RUNNER_OS:-}' (Linux, macOS and Windows are supported)" ;;
esac

name="knowell-$version-$target"
set_output target "$target"
set_output name "$name"
set_output archive "$name.$ext"
set_output exe "$exe"
