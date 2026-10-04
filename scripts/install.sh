#!/bin/sh
# Initial direct installation only. Existing installations use `know update`.
# Checksums authenticate neither the first launcher nor its trust root: see docs/RELEASING.md.
set -eu
umask 077

REPO="${KNOWELL_REPO:-knowell-dev/knowell}"
VERSION="${KNOWELL_VERSION:-}"
INSTALL_DIR="${KNOWELL_INSTALL_DIR:-}"
ATTESTATION=auto
DEFAULT_DIR=false
MAX_BINARY=536870912
TMP=
LOCK=

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }
usage() {
  cat <<'USAGE'
Install the Knowell direct launcher and an immutable runtime.
Usage: install.sh [--version X.Y.Z] [--install-dir DIR] [--attestation auto|require|skip]

--install-dir is a dedicated, new installation root, not a shared bin directory.
Default: ~/.local/share/knowell/install, with a new ~/.local/bin/know symlink.
Existing or linked destinations are refused. Upgrade an installed copy with know update.
The first download is checksum-verified; optional gh attestation verifies provenance.
Native updates fail closed until an operator provisions a trusted TUF repository.
USAGE
}
while [ $# -gt 0 ]; do
  case "$1" in
    --version) [ $# -ge 2 ] || die '--version needs a value'; VERSION="$2"; shift 2 ;;
    --version=*) VERSION="${1#--version=}"; shift ;;
    --install-dir) [ $# -ge 2 ] || die '--install-dir needs a value'; INSTALL_DIR="$2"; shift 2 ;;
    --install-dir=*) INSTALL_DIR="${1#--install-dir=}"; shift ;;
    --attestation) [ $# -ge 2 ] || die '--attestation needs a value'; ATTESTATION="$2"; shift 2 ;;
    --attestation=*) ATTESTATION="${1#--attestation=}"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die 'unknown installer option (try --help)' ;;
  esac
done
case "$ATTESTATION" in auto|require|skip) ;; *) die '--attestation must be auto, require or skip' ;; esac
have curl || die 'need curl for bounded HTTPS downloads'
have awk || die 'need awk for canonical release identity validation'
have head || die 'need head for bounded download streams'
case "$REPO" in *[!A-Za-z0-9_./-]*|/*|*/../*|*//*|*/*/*) die 'invalid repository identity' ;; esac
case "$REPO" in */*) ;; *) die 'invalid repository identity' ;; esac

cleanup() {
  [ -z "$TMP" ] || rm -rf "$TMP"
  [ -z "$LOCK" ] || rmdir "$LOCK" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 1' INT HUP TERM

fetch() {
  # Older curl limits only advertised sizes. Bound actual bytes independently,
  # and preserve curl's status rather than accepting the last pipeline command.
  status_file="$2.status"
  (
    if curl --proto '=https,file' --proto-redir '=https,file' --tlsv1.2 -fsSL --retry 3 \
      --max-redirs 5 --connect-timeout 15 --max-time 300 --max-filesize "$3" "$1" 2>/dev/null; then
      printf '0\n' > "$status_file"
    else
      printf '%s\n' "$?" > "$status_file"
    fi
  ) | head -c "$(( $3 + 1 ))" > "$2" || die 'cannot write the bounded download'
  size=$(wc -c < "$2" | tr -d ' ')
  [ "$size" -gt 0 ] && [ "$size" -le "$3" ] || die 'download is empty or exceeds its byte limit'
  [ -f "$status_file" ] && [ "$(cat "$status_file")" = 0 ] || die 'download failed or was truncated; nothing was installed'
}
sha256_of() {
  if have sha256sum; then sha256sum "$1" | cut -d ' ' -f 1
  elif have shasum; then shasum -a 256 "$1" | cut -d ' ' -f 1
  elif have openssl; then openssl dgst -sha256 "$1" | sed 's/^.*= //'
  else die 'need sha256sum, shasum or openssl'
  fi
}
verify_path() {
  # Never follow an existing ancestor symlink into another installation's state.
  old_ifs=$IFS
  IFS=/
  set -f
  set -- $1
  IFS=$old_ifs
  cursor=
  for part do
    [ -z "$part" ] && continue
    case "$part" in .|..) die 'installation path must not contain dot components' ;; esac
    cursor="$cursor/$part"
    [ ! -L "$cursor" ] || die 'installation path must not contain symbolic links'
  done
  set +f
}
latest_version() {
  url=$(curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL --max-redirs 5 --max-time 60 \
    -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" 2>/dev/null) || return 1
  tag="${url##*/}"
  case "$tag" in v[0-9]*) printf '%s' "${tag#v}" ;; *) return 1 ;; esac
}
detect_target() {
  case "$(uname -m)" in x86_64|amd64) cpu=x86_64 ;; aarch64|arm64) cpu=aarch64 ;; *) die 'unsupported CPU architecture' ;; esac
  case "$(uname -s)" in
    Linux)
      libc=
      if have ldd; then
        libc_info=$(ldd --version 2>&1 || true)
        if printf '%s' "$libc_info" | grep -qi musl; then libc=musl
        elif printf '%s' "$libc_info" | grep -Eqi '^ldd .*(GNU libc|GLIBC|glibc)'; then libc=gnu
        fi
      fi
      if [ -z "$libc" ] && have getconf; then
        libc_info=$(getconf GNU_LIBC_VERSION 2>/dev/null || true)
        if printf '%s' "$libc_info" | grep -Eqi '^glibc [0-9]+(\.[0-9]+)+$'; then libc=gnu; fi
      fi
      [ -n "$libc" ] || die 'cannot identify Linux libc; install a usable ldd or getconf before choosing a release'
      printf '%s-unknown-linux-%s' "$cpu" "$libc" ;;
    Darwin)
      if [ "$cpu" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then cpu=aarch64; fi
      printf '%s-apple-darwin' "$cpu" ;;
    *) die 'unsupported OS (on Windows use install.ps1)' ;;
  esac
}
if [ -z "$VERSION" ]; then
  say 'Looking up the latest Knowell release ...'
  VERSION=$(latest_version) || die 'could not determine the latest release; use --version'
