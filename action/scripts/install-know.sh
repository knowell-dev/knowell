#!/usr/bin/env bash
# Download, verify and unpack the `know` release archive.
#
# Input env:
#   KNOWELL_VERSION, KNOWELL_ARCHIVE, KNOWELL_NAME, KNOWELL_EXE   from resolve-version.sh / asset-name.sh
#   KNOWELL_DL_DIR        directory holding the archive (this is what actions/cache stores)
#   KNOWELL_INSTALL_DIR   directory the binary is unpacked into
#   KNOWELL_ATTESTATION   auto (default) | require | skip
#   KNOWELL_DOWNLOAD_URL  override of https://github.com/knowell-dev/knowell/releases/download (tests)
#   KNOWELL_REPO          override of knowell-dev/knowell (attestation owner check)
#   GH_TOKEN              used only by `gh attestation verify`
# Outputs: bin-dir, bin-path, cache-hit-verified
#
# A cached archive is never trusted: SHA256SUMS is fetched on every run and the archive is
# re-verified, so a poisoned cache entry cannot become the binary that runs.
set -euo pipefail
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

version="${KNOWELL_VERSION:?KNOWELL_VERSION is required}"
archive="${KNOWELL_ARCHIVE:?KNOWELL_ARCHIVE is required}"
name="${KNOWELL_NAME:?KNOWELL_NAME is required}"
exe="${KNOWELL_EXE:?KNOWELL_EXE is required}"
dl_dir="${KNOWELL_DL_DIR:?KNOWELL_DL_DIR is required}"
install_dir="${KNOWELL_INSTALL_DIR:?KNOWELL_INSTALL_DIR is required}"
attestation="${KNOWELL_ATTESTATION:-auto}"
repo="${KNOWELL_REPO:-knowell-dev/knowell}"
base="${KNOWELL_DOWNLOAD_URL:-https://github.com/$repo/releases/download}/v$version"

dl_dir=$(to_posix "$dl_dir")
install_dir=$(to_posix "$install_dir")

case "$attestation" in auto | require | skip) ;; *) die "attestation must be auto, require or skip" ;; esac
case "$archive" in */* | *\\* | '') die "invalid archive name" ;; esac

fetch() { curl -fsSL --proto '=https,file' --max-time 300 --retry 3 -o "$2" "$1"; }

mkdir -p "$dl_dir" "$install_dir"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "download failed: $base/SHA256SUMS"
expected=$(awk -v f="$archive" '{ n = $2; sub(/^\*/, "", n); if (n == f) { print tolower($1); exit } }' "$tmp/SHA256SUMS")
[ -n "$expected" ] || die "SHA256SUMS has no entry for $archive"
printf '%s' "$expected" | grep -Eq '^[0-9a-f]{64}$' || die "SHA256SUMS entry for $archive is not a sha256 digest"

path="$dl_dir/$archive"
cached=false
if [ -f "$path" ]; then
  if [ "$(sha256_of "$path")" = "$expected" ]; then
    cached=true
    say "Using the cached archive $archive (checksum verified)."
  else
    warn "cached $archive does not match SHA256SUMS; discarding it"
    rm -f "$path"
  fi
fi

if [ "$cached" = false ]; then
  say "Downloading $archive ..."
  fetch "$base/$archive" "$tmp/$archive" || die "download failed: $base/$archive (is $version released for this platform?)"
  actual=$(sha256_of "$tmp/$archive")
  [ "$actual" = "$expected" ] || die "checksum mismatch for $archive (expected $expected, got $actual); nothing was installed"
  mv -f "$tmp/$archive" "$path"
  say "Checksum verified."
fi

# Provenance is stronger than a checksum served from the same release.
if [ "$attestation" != skip ]; then
  if have gh && gh auth status >/dev/null 2>&1; then
    if gh attestation verify "$path" --repo "$repo" >/dev/null 2>&1; then
      say "Build provenance attestation verified."
    else
      rm -f "$path"
      die "attestation verification failed for $archive; nothing was installed"
    fi
  elif [ "$attestation" = require ]; then
    die "attestation: require needs the GitHub CLI (gh) with a token (set github-token)"
  else
    note "skipping the attestation check (gh is unavailable or not authenticated)"
  fi
fi

to_win() { if have cygpath; then cygpath -w "$1"; else printf '%s' "$1"; fi; }
extract_zip() {
  if have unzip; then
    unzip -q -o "$1" -d "$2"
  elif have 7z; then
    7z x -y "-o$2" "$1" >/dev/null
  elif have powershell.exe; then
    powershell.exe -NoProfile -Command "Expand-Archive -LiteralPath '$(to_win "$1")' -DestinationPath '$(to_win "$2")' -Force"
  else
    die "no tool to extract a zip archive (unzip, 7z or powershell)"
  fi
}

stage="$tmp/x"
mkdir "$stage"
case "$archive" in
  *.tar.gz) tar -xzf "$path" -C "$stage" ;;
  *.zip) extract_zip "$path" "$stage" ;;
  *) die "unknown archive type: $archive" ;;
esac
[ -f "$stage/$name/$exe" ] || die "the archive does not contain $name/$exe"
cp "$stage/$name/$exe" "$install_dir/$exe"
chmod 755 "$install_dir/$exe"

set_output bin-dir "$install_dir"
set_output bin-path "$install_dir/$exe"
set_output cache-hit-verified "$cached"
