#!/usr/bin/env bash
# shellcheck disable=SC2015 # `cond && ok || fail` is intentional: ok never fails
# Offline tests for the action scripts. No network, no secrets: releases are served from
# file:// URLs, `gh` and `curl` are shimmed by test/bin, and `know` is a fake.
#
#   bash action/test/run-tests.sh
set -uo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
scripts="$here/../scripts"
shim="$here/bin"
# Refuse to run before a missing executable shim can fall through to a real
# network client installed on the runner.
for tool in curl gh know; do
  if [ ! -x "$shim/$tool" ]; then
    printf 'test shim is not executable: %s\n' "$tool" >&2
    exit 1
  fi
done
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

passed=0
failed=0
zip_skipped=false
fail() {
  failed=$((failed + 1))
  printf 'FAIL  %s: %s\n' "$current" "$*"
}
ok() { passed=$((passed + 1)); }
current=""
begin() { current="$1"; }

assert_eq() { # expected actual [what]
  if [ "$1" = "$2" ]; then ok; else fail "${3:-value}: expected '$1', got '$2'"; fi
}
assert_contains() { # haystack needle [what]
  case "$1" in *"$2"*) ok ;; *) fail "${3:-text} does not contain '$2'" ;; esac
}
assert_not_contains() {
  case "$1" in *"$2"*) fail "${3:-text} must not contain '$2'" ;; *) ok ;; esac
}
# out FILE KEY: value of KEY in a GITHUB_OUTPUT file.
out() { grep "^$2=" "$1" | tail -n 1 | cut -d= -f2-; }

# sh_run SCRIPT [ENV=VALUE ...]: run a script in a clean environment, capture its output.
# Sets $rc, $log (combined stdout+stderr) and $gho (the GITHUB_OUTPUT file).
sh_run() {
  local script="$1"
  shift
  gho="$work/gho.$RANDOM"
  : >"$gho"
  log=$(env -i PATH="$PATH" HOME="$work" TMPDIR="$work" GITHUB_OUTPUT="$gho" "$@" bash "$scripts/$script" 2>&1)
  rc=$?
}

PATH_SHIM="$shim:$PATH"
for tool in curl gh; do
  if [ "$(PATH="$PATH_SHIM" command -v "$tool")" != "$shim/$tool" ]; then
    printf 'test shim is not first on PATH: %s\n' "$tool" >&2
    exit 1
  fi
done

