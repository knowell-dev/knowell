# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project will adhere to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once 1.0
is released. Nothing has been released yet.

## [Unreleased]

### Fixed

- Allow stdio clients to probe MCP discovery and then initialize a legacy
  session on the same pipe without retaining the probe's metadata requirement.
  Modern per-request validation and Streamable HTTP behavior remain unchanged.
- Return version-pinned source bodies and complementary code, test and document
  regions by default over MCP, with exact displayed ranges and advancing fetch
  continuations instead of duplicate diagnostic JSON.
- Preserve historical source commits after changes or deletion, reject ambiguous
  retained commit prefixes, and distinguish source acquisition failures from no matches.
- Hydrate only admitted source paths during ordinary search, retain occurrence-specific
  language metadata and declaration ranges, and aggregate relevant capability caveats.
- Carry explicit configuration and workspace routing into agent launchers and select
  Source output consistently for Claude Code, Codex and Cursor.
- Switch estimates no longer claim that a dimension reduction needs no re-embedding:
  vectors are not derived from another profile, so chunks the target does not cover
  are counted. A profile switch no longer reverts when the engine restarts.
- Match the complete embedding input format before starting a profile switch and
  require re-embedding when that format changes, including dimension reductions.
- Build managed PostgreSQL pgvector bundles for every supported platform on both
  PostgreSQL majors, rather than overwriting platforms in the release matrix.
- Extract verified Windows source archives locally and use the installed macOS SDK
  when building pgvector against relocated PostgreSQL binaries.
- Enforce organization grants and token scopes before HTTP/native profile metadata,
  switch estimates and quality reports, and before native usage, integrations, provider
  changes and Hub administration. Validate native usage periods before date arithmetic.
- Filter profile UUID lookups by organization before decoding and reject unsupported
  registration timestamps through checked conversion without a panic or value echo.
- Recheck current grants on stored memory evidence, replacement links and task
  decisions/manifests, preserve persisted workspace identity across equal project names,
  and filter task ownership before limits with precise database pagination.
- Retain source gaps and truthful saved-task freshness; report malformed saved pointers
  and refuse incomplete checkpoint retry receipts without changing stored evidence or
  history. History rationale checks evidence namespaces before associating files.
- Bind new memory evidence to its originating workspace in persistent and in-memory
  repositories, omit unknown origins and preserve canonical project ids during reviews.
- Apply project scope and exclusions before Git rename similarity reads source blobs
  in indexing, overlays, impact analysis, task resumption and CLI diff checks.
- Filter saved Git status before hashing; capture bounded repository control rules
  separately and preserve native line-ending and ignored-parent semantics.
- Preserve missing-index and missing-ref gaps in graph tools, propagate operational
  diff failures and bound trace nodes across all starting points. Omitting the test
  suggestion list no longer removes test evidence from impact risk.
- Report stale or catching-up source indexes in saved memory evidence without
  replacing the record's original commit, content hash or historical context.
- Reject missing Git targets in fresh engine queries without serving an old active
  index; observe advanced refs for honest freshness, preserve existing context pins
  and narrow source access and project-scoped memory before filtered searches.
- Recover expired indexing leases within an explicit view scope without changing
  foreign or unscoped jobs.
- Reconcile changed content policies and unavailable policy manifests before reusing
  unchanged files, including builds queued before the manifest was lost.
- Emit absolute source file URIs in SARIF so GitHub places primary and related
  locations under the checkout correctly, including monorepo project subdirectories.
- Count Gemini batch entries separately in the request limiter, reject batches
  larger than the configured quota capacity, reserve request and token rates together,
  and bound live evaluation token rates.
- Verify real Knowell SARIF upload and GitHub processing with a main-only manual
  workflow that checks expected rules and committed source locations before upload.
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

- Add source hierarchy and optional redacted line counts in schema 15, retaining
  existing chunk/vector identities and requiring explicit database maintenance to upgrade.
- Add bounded source selection without a generative model, explicit comparator
  policies and opt-in persisted parse products; agent benefit and large-corpus scale
  remain unmeasured.
- Explicit signed software updates for direct installer-owned installations, with
  immutable engine versions, a stable launcher, verified preparation, activation,
  rollback and interruption recovery. Package-managed installations retain their
  original update method; no background updater or forced MCP restart is installed.
- TUF repository verification and an offline threshold-signing publisher, with
  persistent trust history, release revocations and reviewed compatibility contracts.
  Production root keys, metadata hosting and renewal remain release prerequisites.
- Explicit database maintenance with persistent operation ownership, runtime admission
  gates, exact schema-history validation and protected fresh managed backups. Ordinary
  startup never migrates an existing schema; restored maintenance backups require
  explicit recovery before runtime admission reopens.
- Restart-safe blue-green embedding profile switches (migration 0013). Switches and each
  view's serving profile are stored; T2 builds the old and the target profile while a
  switch builds, so semantic search never has a gap; all member views flip in one
  transaction once covered; open contexts keep their profile; a restarted engine resumes
  building switches; rollbacks within the retention reuse the old vectors without
  provider calls. A changed configured profile starts a switch when views are indexed.
  New REST: `GET /api/v1/profiles/switches`, `POST .../switches/{id}/cancel` and
  `.../rollback`.
- Durable MCP tool usage (migration 0012): engines buffer calls per UTC hour, tool and
  agent, add them to the store every few seconds and at shutdown, and report usage and
  integration status from the store, so counters survive restarts. Reports now apply
  the requested period to tools too; latency percentiles come from a histogram.
- Durable checkpoint idempotency: decisions, the task update, the checkpoint and its
  receipt are stored in one transaction (migration 0011), so a `save_checkpoint` retry
  with the same caller and key returns the original checkpoint after an engine restart,
  and concurrent retries store a single checkpoint instead of failing or duplicating it.
- Docker test runner (`scripts/docker_test.py`, `deploy/test/`) for the Linux workspace
  and panel suites against throwaway PostgreSQL 17 servers with and without pgvector
  (PostgreSQL 18 on demand), without mounting the host directory.
- `KNOWELL_TEST_STRICT=1` turns a missing test database URL or `git` into a test failure
  instead of a silent skip; the CI Linux test partitions and the Docker runner set it.
- Standalone `know profile list/show` reads tenant-authorized persisted profile metadata
  without a workspace, provider clients, credentials or source/profile registration.
- Standalone `know memory list/show` and `know task list/show` over persisted Engine
  records, with native JSON, sourced text/Markdown and no provider preparation or calls.
- Standalone `know trace` and `know impact` over the real engine graph, with versioned
  evidence, explicit gaps and safe text/JSON/Markdown output.
- Standalone `know index`, `know search` and `know status` commands backed by the real
  engine, with versioned evidence, tier and embedding coverage reports, JSON output
  and explicit incomplete outcomes. `index --rebuild` rebuilds an unchanged target
  after a configured embedding profile change.
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
- Ease panel pages in on navigation, soften hover changes, and replace the loading spinner
  with shimmering placeholder lines that appear only when loading takes longer than 200 ms;
  all motion stops when the operating system asks for reduced motion.
- Fold long graph insight lists into collapsible groups by kind, so the code-graph page no
  longer grows without bound on real repositories, and show their commands as code.
