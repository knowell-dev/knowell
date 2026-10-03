# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project will adhere to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once 1.0
is released. Nothing has been released yet.

## [Unreleased]

### Fixed

- Add an explicit synthetic-only Gemini evaluation command and an opt-in,
  main-only nightly matrix for 768/1536/3072 dimensions with shared token budgets
  and reproducible measurement conditions. Live validation awaits owner setup.
- Refuse degraded hybrid evaluations and expose the real engine through
  `know eval run --retriever hybrid`, using isolated scratch databases and
  deterministic local embeddings for the CI relevance baseline.
- Resolve hub MCP identities against current tenant grants and preserve token scopes
  across the HTTP-to-engine boundary, including reuse of cached contexts.
- Verify login against authenticated hub health, rejecting malformed responses,
  redirects and remote plaintext HTTP.

- Keep concurrent registrations of the same embedding profile idempotent across
  both its name and settings uniqueness constraints.
- Allow core storage and indexing without pgvector, report semantic search as disabled,
  and enable vectors on a later initialization while preserving migration history.
- Install managed pgvector directly from the release bundle's `lib/` and
  `share/extension/` layout, retaining flat bundles and rejecting linked members.
- Let redirected `know init` output close on Windows while managed PostgreSQL keeps
  running, by preventing the server launcher from inheriting the CLI's pipe handles.
- Require Wasmtime and WASI 49.0.2, addressing upstream security advisories
  RUSTSEC-2026-0321 through RUSTSEC-2026-0327 in the plugin runtime.
- Resolve SARIF findings to explicit project roots, including monorepo subdirectories,
  and encode reserved characters in source paths.
- Keep worktree identities stable across Windows short and long path spellings.
- Test invalid UTF-8 path handling on Unix without requiring filesystem support for
  invalid filenames; retain the Linux filesystem regression test.
- Resolve test binaries and fixture paths at runtime when CI relocates a Nextest archive.
- Fix CLI help markup and ambiguous links that failed Rustdoc with warnings denied.
- Preserve executable Action test shims and reject unexpected network requests in the
  offline harness; make the OIDC prerequisite check explicit for shellcheck.

### Added

- Local `know token create|list|revoke` administration with a configured pepper reference,
  explicit user bootstrap, private credential files and transactional audit records.
- Initial Rust workspace, contribution rules, and project documentation.
- Continuous integration: formatting, linting, dependency policy, secret scanning,
  workflow security checks, tests, and a small synthetic evaluation.

### Changed

- Give the panel a softer, neutral look: rounded cards and pill buttons, outline-free status
  badges, sentence-case labels and text colours that meet WCAG AA contrast in both themes.
  The panel now follows the operating system's light or dark preference until a theme is
  picked.
- Show backtick-marked commands in panel text and server messages as code, keep the
  Projects list readable beside its detail, add the missing space in monthly spend, shorten
  the user id in the top bar, and keep top-bar controls and card grids within narrow
  windows.
- Draw code-graph edges as curves between columns, keep them from showing through node
  boxes, fit the canvas to its card and show full node labels on hover.