# --- fixtures -----------------------------------------------------------------------------
sum_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# make_release DIR VERSION TARGET EXT: a fake release tree DIR/v<version>/ with the archive
# and SHA256SUMS (the layout of https://github.com/<repo>/releases/download).
make_release() {
  local dir="$1" version="$2" target="$3" ext="$4" name exe stage
  name="knowell-$version-$target"
  exe=know
  [ "$ext" = zip ] && exe=know.exe
  stage="$work/stage.$RANDOM"
  mkdir -p "$stage/$name" "$dir/v$version"
  cp "$shim/know" "$stage/$name/$exe"
  if [ "$ext" = zip ]; then
    local py=""
    for cand in python3 python; do
      if "$cand" -c 'import zipfile' >/dev/null 2>&1; then py="$cand"; break; fi
    done
    if [ -z "$py" ]; then
      zip_skipped=true
      return 0
    fi
    (cd "$stage" && "$py" -c "import sys,zipfile,os
z=zipfile.ZipFile(sys.argv[1],'w')
z.write(sys.argv[2], sys.argv[3])
z.close()" "$dir/v$version/$name.zip" "$name/$exe" "$name/$exe")
  else
    tar -czf "$dir/v$version/$name.tar.gz" -C "$stage" "$name"
  fi
  printf '%s  %s\n' "$(sum_of "$dir/v$version/$name.$ext")" "$name.$ext" >"$dir/v$version/SHA256SUMS"
  rm -rf "$stage"
}

# --- resolve-version ----------------------------------------------------------------------
begin "curl shim: unexpected network requests are refused"
log=$("$shim/curl" https://unexpected.example.test 2>&1)
assert_eq 1 "$?" rc
assert_contains "$log" "refuses an unexpected network request" message
log=$("$shim/curl" https://github.com/knowell-dev/knowell/releases/latest 2>&1)
assert_eq 1 "$?" "rc without the expected redirect option"

begin "resolve-version: pins"
sh_run resolve-version.sh KNOWELL_VERSION=1.2.3
assert_eq 0 "$rc" rc
assert_eq 1.2.3 "$(out "$gho" version)" version
assert_eq v1.2.3 "$(out "$gho" tag)" tag
sh_run resolve-version.sh KNOWELL_VERSION=v2.0.0-rc.1
assert_eq 2.0.0-rc.1 "$(out "$gho" version)" "prerelease version"
for bad in 1.2 '1.2.3;id' 'v1.2.3 ' '../x'; do
  sh_run resolve-version.sh "KNOWELL_VERSION=$bad"
  assert_eq 1 "$rc" "rc for '$bad'"
done

begin "resolve-version: latest follows the redirect"
sh_run resolve-version.sh KNOWELL_VERSION=latest PATH="$PATH_SHIM" FAKE_REDIRECT=https://github.com/knowell-dev/knowell/releases/tag/v0.9.1
assert_eq 0 "$rc" rc
assert_eq 0.9.1 "$(out "$gho" version)" version
sh_run resolve-version.sh KNOWELL_VERSION= PATH="$PATH_SHIM" FAKE_REDIRECT=https://github.com/knowell-dev/knowell/releases/tag/v0.9.1
assert_eq 0 "$rc" "rc with an empty version"
assert_eq 0.9.1 "$(out "$gho" version)" "empty version resolves latest"
sh_run resolve-version.sh KNOWELL_VERSION=latest PATH="$PATH_SHIM" FAKE_REDIRECT=
assert_eq 1 "$rc" "rc without a release"
assert_contains "$log" "no redirect" "message"
sh_run resolve-version.sh KNOWELL_VERSION=latest PATH="$PATH_SHIM" FAKE_REDIRECT=https://example.test/releases/tag/not-a-version
assert_eq 1 "$rc" "rc for a non-semver tag"

# --- asset-name ---------------------------------------------------------------------------
begin "asset-name: OS/arch mapping"
check_asset() { # os arch target archive exe
  sh_run asset-name.sh KNOWELL_VERSION=1.2.3 RUNNER_OS="$1" RUNNER_ARCH="$2"
  assert_eq 0 "$rc" "rc $1/$2"
  assert_eq "$3" "$(out "$gho" target)" "target $1/$2"
  assert_eq "$4" "$(out "$gho" archive)" "archive $1/$2"
  assert_eq "$5" "$(out "$gho" exe)" "exe $1/$2"
}
check_asset Linux X64 x86_64-unknown-linux-gnu knowell-1.2.3-x86_64-unknown-linux-gnu.tar.gz know
check_asset Linux ARM64 aarch64-unknown-linux-gnu knowell-1.2.3-aarch64-unknown-linux-gnu.tar.gz know
check_asset macOS X64 x86_64-apple-darwin knowell-1.2.3-x86_64-apple-darwin.tar.gz know
check_asset macOS ARM64 aarch64-apple-darwin knowell-1.2.3-aarch64-apple-darwin.tar.gz know
check_asset Windows X64 x86_64-pc-windows-msvc knowell-1.2.3-x86_64-pc-windows-msvc.zip know.exe
check_asset Windows ARM64 aarch64-pc-windows-msvc knowell-1.2.3-aarch64-pc-windows-msvc.zip know.exe
sh_run asset-name.sh KNOWELL_VERSION=1.2.3 RUNNER_OS=Linux RUNNER_ARCH=ARM
assert_eq 1 "$rc" "unsupported arch"
sh_run asset-name.sh KNOWELL_VERSION=1.2.3 RUNNER_OS=Plan9 RUNNER_ARCH=X64
assert_eq 1 "$rc" "unsupported os"

# --- install-know -------------------------------------------------------------------------
host_ext=tar.gz
host_target=x86_64-unknown-linux-gnu
host_exe=know
install_env() { # releasedir dl bin [extra...]
  local rel="$1" dl="$2" bin="$3"
  shift 3
  sh_run install-know.sh KNOWELL_VERSION=1.2.3 KNOWELL_ARCHIVE="knowell-1.2.3-$host_target.$host_ext" \
    KNOWELL_NAME="knowell-1.2.3-$host_target" KNOWELL_EXE="$host_exe" KNOWELL_DL_DIR="$dl" \
    KNOWELL_INSTALL_DIR="$bin" KNOWELL_DOWNLOAD_URL="file://$rel" "$@"
}
to_url_path() { if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1" | sed 's#^\([A-Za-z]\):#/\1:#'; else printf '%s' "$1"; fi; }

begin "install: download, verify, extract"
rel="$work/rel1"
make_release "$rel" 1.2.3 "$host_target" tar.gz
install_env "$(to_url_path "$rel")" "$work/dl1" "$work/bin1" KNOWELL_ATTESTATION=skip
assert_eq 0 "$rc" "rc ($log)"
assert_eq false "$(out "$gho" cache-hit-verified)" "cache-hit"
[ -x "$work/bin1/know" ] && ok || fail "binary missing"
[ -f "$work/dl1/knowell-1.2.3-$host_target.tar.gz" ] && ok || fail "archive not kept for the cache"

begin "install: a verified cached archive is reused"
rm -rf "$work/bin1"
install_env "$(to_url_path "$rel")" "$work/dl1" "$work/bin1" KNOWELL_ATTESTATION=skip
assert_eq 0 "$rc" rc
assert_eq true "$(out "$gho" cache-hit-verified)" "cache-hit"

begin "install: a poisoned cached archive is discarded and replaced"
printf 'poison' >"$work/dl1/knowell-1.2.3-$host_target.tar.gz"
rm -rf "$work/bin1"
install_env "$(to_url_path "$rel")" "$work/dl1" "$work/bin1" KNOWELL_ATTESTATION=skip
assert_eq 0 "$rc" rc
assert_eq false "$(out "$gho" cache-hit-verified)" "cache-hit"
assert_contains "$log" "does not match" "warning"
[ -x "$work/bin1/know" ] && ok || fail "binary missing after recovery"

begin "install: checksum mismatch installs nothing"
rel2="$work/rel2"
make_release "$rel2" 1.2.3 "$host_target" tar.gz
printf '%s  %s\n' "$(printf 'x' | sha256sum 2>/dev/null | cut -d' ' -f1)" "knowell-1.2.3-$host_target.tar.gz" >"$rel2/v1.2.3/SHA256SUMS"
install_env "$(to_url_path "$rel2")" "$work/dl2" "$work/bin2" KNOWELL_ATTESTATION=skip
assert_eq 1 "$rc" rc
assert_contains "$log" "checksum mismatch" "message"
[ ! -e "$work/bin2/know" ] && ok || fail "binary installed despite mismatch"
[ ! -e "$work/dl2/knowell-1.2.3-$host_target.tar.gz" ] && ok || fail "bad archive left in the cache dir"

begin "install: SHA256SUMS without the archive, or malformed"
echo "0000  other-file.tar.gz" >"$rel2/v1.2.3/SHA256SUMS"
install_env "$(to_url_path "$rel2")" "$work/dl2" "$work/bin2" KNOWELL_ATTESTATION=skip
assert_eq 1 "$rc" "rc missing entry"
assert_contains "$log" "no entry" "message"
echo "nothex  knowell-1.2.3-$host_target.tar.gz" >"$rel2/v1.2.3/SHA256SUMS"
install_env "$(to_url_path "$rel2")" "$work/dl2" "$work/bin2" KNOWELL_ATTESTATION=skip
assert_eq 1 "$rc" "rc malformed digest"

begin "install: archive name cannot escape the cache dir"
install_env "$(to_url_path "$rel")" "$work/dl3" "$work/bin3" KNOWELL_ARCHIVE="../evil.tar.gz"
assert_eq 1 "$rc" rc

begin "install: zip archive (Windows layout)"
rel3="$work/rel3"
make_release "$rel3" 1.2.3 x86_64-pc-windows-msvc zip
if [ "$zip_skipped" = true ]; then
  echo "SKIP  zip install test: no python with zipfile on this machine"
else
sh_run install-know.sh KNOWELL_VERSION=1.2.3 KNOWELL_ARCHIVE=knowell-1.2.3-x86_64-pc-windows-msvc.zip \
  KNOWELL_NAME=knowell-1.2.3-x86_64-pc-windows-msvc KNOWELL_EXE=know.exe KNOWELL_DL_DIR="$work/dl4" \
  KNOWELL_INSTALL_DIR="$work/bin4" KNOWELL_DOWNLOAD_URL="file://$(to_url_path "$rel3")" KNOWELL_ATTESTATION=skip
assert_eq 0 "$rc" "rc ($log)"
[ -f "$work/bin4/know.exe" ] && ok || fail "know.exe missing"
fi

begin "install: attestation modes"
install_env "$(to_url_path "$rel")" "$work/dl5" "$work/bin5" KNOWELL_ATTESTATION=require
assert_eq 1 "$rc" "require without gh"
install_env "$(to_url_path "$rel")" "$work/dl5" "$work/bin5" KNOWELL_ATTESTATION=require PATH="$PATH_SHIM" FAKE_GH_LOG="$work/gh.log"
assert_eq 0 "$rc" "require with gh ($log)"
assert_contains "$(cat "$work/gh.log")" "attestation verify" "gh call"
assert_contains "$(cat "$work/gh.log")" "--repo knowell-dev/knowell" "gh repo pin"
install_env "$(to_url_path "$rel")" "$work/dl6" "$work/bin6" KNOWELL_ATTESTATION=auto PATH="$PATH_SHIM" FAKE_GH_ATTEST_RC=1
assert_eq 1 "$rc" "failing attestation must stop the install"
[ ! -e "$work/bin6/know" ] && ok || fail "binary installed despite failed attestation"
install_env "$(to_url_path "$rel")" "$work/dl7" "$work/bin7" KNOWELL_ATTESTATION=auto PATH="$PATH_SHIM" FAKE_GH_AUTH_RC=1
assert_eq 0 "$rc" "auto without gh auth continues"

# --- prepare ------------------------------------------------------------------------------
sha_a=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
zeros=0000000000000000000000000000000000000000
ws="$work/ws"
mkdir -p "$ws"
prep() { sh_run prepare.sh GITHUB_WORKSPACE="$ws" RUNNER_TEMP="$work/tmp" "$@"; }

begin "prepare: defaults"
prep EVENT_NAME=push REPOSITORY=o/r
assert_eq 0 "$rc" "rc ($log)"
assert_eq check "$(out "$gho" command)" command
assert_eq error "$(out "$gho" fail-on)" fail-on
assert_eq true "$(out "$gho" upload-sarif)" upload-sarif
assert_eq "$work/tmp/knowell.sarif" "$(out "$gho" sarif-file)" "default sarif path"
assert_eq "" "$(out "$gho" diff-base)" "diff base for a push without before"

begin "prepare: pull request diff base and fork detection"
prep EVENT_NAME=pull_request REPOSITORY=o/r PR_HEAD_REPO=o/r PR_BASE_SHA=$sha_a
assert_eq false "$(out "$gho" fork)" fork
assert_eq $sha_a "$(out "$gho" diff-base)" "diff base"
assert_eq true "$(out "$gho" upload-sarif)" "upload on same-repo PR"
prep EVENT_NAME=pull_request REPOSITORY=o/r PR_HEAD_REPO=evil/r PR_BASE_SHA=$sha_a
assert_eq true "$(out "$gho" fork)" fork
assert_eq false "$(out "$gho" upload-sarif)" "no upload on fork PR"
assert_contains "$log" "fork" "notice"
prep EVENT_NAME=push REPOSITORY=o/r PUSH_BEFORE_SHA=$zeros
assert_eq "" "$(out "$gho" diff-base)" "all-zero before"
prep EVENT_NAME=push REPOSITORY=o/r PUSH_BEFORE_SHA='abc; rm -rf /'
assert_eq "" "$(out "$gho" diff-base)" "garbage before"

begin "prepare: pull_request_target is refused"
prep EVENT_NAME=pull_request_target REPOSITORY=o/r
assert_eq 1 "$rc" rc
assert_contains "$log" "pull_request_target" "message"

begin "prepare: impact/index need OIDC and a hub"
prep EVENT_NAME=pull_request REPOSITORY=o/r PR_HEAD_REPO=o/r IN_COMMAND=impact
assert_eq 1 "$rc" "impact without hub"
prep EVENT_NAME=pull_request REPOSITORY=o/r PR_HEAD_REPO=o/r IN_COMMAND=impact IN_HUB_URL=https://hub.example.test
assert_eq 0 "$rc" rc
assert_eq false "$(out "$gho" run)" "run without OIDC"
prep EVENT_NAME=pull_request REPOSITORY=o/r PR_HEAD_REPO=o/r IN_COMMAND=impact IN_HUB_URL=https://hub.example.test ACTIONS_ID_TOKEN_REQUEST_URL=https://token-request.example.test/x?a=b
assert_eq true "$(out "$gho" run)" "run with OIDC"
prep EVENT_NAME=pull_request REPOSITORY=o/r PR_HEAD_REPO=fork/r IN_COMMAND=impact IN_HUB_URL=https://hub.example.test ACTIONS_ID_TOKEN_REQUEST_URL=https://token-request.example.test/x?a=b
assert_eq false "$(out "$gho" run)" "fork PR never runs impact"
assert_contains "$log" "fork" "notice"
prep EVENT_NAME=push REPOSITORY=o/r IN_COMMAND=index IN_HUB_URL=https://hub.example.test ACTIONS_ID_TOKEN_REQUEST_URL=https://token-request.example.test/x?a=b
assert_eq true "$(out "$gho" run)" "index on push"
assert_eq "" "$(out "$gho" oidc-audience)" "audience is defaulted later, by run-know"

begin "prepare: input validation"
for kv in IN_COMMAND=deploy IN_FAIL_ON=maybe IN_SARIF=yes IN_VERSION=1.2 'IN_VERSION=1.2.3;id' IN_CONFIG=/etc/passwd IN_CONFIG=../x.toml 'IN_CONFIG=a b' IN_CONFIG=-rf \
  IN_WORKDIR=../up IN_ATTESTATION=never IN_SARIF_CATEGORY='a b' 'IN_HUB_URL=http://hub.example.test' 'IN_HUB_URL=https://hub.example.test/"x' IN_COMMENT=1; do
  prep EVENT_NAME=push REPOSITORY=o/r IN_COMMAND=check "$kv"
  assert_eq 1 "$rc" "rejects $kv"
done
prep EVENT_NAME=push REPOSITORY=o/r IN_COMMAND=check IN_CONFIG=sub/knowell.toml IN_WORKDIR=packages/api IN_FAIL_ON=never IN_SARIF=FALSE
assert_eq 0 "$rc" "accepts valid values"
assert_eq false "$(out "$gho" sarif)" "bool normalised"
assert_eq false "$(out "$gho" upload-sarif)" "sarif disabled"

begin "prepare: SARIF path handling"
prep EVENT_NAME=push REPOSITORY=o/r IN_SARIF_FILE=out/results.sarif
assert_eq "$ws/out/results.sarif" "$(out "$gho" sarif-file)" "relative path lands in the workspace"
prep EVENT_NAME=push REPOSITORY=o/r IN_SARIF_FILE="$ws/x.sarif"
assert_eq 0 "$rc" "absolute path inside the workspace"
prep EVENT_NAME=push REPOSITORY=o/r IN_SARIF_FILE="$work/tmp/x.sarif"
assert_eq 0 "$rc" "absolute path inside runner temp"
prep EVENT_NAME=push REPOSITORY=o/r IN_SARIF_FILE=/etc/x.sarif
assert_eq 1 "$rc" "absolute path outside"
prep EVENT_NAME=push REPOSITORY=o/r IN_SARIF_FILE=../x.sarif
assert_eq 1 "$rc" "traversal"
# shellcheck disable=SC2016 # the $( ) is a literal hostile value
prep EVENT_NAME=push REPOSITORY=o/r 'IN_SARIF_FILE=a$(id).sarif'
assert_eq 1 "$rc" "shell metacharacters"
prep EVENT_NAME=push REPOSITORY=o/r IN_SARIF_FILE=-x.sarif
assert_eq 1 "$rc" "option-looking path"

# --- run-know -----------------------------------------------------------------------------
repo="$work/repo"
mkdir -p "$repo"
git -C "$repo" init -q . 2>/dev/null
git -C "$repo" -c user.name=t -c user.email=t@example.test commit -q --allow-empty -m init
head_sha=$(git -C "$repo" rev-parse HEAD)
: >"$repo/knowell.toml"
know_bin="$shim/know"
rk() { sh_run run-know.sh PATH="$PATH_SHIM" KNOWELL_BIN="$know_bin" GITHUB_WORKSPACE="$repo" RUNNER_TEMP="$work/tmp" FAKE_KNOW_LOG="$work/know.log" "$@"; }

begin "run-know: check writes SARIF and counts findings"
: >"$work/know.log"
rk KNOWELL_COMMAND=check KNOWELL_SARIF_FILE="$work/tmp/r.sarif" KNOWELL_FAIL_ON=warning KNOWELL_DIFF_BASE="$head_sha"
assert_eq 0 "$rc" "rc ($log)"
assert_eq 0 "$(out "$gho" exit-code)" exit-code
assert_eq 2 "$(out "$gho" findings)" findings
assert_eq true "$(out "$gho" sarif-exists)" sarif-exists
args=$(cat "$work/know.log")
assert_contains "$args" "--workspace knowell.toml check --format sarif --output $work/tmp/r.sarif --fail-on warning --diff-base $head_sha" "know arguments"

begin "run-know: a missing diff base falls back to a full run"
: >"$work/know.log"
rk KNOWELL_COMMAND=check KNOWELL_SARIF_FILE="$work/tmp/r.sarif" KNOWELL_DIFF_BASE=$sha_a
assert_eq 0 "$rc" rc
assert_not_contains "$(cat "$work/know.log")" "--diff-base" "know arguments"
assert_contains "$log" "not in the checkout" "warning"

begin "run-know: know's exit code is reported, not raised"
rk KNOWELL_COMMAND=check KNOWELL_SARIF_FILE="$work/tmp/r.sarif" FAKE_KNOW_RC=1
assert_eq 0 "$rc" "script rc"
assert_eq 1 "$(out "$gho" exit-code)" exit-code
rk KNOWELL_COMMAND=check KNOWELL_SARIF_FILE="$work/tmp/r.sarif" FAKE_KNOW_RC=3
assert_eq 3 "$(out "$gho" exit-code)" "operational error code"

begin "run-know: working directory"
mkdir -p "$repo/sub"
: >"$repo/sub/knowell.toml"
rk KNOWELL_COMMAND=check KNOWELL_SARIF_FILE="$work/tmp/r.sarif" KNOWELL_WORKDIR=sub KNOWELL_CONFIG=knowell.toml
assert_eq 0 "$rc" rc
rk KNOWELL_COMMAND=check KNOWELL_SARIF_FILE="$work/tmp/r.sarif" KNOWELL_WORKDIR=nope
assert_eq 1 "$rc" "missing directory"

jwt="eyFAKE.KNOWELL_CANARY_JWT.sig"
begin "run-know: impact requests an OIDC token and never logs it"
: >"$work/know.log"
rk KNOWELL_COMMAND=impact KNOWELL_HUB_URL=https://hub.example.test KNOWELL_REPORT_FILE="$work/tmp/rep.md" \
  ACTIONS_ID_TOKEN_REQUEST_URL="https://token-request.example.test/?api=1" ACTIONS_ID_TOKEN_REQUEST_TOKEN=fake-bearer \
  FAKE_OIDC_JWT="$jwt" FAKE_CURL_LOG="$work/curl.log"
assert_eq 0 "$rc" "rc ($log)"
assert_eq true "$(out "$gho" report-exists)" report-exists
args=$(cat "$work/know.log")
assert_contains "$args" "impact --hub https://hub.example.test --oidc-audience https://hub.example.test --format markdown --output $work/tmp/rep.md" "know arguments"
assert_contains "$args" "oidc-token-present" "token handed over through the environment"
assert_not_contains "$args" "$jwt" "know argument log"
assert_contains "$log" "::add-mask::$jwt" "mask command"
assert_not_contains "$(printf '%s' "$log" | sed "s/::add-mask::$jwt//")" "KNOWELL_CANARY_JWT" "log outside the mask line"
assert_contains "$(cat "$work/curl.log")" "audience=https%3A%2F%2Fhub.example.test" "audience encoded"

begin "run-know: impact without a token URL fails cleanly"
rk KNOWELL_COMMAND=impact KNOWELL_HUB_URL=https://hub.example.test
assert_eq 1 "$rc" rc

begin "run-know: index and doc-drift"
: >"$work/know.log"
rk KNOWELL_COMMAND=index KNOWELL_HUB_URL=https://hub.example.test KNOWELL_OIDC_AUDIENCE=aud1 KNOWELL_COMMIT=$sha_a KNOWELL_REF=refs/heads/main \
  ACTIONS_ID_TOKEN_REQUEST_URL="https://token-request.example.test/?api=1" ACTIONS_ID_TOKEN_REQUEST_TOKEN=fake-bearer FAKE_OIDC_JWT="$jwt"
assert_eq 0 "$rc" "index rc ($log)"
assert_contains "$(cat "$work/know.log")" "index --hub https://hub.example.test --oidc-audience aud1 --commit $sha_a --ref refs/heads/main" "index arguments"
: >"$work/know.log"
rk KNOWELL_COMMAND=doc-drift KNOWELL_REPORT_FILE="$work/tmp/dd.md"
assert_eq true "$(out "$gho" report-exists)" "doc-drift report"
assert_contains "$(cat "$work/know.log")" "doc-drift --format markdown --output $work/tmp/dd.md" "doc-drift arguments"

# --- post-comment -------------------------------------------------------------------------
printf '## Impact\n\n- service-a affects service-b\n' >"$work/report.md"
pc() { sh_run post-comment.sh PATH="$PATH_SHIM" FAKE_GH_LOG="$work/gh2.log" FAKE_GH_BODY="$work/body.md" GH_TOKEN=fake KNOWELL_REPOSITORY=o/r KNOWELL_REPORT_FILE="$work/report.md" "$@"; }

begin "post-comment: creates one marker-delimited comment"
: >"$work/gh2.log"
pc KNOWELL_MODE=pr-comment KNOWELL_PR_NUMBER=7
assert_eq 0 "$rc" "rc ($log)"
assert_contains "$(cat "$work/gh2.log")" "--method POST repos/o/r/issues/7/comments" "create call"
assert_eq "<!-- knowell-impact -->" "$(head -n 1 "$work/body.md" | tr -d '\r')" "marker is the first line"

begin "post-comment: updates the existing comment in place"
: >"$work/gh2.log"
pc KNOWELL_MODE=pr-comment KNOWELL_PR_NUMBER=7 FAKE_GH_EXISTING=4242
assert_contains "$(cat "$work/gh2.log")" "--method PATCH repos/o/r/issues/comments/4242" "update call"
assert_not_contains "$(cat "$work/gh2.log")" "--method POST" "no second comment"

begin "post-comment: not a PR, bad input, truncation"
: >"$work/gh2.log"
pc KNOWELL_MODE=pr-comment
assert_eq 0 "$rc" "no PR number is a no-op"
assert_not_contains "$(cat "$work/gh2.log")" "--method" "no API call"
pc KNOWELL_MODE=pr-comment KNOWELL_PR_NUMBER='7;id'
assert_not_contains "$(cat "$work/gh2.log")" "--method" "bad PR number makes no call"
pc KNOWELL_MODE=pr-comment KNOWELL_PR_NUMBER=7 'KNOWELL_REPOSITORY=o/r;id'
assert_eq 1 "$rc" "bad repository"
head -c 100000 /dev/zero | tr '\0' 'a' >"$work/big.md"
pc KNOWELL_MODE=pr-comment KNOWELL_PR_NUMBER=7 KNOWELL_REPORT_FILE="$work/big.md"
size=$(wc -c <"$work/body.md" | tr -d ' ')
[ "$size" -lt 65000 ] && ok || fail "body not truncated ($size bytes)"
assert_contains "$(cat "$work/body.md")" "truncated" "truncation note"

begin "post-comment: issue mode"
: >"$work/gh2.log"
pc KNOWELL_MODE=issue
assert_contains "$(cat "$work/gh2.log")" "--method POST repos/o/r/issues" "issue create"
assert_eq "<!-- knowell-doc-drift -->" "$(head -n 1 "$work/body.md" | tr -d '\r')" "marker"
: >"$work/gh2.log"
pc KNOWELL_MODE=issue FAKE_GH_EXISTING=12
assert_contains "$(cat "$work/gh2.log")" "--method PATCH repos/o/r/issues/12" "issue update"

# --- end to end: scripts chained like action.yml does ----------------------------------------
begin "end to end: version -> asset -> install -> run"
e2e_rel="$work/e2e"
make_release "$e2e_rel" 1.2.3 "$host_target" tar.gz
sh_run resolve-version.sh KNOWELL_VERSION=v1.2.3
v=$(out "$gho" version)
sh_run asset-name.sh KNOWELL_VERSION="$v" RUNNER_OS=Linux RUNNER_ARCH=X64
archive=$(out "$gho" archive)
name=$(out "$gho" name)
exe=$(out "$gho" exe)
sh_run install-know.sh KNOWELL_VERSION="$v" KNOWELL_ARCHIVE="$archive" KNOWELL_NAME="$name" KNOWELL_EXE="$exe" \
  KNOWELL_DL_DIR="$work/e2e-dl" KNOWELL_INSTALL_DIR="$work/e2e-bin" KNOWELL_DOWNLOAD_URL="file://$(to_url_path "$e2e_rel")" KNOWELL_ATTESTATION=skip
assert_eq 0 "$rc" "install ($log)"
bin=$(out "$gho" bin-path)
sh_run run-know.sh PATH="$PATH_SHIM" KNOWELL_BIN="$bin" KNOWELL_COMMAND=check GITHUB_WORKSPACE="$repo" KNOWELL_SARIF_FILE="$work/tmp/e2e.sarif" FAKE_KNOW_LOG="$work/know.log"
assert_eq 2 "$(out "$gho" findings)" "findings from the installed binary"

printf '\n%d passed, %d failed\n' "$passed" "$failed"
[ "$failed" -eq 0 ]
