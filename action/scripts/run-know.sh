#!/usr/bin/env bash
# Run `know` for the selected command and record the result. This script never fails the
# step itself (except on bad setup): it writes `exit-code` so that later steps (SARIF upload,
# PR comment) still run, and the final step turns a non-zero code into a failure.
#
# Input env:
#   KNOWELL_BIN        path of the know executable
#   KNOWELL_COMMAND    check | impact | index | doc-drift
#   KNOWELL_CONFIG     workspace config, relative to the working directory
#   KNOWELL_WORKDIR    working directory, relative to the checkout (default .)
#   KNOWELL_FAIL_ON    error | warning | never
#   KNOWELL_HUB_URL, KNOWELL_OIDC_AUDIENCE
#   KNOWELL_SARIF_FILE absolute SARIF path (check)
#   KNOWELL_DIFF_BASE  commit to diff against, or empty
#   KNOWELL_REPORT_FILE  where markdown reports go (impact, doc-drift)
#   KNOWELL_COMMIT, KNOWELL_REF   commit sha and ref (index)
#   ACTIONS_ID_TOKEN_REQUEST_URL / ACTIONS_ID_TOKEN_REQUEST_TOKEN   (impact, index)
# Outputs: exit-code, findings, sarif-file, sarif-exists, report-file, report-exists
set -euo pipefail
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

bin="${KNOWELL_BIN:?KNOWELL_BIN is required}"
command_in="${KNOWELL_COMMAND:?KNOWELL_COMMAND is required}"
config="${KNOWELL_CONFIG:-knowell.toml}"
workdir="${KNOWELL_WORKDIR:-.}"
fail_on="${KNOWELL_FAIL_ON:-error}"
hub="${KNOWELL_HUB_URL:-}"
aud="${KNOWELL_OIDC_AUDIENCE:-}"
sarif_file="${KNOWELL_SARIF_FILE:-}"
diff_base="${KNOWELL_DIFF_BASE:-}"
report_file="${KNOWELL_REPORT_FILE:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}/knowell-report.md}"
[ -n "$aud" ] || aud="$hub"

[ -x "$bin" ] || die "know executable not found or not executable: $bin"
cd "$(to_posix "${GITHUB_WORKSPACE:-.}")"
cd "$workdir" || die "working-directory '$workdir' does not exist"
[ -f "$config" ] || warn "workspace config '$config' not found in '$workdir'; know will use its defaults or report the problem"

# The diff base is only usable when the commit is present in the checkout.
if [ -n "$diff_base" ]; then
  if ! git cat-file -e "$diff_base^{commit}" 2>/dev/null; then
    if ! git fetch --no-tags --depth=1 --quiet origin "$diff_base" 2>/dev/null || ! git cat-file -e "$diff_base^{commit}" 2>/dev/null; then
      warn "base commit $diff_base is not in the checkout (use fetch-depth: 0 or fetch-depth: 2+ with actions/checkout); analysing the whole workspace instead of the diff"
      diff_base=""
    fi
  fi
fi

# fetch_oidc_token: sets $oidc_token to a JWT for the hub audience and masks it in the log.
# It runs in the main shell (not a command substitution) so the mask command reaches the runner.
fetch_oidc_token() {
  local url="${ACTIONS_ID_TOKEN_REQUEST_URL:-}" bearer="${ACTIONS_ID_TOKEN_REQUEST_TOKEN:-}" enc body tok
  if [ -z "$url" ] || [ -z "$bearer" ]; then
    die "no OIDC token request URL; grant the job 'permissions: id-token: write'"
  fi
  enc=$(printf '%s' "$aud" | sed -e 's/%/%25/g' -e 's/:/%3A/g' -e 's#/#%2F#g' -e 's/?/%3F/g' -e 's/&/%26/g' -e 's/=/%3D/g' -e 's/#/%23/g' -e 's/+/%2B/g')
  body=$(curl -fsS --max-time 30 --retry 2 -H "Authorization: bearer $bearer" "$url&audience=$enc") ||
    die "could not request an OIDC token from GitHub"
  if have jq; then
    tok=$(printf '%s' "$body" | jq -r '.value // empty' | tr -d '\r')
  else
    tok=$(printf '%s' "$body" | sed -n 's/.*"value"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
  fi
  [ -n "$tok" ] || die "GitHub returned no OIDC token"
  printf '::add-mask::%s\n' "$tok"
  oidc_token="$tok"
}

args=(--workspace "$config")
env_token=""
oidc_token=""
report_out=false
sarif_out=false

case "$command_in" in
  check)
    [ -n "$sarif_file" ] || die "KNOWELL_SARIF_FILE is required for check"
    mkdir -p "$(dirname "$sarif_file")"
    rm -f "$sarif_file"
    args+=(check --format sarif --output "$sarif_file" --fail-on "$fail_on")
    [ -z "$diff_base" ] || args+=(--diff-base "$diff_base")
    sarif_out=true
    ;;
  impact)
    [ -n "$hub" ] || die "hub-url is required for impact"
    mkdir -p "$(dirname "$report_file")"
    rm -f "$report_file"
    args+=(impact --hub "$hub" --oidc-audience "$aud" --format markdown --output "$report_file")
    [ -z "$diff_base" ] || args+=(--diff-base "$diff_base")
    fetch_oidc_token
    env_token="$oidc_token"
    report_out=true
    ;;
  index)
    [ -n "$hub" ] || die "hub-url is required for index"
    args+=(index --hub "$hub" --oidc-audience "$aud")
    [ -z "${KNOWELL_COMMIT:-}" ] || args+=(--commit "$KNOWELL_COMMIT")
    [ -z "${KNOWELL_REF:-}" ] || args+=(--ref "${KNOWELL_REF}")
    fetch_oidc_token
    env_token="$oidc_token"
    ;;
  doc-drift)
    mkdir -p "$(dirname "$report_file")"
    rm -f "$report_file"
    args+=(doc-drift --format markdown --output "$report_file")
    report_out=true
    ;;
  *) die "unknown command '$command_in'" ;;
esac

# Only the printed command (without the token) is logged.
say "Running: know ${args[*]}"
rc=0
if [ -n "$env_token" ]; then
  KNOWELL_OIDC_TOKEN="$env_token" "$bin" "${args[@]}" || rc=$?
else
  "$bin" "${args[@]}" || rc=$?
fi

findings=0
sarif_exists=false
if [ "$sarif_out" = true ] && [ -s "$sarif_file" ]; then
  sarif_exists=true
  if have jq; then
    findings=$(jq '[.runs[]?.results[]?] | length' "$sarif_file" 2>/dev/null | tr -d '\r' || true)
  else
    findings=$(grep -o '"ruleId"' "$sarif_file" | wc -l | tr -d ' ')
  fi
  case "$findings" in '' | *[!0-9]*) findings=0 ;; esac
elif [ "$sarif_out" = true ] && [ "$rc" -le 1 ]; then
  warn "know did not write a SARIF file at $sarif_file"
fi
report_exists=false
if [ "$report_out" = true ] && [ -s "$report_file" ]; then report_exists=true; fi

if [ "$rc" -gt 1 ]; then
  warn "know exited with code $rc (an operational error, not a finding)"
fi

set_output exit-code "$rc"
set_output findings "$findings"
set_output sarif-file "$sarif_file"
set_output sarif-exists "$sarif_exists"
set_output report-file "$report_file"
set_output report-exists "$report_exists"
