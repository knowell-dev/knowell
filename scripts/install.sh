#!/bin/sh
# Install the Knowell `know` command from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/knowell-dev/knowell/main/scripts/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --version 1.0.0
#
# Options:
#   --version X.Y.Z        install this version (default: the latest release)
#   --install-dir DIR      install into DIR (default: $KNOWELL_INSTALL_DIR or ~/.local/bin)
#   --attestation MODE     auto (default): verify build provenance when an authenticated
#                          `gh` is available; require: fail without it; skip: never check
#   -h, --help             show this help
#
# The archive is always verified against the release's SHA256SUMS. The script never
# uses sudo: if the install directory is not writable it stops and says so.
#
# Environment (mainly for testing): KNOWELL_REPO (default knowell-dev/knowell),
# KNOWELL_DOWNLOAD_BASE (replaces https://github.com/<repo>/releases/download).

set -eu

REPO="${KNOWELL_REPO:-knowell-dev/knowell}"
VERSION="${KNOWELL_VERSION:-}"
INSTALL_DIR="${KNOWELL_INSTALL_DIR:-}"
ATTESTATION="auto"

say() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'USAGE'
Install the Knowell `know` command from a GitHub release.

Usage: install.sh [--version X.Y.Z] [--install-dir DIR] [--attestation auto|require|skip]

  --version X.Y.Z      install this version (default: the latest release)
  --install-dir DIR    install into DIR (default: $KNOWELL_INSTALL_DIR or ~/.local/bin)
  --attestation MODE   auto (default): verify build provenance when an authenticated
                       gh is available; require: fail without it; skip: never check
  -h, --help           show this help

The archive is always verified against the release's SHA256SUMS. The script never
uses sudo: if the install directory is not writable it stops and says so.
USAGE
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) [ $# -ge 2 ] || die "--version needs a value"; VERSION="$2"; shift 2 ;;
    --version=*) VERSION="${1#--version=}"; shift ;;
    --install-dir) [ $# -ge 2 ] || die "--install-dir needs a value"; INSTALL_DIR="$2"; shift 2 ;;
    --install-dir=*) INSTALL_DIR="${1#--install-dir=}"; shift ;;
    --attestation) [ $# -ge 2 ] || die "--attestation needs a value"; ATTESTATION="$2"; shift 2 ;;
    --attestation=*) ATTESTATION="${1#--attestation=}"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1 (try --help)" ;;
  esac
done

case "$ATTESTATION" in auto|require|skip) ;; *) die "--attestation must be auto, require or skip" ;; esac

have() { command -v "$1" >/dev/null 2>&1; }

# --- download helpers -------------------------------------------------------------

fetch() { # fetch URL DEST
  if have curl; then
    curl --proto '=https,file' --tlsv1.2 -fsSL --retry 3 -o "$2" "$1"
  elif have wget; then
    wget -q -O "$2" "$1"
  else
    die "need curl or wget"
  fi
}

latest_version() {
  # The /releases/latest redirect needs no API token and has no rate limit.
  if have curl; then
    url=$(curl --proto '=https' --tlsv1.2 -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest") || return 1
  elif have wget; then
    url=$(wget -q --max-redirect=5 -S --spider "https://github.com/$REPO/releases/latest" 2>&1 | sed -n 's/^ *[Ll]ocation: //p' | tail -n 1 | tr -d '\r') || return 1
  else
    die "need curl or wget"
  fi
  tag="${url##*/}"
  case "$tag" in v[0-9]*) printf '%s' "${tag#v}" ;; *) return 1 ;; esac
}

sha256_of() {
  if have sha256sum; then sha256sum "$1" | cut -d ' ' -f 1
  elif have shasum; then shasum -a 256 "$1" | cut -d ' ' -f 1
  elif have openssl; then openssl dgst -sha256 "$1" | sed 's/^.*= //'
  else die "need sha256sum, shasum or openssl to verify the download"
  fi
}

# --- platform ---------------------------------------------------------------------

