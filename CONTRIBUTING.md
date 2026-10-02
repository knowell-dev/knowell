# Contributing to Knowell

Thanks for your interest. Knowell is pre-1.0 and moving quickly; please open an issue
before starting a large change so we can agree on the approach.

All project text (code, comments, docs, commit messages, UI strings) is in English.

## Ground rules

- **No secrets.** Never commit, paste, or test against real API keys, tokens, passwords,
  or `.env` files. Tests use obviously fake values built inside the test.
- **No private code.** Fixtures, benchmarks, and examples are synthetic or come from openly
  licensed public repositories. Do not copy code, names, paths, or strings from private
  codebases into tests, docs, comments, or commit messages.
- Read [`AGENTS.md`](AGENTS.md) (code rules: no `unsafe`, no panics on any input, no
  silent fallbacks) and [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) before writing code.
- A Developer Certificate of Origin sign-off is **not** required. By contributing you agree
  to the dual license described in the README.

## Development setup

1. Install Rust (stable, at least the `rust-version` in `Cargo.toml`) with `rustfmt` and
   `clippy`.
2. Optional tools: `cargo-nextest`, `cargo-deny`.
3. Clone and build:

   ```sh
   cargo build -p knowell
   ```

### Build lock

Parallel Rust builds can exhaust a machine's memory. `scripts/buildlock.py` serializes
build commands:

```sh
python scripts/buildlock.py cargo test -p knowell-config
```

It is **optional for humans** and **required for AI agents** working in this repository.
Do not start two builds at once and do not pass `-j` above 4.

## Checks

Run these before opening a pull request:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked        # or: cargo nextest run --workspace --locked
cargo deny check
```

Prefer crate-scoped runs while iterating, for example `cargo test -p knowell-secrets`.
CI runs the same checks plus gitleaks, workflow linting, and a small synthetic evaluation;
a single required check named `gate` summarizes them.

## Pull requests

1. Fork and branch from `main`; keep the change focused.
2. Add or update tests. Never weaken or delete a test to make it pass.
3. Update docs and `CHANGELOG.md` (under `[Unreleased]`) when behavior changes.
4. Fill in the pull request template.
5. PRs are **squash merged** through a **merge queue**. The squash commit message is the
   PR title and description, so write them as you would a commit message.

New dependencies need a justification in the PR; all dependencies must pass `cargo deny`
(permissive licenses only).

## Reporting bugs and security issues

Use the issue templates for bugs and feature requests. Do **not** report security issues
publicly; see [`SECURITY.md`](SECURITY.md).
