#!/usr/bin/env bash
# Validate the action inputs and describe the run context.
#
# Input env (all supplied through `env:` in action.yml, never interpolated into a script):
#   IN_COMMAND IN_VERSION IN_CONFIG IN_SARIF IN_SARIF_FILE IN_FAIL_ON IN_HUB_URL
#   IN_OIDC_AUDIENCE IN_COMMENT IN_ATTESTATION IN_WORKDIR IN_SARIF_CATEGORY
#   EVENT_NAME REPOSITORY PR_HEAD_REPO PR_BASE_SHA PUSH_BEFORE_SHA
#   GITHUB_WORKSPACE RUNNER_TEMP ACTIONS_ID_TOKEN_REQUEST_URL (presence only)
# Outputs: normalised inputs plus fork, oidc-available, run, upload-sarif, diff-base, sarif-file.
set -euo pipefail
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

command_in="${IN_COMMAND:-check}"
case "$command_in" in
  check | impact | index | doc-drift) ;;
  *) die "command must be one of: check, impact, index, doc-drift (got '$command_in')" ;;
esac

version_in="${IN_VERSION:-latest}"
if [ "$version_in" != latest ]; then
  is_semver "${version_in#v}" || die "version must be 'latest', X.Y.Z or vX.Y.Z"
fi

# Paths: relative, inside the checkout, no traversal, no option-looking or odd characters.
safe_relpath() {
  local label="$1" value="$2"
  case "$value" in
    '' | /* | [A-Za-z]:* | -* | *..* | *[!A-Za-z0-9._/@+-]*)
      die "$label must be a plain relative path inside the workspace (got '$value')"
      ;;
  esac
}
config="${IN_CONFIG:-knowell.toml}"
workdir="${IN_WORKDIR:-.}"
safe_relpath workspace-config "$config"
safe_relpath working-directory "$workdir"

fail_on="${IN_FAIL_ON:-error}"
case "$fail_on" in error | warning | never) ;; *) die "fail-on must be error, warning or never (got '$fail_on')" ;; esac

sarif_in=$(normalize_bool "${IN_SARIF:-true}") || die "sarif must be true or false"
comment_in=$(normalize_bool "${IN_COMMENT:-true}") || die "comment must be true or false"
attestation="${IN_ATTESTATION:-auto}"
case "$attestation" in auto | require | skip) ;; *) die "attestation must be auto, require or skip" ;; esac

url_chars='A-Za-z0-9._~:/?#@!$&+,;=%-'
hub="${IN_HUB_URL:-}"
if [ -n "$hub" ]; then
  printf '%s' "$hub" | grep -Eq "^https://[$url_chars]+\$" || die "hub-url must be an https:// URL without spaces or quotes"
fi
aud="${IN_OIDC_AUDIENCE:-}"
if [ -n "$aud" ]; then
  printf '%s' "$aud" | grep -Eq "^[$url_chars]+\$" || die "oidc-audience contains unsupported characters"
fi
category="${IN_SARIF_CATEGORY:-knowell}"
printf '%s' "$category" | grep -Eq '^[A-Za-z0-9._/-]{1,100}$' || die "sarif-category contains unsupported characters"

case "$command_in" in
  impact | index) [ -n "$hub" ] || die "command '$command_in' needs hub-url" ;;
esac

# --- context -------------------------------------------------------------------------
event="${EVENT_NAME:-}"
if [ "$event" = pull_request_target ]; then
  die "pull_request_target is not supported: it runs with repository secrets next to untrusted pull request content. Use the pull_request event."
fi

fork=false
if [ "$event" = pull_request ] && [ -n "${PR_HEAD_REPO:-}" ] && [ "${PR_HEAD_REPO}" != "${REPOSITORY:-}" ]; then
  fork=true
fi

oidc=false
if [ -n "${ACTIONS_ID_TOKEN_REQUEST_URL:-}" ] && [ "$fork" = false ]; then oidc=true; fi

needs_hub=false
if [ "$command_in" = impact ] || [ "$command_in" = index ]; then needs_hub=true; fi

run=true
if [ "$needs_hub" = true ] && [ "$oidc" = false ]; then
  run=false
  if [ "$fork" = true ]; then
    note "pull request from a fork: skipping '$command_in' (forks get no OIDC token and no secrets)"
  else
    note "no OIDC token available: skipping '$command_in'. Grant the job 'permissions: id-token: write'."
  fi
fi

upload=false
if [ "$command_in" = check ] && [ "$sarif_in" = true ]; then
  if [ "$fork" = false ]; then
    upload=true
  else
    note "pull request from a fork: SARIF is written but not uploaded to code scanning (read-only token)"
  fi
fi

# Diff base: the PR base commit, or the previous tip of a push. All zeros means a new branch.
diff_base=""
case "$event" in
  pull_request) diff_base="${PR_BASE_SHA:-}" ;;
  push) diff_base="${PUSH_BEFORE_SHA:-}" ;;
esac
if ! printf '%s' "$diff_base" | grep -Eq '^([0-9a-f]{40}|[0-9a-f]{64})$' || printf '%s' "$diff_base" | grep -Eq '^0+$'; then
  diff_base=""
fi

# SARIF path: relative paths live under the workspace; absolute paths must be inside the
# workspace or the runner temp dir. Default: <runner temp>/knowell.sarif.
workspace_native="${GITHUB_WORKSPACE:-$PWD}"
temp_native="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
workspace=$(to_posix "$workspace_native")
temp=$(to_posix "$temp_native")
sarif_file="${IN_SARIF_FILE:-}"
if [ -z "$sarif_file" ]; then
  sarif_path="$temp_native/knowell.sarif"
else
  case "$sarif_file" in
    *..* | -* | *[!A-Za-z0-9._/:\@+-]*) die "sarif-file contains '..' or unsupported characters" ;;
  esac
  posix=$(to_posix "$sarif_file")
  case "$posix" in
    /*)
      case "$posix" in
        "$workspace"/* | "$temp"/*) sarif_path="$sarif_file" ;;
        *) die "sarif-file must be inside the workspace or the runner temp directory" ;;
      esac
      ;;
    *) sarif_path="$workspace_native/$sarif_file" ;;
  esac
fi

set_output command "$command_in"
set_output version "$version_in"
set_output config "$config"
set_output workdir "$workdir"
set_output fail-on "$fail_on"
set_output sarif "$sarif_in"
set_output comment "$comment_in"
set_output attestation "$attestation"
set_output hub-url "$hub"
set_output oidc-audience "$aud"
set_output sarif-category "$category"
set_output fork "$fork"
set_output oidc-available "$oidc"
set_output run "$run"
set_output upload-sarif "$upload"
set_output diff-base "$diff_base"
set_output sarif-file "$sarif_path"