detect_target() {
  os=$(uname -s)
  arch=$(uname -m)
  case "$os" in
    Linux)
      case "$arch" in
        x86_64|amd64) cpu=x86_64 ;;
        aarch64|arm64) cpu=aarch64 ;;
        *) die "unsupported CPU: $arch" ;;
      esac
      libc=gnu
      if have ldd && ldd --version 2>&1 | grep -qi musl; then libc=musl; fi
      printf '%s-unknown-linux-%s' "$cpu" "$libc"
      ;;
    Darwin)
      case "$arch" in
        x86_64)
          cpu=x86_64
          # A shell running under Rosetta reports x86_64 on Apple silicon.
          if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then cpu=aarch64; fi
          ;;
        arm64|aarch64) cpu=aarch64 ;;
        *) die "unsupported CPU: $arch" ;;
      esac
      printf '%s-apple-darwin' "$cpu"
      ;;
    *) die "unsupported OS: $os (on Windows use scripts/install.ps1)" ;;
  esac
}

# --- main -------------------------------------------------------------------------

if [ -z "$VERSION" ]; then
  say "Looking up the latest release of $REPO ..."
  VERSION=$(latest_version) || die "could not determine the latest release (is there one yet? use --version)"
fi
VERSION="${VERSION#v}"
case "$VERSION" in
  *[!0-9A-Za-z.-]*|'') die "invalid version: $VERSION" ;;
esac
printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$' || die "invalid version: $VERSION"

TARGET=$(detect_target)
NAME="knowell-$VERSION-$TARGET"
ARCHIVE="$NAME.tar.gz"
BASE="${KNOWELL_DOWNLOAD_BASE:-https://github.com/$REPO/releases/download}/v$VERSION"

if [ -z "$INSTALL_DIR" ]; then
  [ -n "${HOME:-}" ] || die "HOME is not set; pass --install-dir"
  INSTALL_DIR="$HOME/.local/bin"
fi

TMP=$(mktemp -d 2>/dev/null || mktemp -d -t knowell) || die "cannot create a temporary directory"
trap 'rm -rf "$TMP"' EXIT INT HUP TERM

say "Downloading Knowell $VERSION for $TARGET ..."
fetch "$BASE/$ARCHIVE" "$TMP/$ARCHIVE" || die "download failed: $BASE/$ARCHIVE (is $VERSION released for $TARGET?)"
fetch "$BASE/SHA256SUMS" "$TMP/SHA256SUMS" || die "download failed: $BASE/SHA256SUMS"

expected=$(awk -v f="$ARCHIVE" '{ n = $2; sub(/^\*/, "", n); if (n == f) { print tolower($1); exit } }' "$TMP/SHA256SUMS")
[ -n "$expected" ] || die "SHA256SUMS has no entry for $ARCHIVE"
actual=$(sha256_of "$TMP/$ARCHIVE")
[ "$expected" = "$actual" ] || die "checksum mismatch for $ARCHIVE (expected $expected, got $actual); nothing was installed"
say "Checksum verified."

# Build provenance: stronger than a checksum served from the same place.
if [ "$ATTESTATION" != "skip" ]; then
  if have gh && gh auth status >/dev/null 2>&1; then
    if gh attestation verify "$TMP/$ARCHIVE" --repo "$REPO" >/dev/null 2>&1; then
      say "Build provenance attestation verified."
    else
      die "attestation verification failed for $ARCHIVE; nothing was installed"
    fi
  elif [ "$ATTESTATION" = "require" ]; then
    die "--attestation require needs the GitHub CLI (gh), logged in"
  else
    say "Skipping attestation check (install and log in to gh to enable it)."
  fi
fi

mkdir "$TMP/x"
tar -xzf "$TMP/$ARCHIVE" -C "$TMP/x" || die "could not extract $ARCHIVE"
BIN="$TMP/x/$NAME/know"
[ -f "$BIN" ] || die "the archive does not contain $NAME/know"

mkdir -p "$INSTALL_DIR" 2>/dev/null || true
[ -d "$INSTALL_DIR" ] && [ -w "$INSTALL_DIR" ] || die "cannot write to $INSTALL_DIR; choose another directory with --install-dir (this script never uses sudo)"

# Copy then rename so a running `know` is replaced atomically.
STAGED="$INSTALL_DIR/.know.$$"
if ! { cp "$BIN" "$STAGED" && chmod 755 "$STAGED" && mv -f "$STAGED" "$INSTALL_DIR/know"; }; then
  rm -f "$STAGED"
  die "could not install into $INSTALL_DIR"
fi

say "Installed know $VERSION to $INSTALL_DIR/know"
case ":${PATH:-}:" in
  *":$INSTALL_DIR:"*) ;;
  *) say "Add it to your PATH, for example:  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
