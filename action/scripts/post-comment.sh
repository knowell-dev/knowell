#!/usr/bin/env bash
# Create or update ONE marker-delimited PR comment (impact) or issue (doc-drift).
#
# Input env:
#   GH_TOKEN         github-token input (needs pull-requests: write / issues: write)
#   KNOWELL_MODE     pr-comment | issue
#   KNOWELL_REPOSITORY   owner/repo
#   KNOWELL_PR_NUMBER    pull request number (pr-comment)
#   KNOWELL_REPORT_FILE  markdown produced by know
#   KNOWELL_ISSUE_TITLE  issue title (issue mode; default below)
#
# The marker identifies our comment. Only comments written by a bot account are updated, so
# a human quoting the marker cannot hijack the update path.
set -euo pipefail
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

mode="${KNOWELL_MODE:?KNOWELL_MODE is required}"
repo="${KNOWELL_REPOSITORY:?KNOWELL_REPOSITORY is required}"
report="${KNOWELL_REPORT_FILE:?KNOWELL_REPORT_FILE is required}"
max_bytes=60000 # GitHub rejects comment bodies above 65536 characters

printf '%s' "$repo" | grep -Eq '^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$' || die "invalid repository '$repo'"
[ -s "$report" ] || {
  note "no report to post"
  exit 0
}
have gh || die "the GitHub CLI (gh) is required to post comments"

case "$mode" in
  pr-comment)
    marker='<!-- knowell-impact -->'
    number="${KNOWELL_PR_NUMBER:-}"
    printf '%s' "$number" | grep -Eq '^[0-9]+$' || {
      note "not a pull request run: no comment posted"
      exit 0
    }
    ;;
  issue)
    marker='<!-- knowell-doc-drift -->'
    title="${KNOWELL_ISSUE_TITLE:-Knowell: documentation drift}"
    ;;
  *) die "unknown mode '$mode'" ;;
esac

body="$(mktemp)"
trap 'rm -f "$body"' EXIT
{
  printf '%s\n' "$marker"
  head -c "$max_bytes" "$report"
  if [ "$(wc -c <"$report" | tr -d ' ')" -gt "$max_bytes" ]; then
    printf '\n\n_Report truncated to fit the GitHub comment size limit._\n'
  fi
  printf '\n'
} >"$body"

# The marker is a fixed string below; it is never taken from input.
if [ "$mode" = pr-comment ]; then
  existing=$(gh api --paginate "repos/$repo/issues/$number/comments?per_page=100" \
    --jq '.[] | select(.user.type == "Bot") | select(.body | startswith("<!-- knowell-impact -->")) | .id' | head -n 1)
  if [ -n "$existing" ]; then
    gh api --method PATCH "repos/$repo/issues/comments/$existing" -F "body=@$body" >/dev/null
    say "Updated the Knowell comment ($existing)."
  else
    gh api --method POST "repos/$repo/issues/$number/comments" -F "body=@$body" >/dev/null
    say "Posted a new Knowell comment."
  fi
else
  existing=$(gh api --paginate "repos/$repo/issues?state=open&per_page=100" \
    --jq '.[] | select(.pull_request == null) | select(.user.type == "Bot") | select(.body // "" | startswith("<!-- knowell-doc-drift -->")) | .number' | head -n 1)
  if [ -n "$existing" ]; then
    gh api --method PATCH "repos/$repo/issues/$existing" -F "body=@$body" >/dev/null
    say "Updated the doc-drift issue (#$existing)."
  else
    gh api --method POST "repos/$repo/issues" -f "title=$title" -F "body=@$body" >/dev/null
    say "Opened a doc-drift issue."
  fi
fi
