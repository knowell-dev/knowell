#!/usr/bin/env bash
# Resolve the requested version to a concrete X.Y.Z and its tag.
#
# Input env:  KNOWELL_VERSION   "latest", "1.2.3" or "v1.2.3"
#             KNOWELL_RELEASES_URL  override of https://github.com/knowell-dev/knowell/releases (tests)
# Outputs:    version, tag
#
# "latest" follows the redirect of <releases>/latest, which needs no API token and has no
# rate limit; the tag is the last path segment of the redirect target.
set -euo pipefail
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

requested="${KNOWELL_VERSION:-latest}"
releases="${KNOWELL_RELEASES_URL:-https://github.com/knowell-dev/knowell/releases}"

if [ "$requested" = "latest" ]; then
  note "version 'latest' is not reproducible; pin a version in production workflows"
  location=$(curl -fsSI --proto '=https,file' --max-time 30 --retry 3 -o /dev/null -w '%{redirect_url}' "$releases/latest") ||
    die "could not look up the latest Knowell release"
  [ -n "$location" ] || die "could not look up the latest Knowell release (no redirect; is there a release yet?)"
  requested="${location##*/}"
fi

version="${requested#v}"
is_semver "$version" || die "invalid version '$requested': expected latest, X.Y.Z or vX.Y.Z"

set_output version "$version"
set_output tag "v$version"