fi
VERSION="${VERSION#v}"
[ "${#VERSION}" -le 128 ] || die 'invalid version'
printf '%s' "$VERSION" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$' || die 'invalid version'
printf '%s' "$VERSION" | awk '
  { split($0, fields, /[-.]/)
    for (i=1; i<=3; i++) if (length(fields[i])>20 || (length(fields[i])==20 && "x" fields[i]>"x18446744073709551615")) exit 1
    if (index($0,"-")) { pre=$0; sub(/^[0-9]+\.[0-9]+\.[0-9]+-/,"",pre); count=split(pre, parts, /\./)
      for (i=1; i<=count; i++) if (parts[i] ~ /^0[0-9]+$/) exit 1
    }
  }' || die 'invalid version'
case "$VERSION" in 0.*|1.0.0-*) die 'public installations require the first stable 1.0 release or later' ;; esac
TARGET=$(detect_target)
if [ -z "$INSTALL_DIR" ]; then
  [ -n "${HOME:-}" ] || die 'HOME is not set; pass --install-dir'
  INSTALL_DIR="$HOME/.local/share/knowell/install"
  DEFAULT_DIR=true
fi
case "$INSTALL_DIR" in /*) ;; *) die '--install-dir must be an absolute dedicated directory' ;; esac
INSTALL_DIR="${INSTALL_DIR%/}"
[ -n "$INSTALL_DIR" ] || die '--install-dir must be a dedicated directory below a private parent'
verify_path "$INSTALL_DIR"
[ ! -e "$INSTALL_DIR" ] && [ ! -L "$INSTALL_DIR" ] || die 'installation destination already exists; use know update or an explicit repair procedure'
if [ "$DEFAULT_DIR" = true ]; then
  verify_path "$HOME/.local/bin"
  [ ! -e "$HOME/.local/bin/know" ] && [ ! -L "$HOME/.local/bin/know" ] || die 'an existing know command would be shadowed; choose a separate --install-dir'
fi
parent=$(dirname "$INSTALL_DIR")
mkdir -p "$parent" || die 'cannot create the installation parent'
# Portable mv has no universal no-replace-directory primitive. A private parent
# excludes other users from racing publication; the lock coordinates bootstraps.
[ "$(find "$parent" -prune -user "$(id -u)" -print)" = "$parent" ] || die 'installation parent must be owned by the current user; choose a private dedicated subdirectory'
[ -z "$(find "$parent" -prune \( -perm -0020 -o -perm -0002 \) -print)" ] || die 'installation parent must not be writable by other users; choose a private dedicated subdirectory'
candidate_lock="$INSTALL_DIR.bootstrap-lock"
mkdir "$candidate_lock" 2>/dev/null || die 'another bootstrap owns this destination'
LOCK="$candidate_lock"
# Once locked, recheck before any publication into the installation root.
[ ! -e "$INSTALL_DIR" ] && [ ! -L "$INSTALL_DIR" ] || die 'installation destination changed during bootstrap'
TMP=$(mktemp -d "$parent/.knowell-bootstrap.XXXXXX") || die 'cannot create private bootstrap staging'
BASE="${KNOWELL_DOWNLOAD_BASE:-https://github.com/$REPO/releases/download}/v$VERSION"
fetch "$BASE/SHA256SUMS" "$TMP/SHA256SUMS" 1048576

raw_asset() {
  awk -v suffix=".$1" '
    { digest=tolower($1); name=$2; sub(/^\*/, "", name)
      if (length(digest)==64 && digest !~ /[^0-9a-f]/ && name == digest suffix) { result=name; count++ }
    }
    END { if (count == 1) print result; else exit 1 }
  ' "$TMP/SHA256SUMS" || die 'SHA256SUMS must list exactly one canonical raw component'
}
ENGINE_NAME=$(raw_asset "knowell-$VERSION-$TARGET-engine")
LAUNCHER_NAME=$(raw_asset "knowell-$VERSION-$TARGET-launcher")
ENGINE_SHA="${ENGINE_NAME%%.*}"
LAUNCHER_SHA="${LAUNCHER_NAME%%.*}"
say "Downloading Knowell $VERSION for $TARGET ..."
fetch "$BASE/$ENGINE_NAME" "$TMP/engine" "$MAX_BINARY"
fetch "$BASE/$LAUNCHER_NAME" "$TMP/launcher" "$MAX_BINARY"
if [ "$DEFAULT_DIR" = true ]; then
  # Some compatibility shells copy instead of creating links; such a launcher cannot
  # discover its installation root. Check privately before publishing any command.
  ln -s "$TMP/launcher" "$TMP/linkcheck" 2>/dev/null || die 'this host cannot create the required symbolic link; use --install-dir or the native Windows installer'
  [ -L "$TMP/linkcheck" ] || die 'this host emulates symbolic links; use --install-dir or the native Windows installer'
