# Knowell Check

A composite GitHub Action that runs [Knowell](https://github.com/knowell-dev/knowell) in your CI:

- **`check`** on pull requests: deterministic checks (contract drift, migration/entity mismatch,
  missing i18n keys, rule violations, orphan endpoints and events). Results are SARIF, so they
  show up inline on the pull request through code scanning. **Needs no secret** and is safe on
  fork pull requests.
- **`impact`** on pull requests: a cross-project impact comment, produced by your team's Knowell
  hub and authenticated with GitHub OIDC (no stored token). Skipped on forks.
- **`index`** on pushes to a tracked branch: tells the hub to update its index.
- **`doc-drift`** on a schedule: a documentation drift report, kept in one issue.

The action downloads a prebuilt `know` release binary (nothing is compiled), verifies its
SHA-256 against the release's `SHA256SUMS`, verifies the build-provenance attestation when `gh`
is authenticated, caches the archive, and analyses only the pull request diff where possible.

> Developed inside the Knowell monorepo under `action/`; it is mirrored to
> `knowell-dev/knowell-action` on release. Marketplace name: **Knowell Check**.

## Quick start: pull request check

```yaml
name: Knowell
on:
  pull_request:

permissions:
  contents: read

jobs:
  check:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      security-events: write # upload SARIF to code scanning
    steps:
      - uses: actions/checkout@<full-sha> # vX.Y.Z
        with:
          fetch-depth: 0 # lets Knowell diff against the PR base; omit to analyse everything
          persist-credentials: false
      - uses: knowell-dev/knowell-action@<full-sha> # vX.Y.Z
        with:
          command: check
          version: 1.0.0 # pin a release
```

Use the `pull_request` event. `pull_request_target` is **refused**: it runs with repository
secrets next to untrusted pull request content.

On a fork pull request the token is read-only, so the action still runs `know check` and writes
the SARIF file, but does not upload it (and fails the job per `fail-on`). Findings are in the log.

## Impact comments

```yaml
jobs:
  impact:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      id-token: write # request a GitHub OIDC token for the hub
      pull-requests: write # post/update the comment
    steps:
      - uses: actions/checkout@<full-sha> # vX.Y.Z
        with: { fetch-depth: 0, persist-credentials: false }
      - uses: knowell-dev/knowell-action@<full-sha> # vX.Y.Z
        with:
          command: impact
          version: 1.0.0
          hub-url: https://knowell.example.com
          oidc-audience: https://knowell.example.com # default: hub-url
```

The comment is delimited by a hidden marker (`<!-- knowell-impact -->`) and **updated in place**,
so a pull request has at most one. Only bot-authored comments are ever updated. Reports longer
than 60 000 bytes are truncated with a note.

On fork pull requests, or when the job lacks `id-token: write`, the step is skipped with a
notice (no OIDC token, no comment, no failure).

## Index update on push

```yaml
on:
  push:
    branches: [main] # your tracked branch
jobs:
  index:
    runs-on: ubuntu-latest
    permissions: { contents: read, id-token: write }
    steps:
      - uses: actions/checkout@<full-sha> # vX.Y.Z
        with: { persist-credentials: false }
      - uses: knowell-dev/knowell-action@<full-sha> # vX.Y.Z
        with:
          command: index
          version: 1.0.0
          hub-url: https://knowell.example.com
```

## Doc drift (weekly)

```yaml
on:
  schedule: [{ cron: "17 6 * * 1" }]
jobs:
  drift:
    runs-on: ubuntu-latest
    permissions: { contents: read, issues: write }
    steps:
      - uses: actions/checkout@<full-sha> # vX.Y.Z
        with: { persist-credentials: false }
      - uses: knowell-dev/knowell-action@<full-sha> # vX.Y.Z
        with: { command: doc-drift, version: 1.0.0 }
```

One issue (marker `<!-- knowell-doc-drift -->`, created by the bot) is opened and then updated.
Set `comment: false` to only produce the report file.

## Monorepo and multi-repo

- **Monorepo**: one workspace config at the root (`knowell.toml`), one workflow. Use
  `working-directory` and `workspace-config` if the config lives in a subdirectory.
  Each project's `path` and optional `root` identify its source directory. SARIF records
  that directory as an absolute file URI, so annotations resolve to the checkout's files
  even when the command runs in a subdirectory. Files in other repositories remain external
  to the uploaded checkout.
- **Multi-repo with a hub**: every repository runs `check` (local, no secret) and `index`
  (pushes to its hub); `impact` asks the hub which other projects a change touches. The hub
  trusts the repositories through GitHub OIDC claims (repository, ref, workflow), so there is
  no token to rotate or leak.

## Inputs

| Input | Default | Meaning |
|---|---|---|
| `command` | `check` | `check`, `impact`, `index` or `doc-drift` |
| `version` | `latest` | `latest`, `X.Y.Z` or `vX.Y.Z`. **Pin it** in production workflows. |
| `workspace-config` | `knowell.toml` | Workspace config, relative to `working-directory` |
| `working-directory` | `.` | Directory `know` runs in, relative to the checkout |
| `sarif` | `true` | `check`: write SARIF and upload it to code scanning |
| `sarif-file` | `<runner temp>/knowell.sarif` | `check`: SARIF path (inside the workspace or runner temp) |
| `sarif-category` | `knowell` | Code scanning category |
| `fail-on` | `error` | `check`: `error`, `warning` or `never` |
| `hub-url` | | `impact`/`index`: `https://` URL of the hub |
| `oidc-audience` | `hub-url` | `impact`/`index`: audience of the OIDC token |
| `comment` | `true` | `impact`: PR comment; `doc-drift`: issue |
| `attestation` | `auto` | `auto` (verify when `gh` is authenticated), `require`, `skip` |
| `github-token` | `${{ github.token }}` | Comments, issues and the attestation check only. Never sent to the hub. |

## Outputs

| Output | Meaning |
|---|---|
| `sarif-file` | Path of the SARIF file (`check`) |
| `findings` | Number of results in the SARIF file (`check`); `0` otherwise |
| `exit-code` | Exit code of `know`; empty when the command was skipped |

The job fails when `know` exits non-zero, after the SARIF upload and the comment have run.

## Permissions

| Flow | `permissions` |
|---|---|
| `check` + SARIF | `contents: read`, `security-events: write` |
| `check`, `sarif: false` | `contents: read` |
| `impact` | `contents: read`, `id-token: write`, `pull-requests: write` |
| `index` | `contents: read`, `id-token: write` |
| `doc-drift` | `contents: read`, `issues: write` |

Code scanning is free for public repositories; private repositories need GitHub Code Security.
If the upload fails, the action warns and the check result is unaffected.

## Security model

- **No secrets are needed for `check`.** It reads the checkout and writes a file. Fork pull
  requests run it with a read-only token.
- **Supply chain.** The binary comes from `github.com/knowell-dev/knowell/releases/download/v<version>/`,
  asset `knowell-<version>-<target>.tar.gz` (`.zip` on Windows). Its SHA-256 is checked against the
  release `SHA256SUMS` on every run, **including when the archive comes from the cache** (a
  poisoned cache entry is discarded). Where `gh` is authenticated, `gh attestation verify`
  checks the build provenance against `knowell-dev/knowell`; `attestation: require` makes that
  mandatory. Every third-party action in `action.yml` is pinned by full commit SHA.
- **Script injection.** Inputs are validated (paths, enums, URLs) and reach scripts only
  through `env:`; no `${{ }}` expression appears inside a `run:` script. Output values are single-line.
- **OIDC.** The token is requested only for `impact`/`index`, only when the runner offers one
  and the pull request is not from a fork, is masked in the log, and is passed to `know` through
  the `KNOWELL_OIDC_TOKEN` environment variable (never on the command line).
- **`github-token`** is used for comments/issues and the attestation check, nothing else.
- **Not supported:** `pull_request_target`.

## Assumed `know` CLI contract

The CLI is being built; this action assumes the following. The CLI owner should match it or
tell us what to change in `scripts/run-know.sh`.

```text
know --workspace <file> check     --format sarif --output <file> --fail-on <error|warning|never> [--diff-base <sha>]
know --workspace <file> impact    --hub <url> --oidc-audience <aud> --format markdown --output <file> [--diff-base <sha>]
know --workspace <file> index     --hub <url> --oidc-audience <aud> [--commit <sha>] [--ref <ref>]
know --workspace <file> doc-drift --format markdown --output <file>
```

- `--workspace` (the `knowell.toml` workspace file) is a global option accepted before the subcommand; `--config` is the engine config and is not passed by the action.
- Exit codes: `0` = ok, `1` = findings at or above `--fail-on` (or a failed gate), `>= 2` =
  operational error. The action reports both through `exit-code`.
- `impact` and `index` read the hub credential from `KNOWELL_OIDC_TOKEN` (a GitHub OIDC JWT
  for the given audience). `impact` writes the comment body (Markdown, no marker) to `--output`.
- `check` always writes valid SARIF 2.1.0 to `--output` when it exits 0 or 1; an empty
  `results` array means no findings.
- `--diff-base` limits analysis to files changed since that commit. The action only passes it
  when the commit exists in the checkout.

## Troubleshooting

- **"base commit ... is not in the checkout"**: use `fetch-depth: 0` (or enough depth) in
  `actions/checkout`; the action then falls back to a full analysis.
- **"SARIF upload failed"**: grant `security-events: write`, and check that code scanning is
  available for the repository.
- **"checksum mismatch" / "attestation verification failed"**: nothing was installed. Check
  the version exists for your runner; report it if it persists, it may be a tampered download.
- **"no OIDC token available: skipping"**: add `permissions: id-token: write`, and note forks
  never get one.
- **"no entry in SHA256SUMS"**: the release has no asset for your platform (supported: Linux,
  macOS, Windows on x64 and arm64).
- **"pull_request_target is not supported"**: switch the trigger to `pull_request`.

## Development

Scripts live in `action/scripts/*.sh` (one concern each, configured only by environment
variables). The offline harness and fakes are in `action/test/`:

```sh
bash action/test/run-tests.sh   # no network, no secrets
shellcheck -x -P SCRIPTDIR action/scripts/*.sh action/test/*.sh action/test/bin/*
```

CI (`.github/workflows/action-test.yml`) runs the harness on Linux, macOS and Windows, and runs
`action.yml` itself against a fake release served from disk. The scripts also read
`KNOWELL_DOWNLOAD_URL` and `KNOWELL_RELEASES_URL` (test overrides of the release location).
The shims must retain their executable bits in Git. The harness checks this before running,
and its curl shim refuses unexpected network requests instead of contacting real services.
