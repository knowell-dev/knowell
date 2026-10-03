# Releasing Knowell

There are **no releases before 1.0**. Use publication-free dry runs of
`.github/workflows/release.yml` to verify the release pipeline before the owner's
release decision.

## What a release publishes

| Channel | How |
|---|---|
| GitHub Release | Archives for Linux (gnu; musl is experimental), macOS, Windows on x64 and arm64, `.deb` and `.rpm` (Linux gnu), `SHA256SUMS`, a CycloneDX SBOM, pgvector bundles for PostgreSQL 17 and 18 (Windows on ARM excluded: no PostgreSQL binaries exist). Build provenance attestations cover every file. Created as a draft, published last. |
| crates.io | `dist/publish_crates.py`, dependency order, rate-limit aware, safe to re-run |
| GHCR | `ghcr.io/knowell-dev/knowell`, multi-arch, built from the release binaries, attested |
| npm | `knowell` (launcher, with provenance) |
| Homebrew / Scoop | Manifests rendered from `dist/templates` into `knowell-dev/homebrew-tap` and `knowell-dev/scoop-bucket` |
| winget | Pull request to `microsoft/winget-pkgs` (`wingetcreate`, pinned and checksummed) |
| PyPI | Disabled (decision pending); `wrappers/pypi` is a scaffold |

Stable releases only for Homebrew, Scoop, winget and the `latest` container/npm tags;
pre-release versions (`1.0.0-rc.1`) go to GitHub (marked pre-release), crates.io, GHCR
(version tag) and npm (`next`).

## Environments and secrets (names only)

Environment `release` (owner approval required; deployment branches/tags limited to `v*`):

| Name | Kind | Used by |
|---|---|---|
| `CARGO_REGISTRY_TOKEN` | secret | `crates` (first publish; remove after switching to trusted publishing) |
| `NPM_TOKEN` | secret | `npm` |
| `RELEASE_APP_CLIENT_ID` | variable | `tap-bucket` (GitHub App on `knowell-dev`, contents:write on the tap and bucket repos only) |
| `RELEASE_APP_PRIVATE_KEY` | secret | `tap-bucket` |
| `WINGET_PAT` | secret | `winget` (fork + pull-request scope only) |

GHCR uses the job's `GITHUB_TOKEN`; attestations use OIDC. No other job sees a secret, and
pull-request workflows see none.

## Dry run (do this before every release, and whenever the pipeline changes)

1. Actions, Release, Run workflow, keep `dry_run` checked (the default).
2. It attempts eight binary targets; six are required, while the two experimental
   musl targets may fail or be absent. It builds `.deb`/`.rpm`, SBOM and ten pgvector bundles:
   PostgreSQL 17 and 18 each use Linux x64/arm64, macOS x64/arm64 and Windows x64.
   It runs the tooling tests, checks an amd64 container, writes `SHA256SUMS`, installs
   the Linux archive with `scripts/install.sh`, and renders Homebrew, Scoop and winget
   manifests. The crate check runs `cargo publish --workspace --dry-run --locked` only
   when publishable crates exist; the current `publish = false` workspace instead emits
   a warning and skips packaging. The npm check uses `npm pack --dry-run` to inspect its
   file list. Nothing is published, and no release-environment approval is requested.
3. Download the `release-assets` and `rendered-manifests` artifacts and inspect them.

A green dry run currently does not establish `.crate` or npm tarball contents, an arm64
container artifact, or provenance verification. Build attestations are disabled in dry
runs, and the Linux installer check explicitly skips attestation verification. Those
checks, plus Windows installer verification, remain release-readiness work.

Locally: `python -m unittest discover -s dist`, `cd wrappers/npm && npm test`,
`PYTHONPATH=wrappers/pypi/src python -m unittest discover -s wrappers/pypi/tests`.

## Release checklist (1.0 and later)

1. Roadmap's 1.0 criteria are met; `CHANGELOG.md` is final; `SECURITY.md` is current.
2. Remove `publish = false` from the workspace and publish metadata (see the orchestrator notes
   in the pipeline hand-off: every crate needs `description`, `readme`, version requirements).
3. Set the workspace version (and internal `version = "x.y.z"` requirements) via a PR; merge.
4. Run the dry run on `main`; fix anything it reports.
5. Confirm the `release` environment secrets above exist and the App is installed.
6. Owner creates the tag: `git tag v<version> && git push origin v<version>` (tags `v*` are
   owner-only; the tag must equal the workspace version and be on `main`).
7. Approve the `release` environment jobs when asked (one approval can cover several jobs).
8. The release stays a draft until crates.io and GHCR succeed, then it is published, then npm,
   Homebrew, Scoop and winget run. If a job fails, fix and re-run failed jobs: every step is
   idempotent (already published crates are skipped, the draft is updated in place).
9. Verify: `gh attestation verify <asset> --repo knowell-dev/knowell`, install via
   `scripts/install.sh` and `scripts/install.ps1`, `npx -y knowell --version`, pull the image.
10. After the first crates.io publish, set up trusted publishing for each crate and switch the
    `crates` job (the commented block in `release.yml`), then delete `CARGO_REGISTRY_TOKEN`.

Not yet automated (planned): the `knowell-action` tag and the GitHub Pages docs.