fi
[ "$(sha256_of "$TMP/engine")" = "$ENGINE_SHA" ] || die 'runtime checksum mismatch; nothing was installed'
[ "$(sha256_of "$TMP/launcher")" = "$LAUNCHER_SHA" ] || die 'launcher checksum mismatch; nothing was installed'
say 'Checksum verified. Initial bootstrap trust is independent of native TUF update verification.'
if [ "$ATTESTATION" != skip ]; then
  if have gh && gh auth status >/dev/null 2>&1; then
    for component in engine launcher; do
      gh attestation verify "$TMP/$component" --repo "$REPO" \
        --signer-workflow "$REPO/.github/workflows/release.yml" --source-ref "refs/tags/v$VERSION" >/dev/null 2>&1 || die 'attestation verification failed; nothing was installed'
    done
    say 'Build provenance attestation verified.'
  elif [ "$ATTESTATION" = require ]; then die '--attestation require needs gh, logged in'
  else say 'Skipping attestation check (use --attestation require to require provenance).'
  fi
fi
STAGE="$TMP/install"
mkdir -p "$STAGE/versions/$VERSION/$TARGET" "$STAGE/metadata"
chmod 700 "$TMP/engine" "$TMP/launcher"
mv "$TMP/engine" "$STAGE/versions/$VERSION/$TARGET/know"
mv "$TMP/launcher" "$STAGE/know"
ENGINE_SIZE=$(wc -c < "$STAGE/versions/$VERSION/$TARGET/know" | tr -d ' ')
LAUNCHER_SIZE=$(wc -c < "$STAGE/know" | tr -d ' ')
printf '{"format_version":1,"owner":"direct","target":"%s","launcher_protocol":1}\n' "$TARGET" > "$STAGE/install.json"
printf '{"format_version":1,"version":"%s","target":"%s","sha256":"%s","size":%s}\n' "$VERSION" "$TARGET" "$ENGINE_SHA" "$ENGINE_SIZE" > "$STAGE/current.json"
printf '{"format_version":1,"version":"%s","target":"%s","sha256":"%s","size":%s}\n' "$VERSION" "$TARGET" "$LAUNCHER_SHA" "$LAUNCHER_SIZE" > "$STAGE/launcher.json"
[ ! -e "$INSTALL_DIR" ] && [ ! -L "$INSTALL_DIR" ] || die 'installation destination changed before publication'
mv "$STAGE" "$INSTALL_DIR" || die 'could not publish the initial installation'
if [ "$DEFAULT_DIR" = true ]; then
  mkdir -p "$HOME/.local/bin"
  ln -s "$INSTALL_DIR/know" "$HOME/.local/bin/know" || die 'installation is ready but a competing know entrypoint appeared; no command was overwritten'
  case ":${PATH:-}:" in *":$HOME/.local/bin:"*) ;; *) say 'Add ~/.local/bin to PATH to use know.' ;; esac
else
  say 'Add the chosen dedicated installation directory to PATH to use know.'
fi
say "Installed know $VERSION with a private immutable runtime."
say 'Existing installations use know update; install scripts never overwrite a running launcher.'
