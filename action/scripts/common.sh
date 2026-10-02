#!/usr/bin/env bash
# Shared helpers for the Knowell Check action scripts. Source this file; do not execute it.
# Every script reads its input from environment variables (never from interpolated
# workflow expressions) and reports results through $GITHUB_OUTPUT.

die() {
  printf '::error::%s\n' "$*" >&2
  exit 1
}

warn() { printf '::warning::%s\n' "$*" >&2; }
note() { printf '::notice::%s\n' "$*" >&2; }
say() { printf '%s\n' "$*" >&2; }

# set_output NAME VALUE: values must be single-line so an output can never inject another.
set_output() {
  local name="$1" value="$2"
  case "$value" in
    *$'\n'* | *$'\r'*) die "refusing to write a multi-line value to output '$name'" ;;
  esac
  if [ -n "${GITHUB_OUTPUT:-}" ]; then
    printf '%s=%s\n' "$name" "$value" >>"$GITHUB_OUTPUT"
  else
    printf '%s=%s\n' "$name" "$value"
  fi
}

have() { command -v "$1" >/dev/null 2>&1; }

# sha256_of FILE: lowercase hex digest, using whatever the runner has.
sha256_of() {
  local out
  if have sha256sum; then
    out=$(sha256sum "$1")
  elif have shasum; then
    out=$(shasum -a 256 "$1")
  elif have openssl; then
    out=$(openssl dgst -sha256 "$1" | sed 's/^.*= //')
  else
    die "no sha256 tool found (sha256sum, shasum or openssl)"
  fi
  printf '%s' "${out%% *}" | tr '[:upper:]' '[:lower:]'
}

# normalize_bool VALUE: true or false only (case-insensitive); prints the normalised value.
normalize_bool() {
  case "$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')" in
    true) printf 'true' ;;
    false) printf 'false' ;;
    *) return 1 ;;
  esac
}

# to_posix PATH: map a Windows path to the form bash understands (no-op elsewhere).
to_posix() {
  if have cygpath; then cygpath -u "$1"; else printf '%s' "$1"; fi
}

# is_semver VALUE: X.Y.Z with an optional pre-release suffix, no leading "v".
is_semver() {
  printf '%s' "$1" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'
}
