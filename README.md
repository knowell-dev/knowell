<p align="center">
  <img src="docs/knowell-icon.png" alt="Knowell" width="120">
</p>

<h1 align="center">Knowell</h1>

<p align="center">
  <strong>The hybrid code intelligence and memory engine for AI coding agents.</strong>
</p>

<p align="center">
  <a href="https://github.com/knowell-dev/knowell/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/knowell-dev/knowell/actions/workflows/ci.yml/badge.svg"></a>
  <a href="#license"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue"></a>
</p>

> The first stable release is **1.0**. Until it is tagged, install from source.

---

**Knowell** is an open-source Rust engine that indexes multi-repository workspaces and
serves source-cited context through MCP to Codex, Claude Code, Cursor and other coding
agents. It combines exact symbol and path matches, BM25 text search, optional semantic
vectors and evidence-carrying code relationships, then packs relevant results within a
requested context budget. Versioned decisions and task checkpoints support continuity
across sessions, while freshness and coverage gaps remain explicit.

A feature can cross an API, an event consumer and a shared package. Its business name may
never appear in the code. Knowell helps an agent find the implementation, follow indexed
relationships across projects and retrieve the decisions saved by earlier sessions — with
the source version behind each result.

## What Knowell does

- **Finds code by meaning, words and structure.** Hybrid retrieval fuses exact symbol and
  path matches, a code-aware BM25 index (Tantivy) and semantic vectors (PostgreSQL +
  pgvector), then expands along available code-graph relationships. Analysis coverage and
  unresolved relationships are reported with the results.
  It complements `grep`; it doesn't replace it.
- **Connects projects through their contracts.** HTTP endpoints, events and topics, RPCs,
  database tables, environment variable names and i18n keys become graph nodes that link
  producers to consumers across repositories where extraction is supported. Edges carry
  their evidence type and resolution status; coverage depends on the configured analysis
  and rule packs.
- **Tracks changes and reports freshness.** Each project follows the ref you choose
  (`branch:development`,
  `remote:origin/release/2.x`, a tag, a commit or a worktree's `HEAD`). Commits, saves,
  webhooks and periodic reconciliation update only what changed; a rebase or force-push
  reuses unchanged work. A missing ref is reported — never silently replaced by another
  branch. The latest seen commit and the active index commit are reported separately;
  indexing can lag behind source changes.
- **Keeps your work-in-progress private.** Uncommitted changes in a worktree form a personal
  overlay on top of the shared view, visible only to you and your agents.
- **Preserves decisions and task progress.** Decisions, rules and task progress live in a
  scoped, versioned memory
  (organisation, workspace, project, task, user) with sources and a review flow: agent
  findings are proposals until a human accepts them; records go stale when their evidence
  changes. A fresh agent session resumes a saved task with what changed since the last
  checkpoint. Cross-machine continuity uses a shared hub or export/import and covers
  records saved to Knowell; agents must write decisions and checkpoints through its tools.
- **Checks pull requests.** `know check` reports contract drift, migration/entity
  mismatches, missing i18n keys, orphan endpoints and events, and architecture rule
  violations — deterministically, with no API key, as SARIF annotations in GitHub.
- **Explains itself.** Every result carries project, view, commit, path, line range, content
  hash, why it matched and how fresh the index is. An empty answer says why it is empty.

## Current status

Knowell is pre-1.0. Hybrid retrieval, context packing, MCP tools and persisted memory have
implementations; the architecture describes the full 1.0 target. The roadmap's milestone
labels are not a current inventory of implemented features. Release readiness requires the
[1.0 acceptance scenarios](docs/ROADMAP.md#10-acceptance-scenarios) to pass.

Context packs use an estimated token budget; the agent's model context limit still applies.
Session continuity comes from saved records and checkpoints; Knowell does not inherit an
agent's conversation history. Symbol inspection currently reports incomplete reference
coverage, and `history` provides indexed co-changes and saved rationale without git log or
blame. See the [engine's known limits](crates/knowell-engine/README.md#known-limits) for
integration details.

## Quick start

```sh
# Install from a source checkout (requires Rust and Python)
git clone https://github.com/knowell-dev/knowell.git
cd knowell
python scripts/buildlock.py cargo install --path crates/knowell --locked

# Set up a local engine: Knowell runs its own PostgreSQL + pgvector, no Docker needed
know init

# Import a multi-repo workspace (.gitmodules, go.work, pnpm/npm/Cargo workspaces, folders)
know workspace import ~/code/shop

# Start the engine: local panel and MCP endpoint on http://127.0.0.1:7420
know serve

# Connect your agent (writes its MCP config and a short start-up instruction)
know connect codex        # or: claude, cursor
```

Then ask your agent things like *"where do we stop a customer from being charged twice?"*,
*"which services react when a subscription is cancelled?"* or *"what breaks if I remove this
field?"* — in English or Turkish.

## How it works

```
 Workspace ── ref policy · embedding profile · data policy · shared memory
   └─ Project (a repository, or a root inside a monorepo)
        └─ View (local branch · remote branch · worktree HEAD · pinned tag or commit)

 change ─▶ T0 text & paths ─▶ T1 symbols & imports ─▶ T2 embeddings ─▶ T3 relations & contracts
 query  ─▶ plan ─▶ exact + BM25 + vectors ─▶ fusion ─▶ graph expansion ─▶ budgeted, cited context
```

One binary runs in four roles: **standalone** (everything on one machine), **hub** (a shared
team server with users, permissions and audit), **worker** (scalable indexing) and **edge**
(each developer's machine: worktree overlays and a stdio MCP server for local agents).
In team deployments, provider API keys belong on the hub. Standalone commands resolve
their configured provider secret references on the local machine.

The full design is in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## For agents: MCP tools

| Tool | Purpose |
|---|---|
| `open_workspace` | Start a session: project map, rules, freshness, open tasks, recent decisions |
| `search` | Code, docs, memory and contracts by meaning and by words |
| `fetch` | The exact, versioned source behind any result id |
| `inspect_symbol` | Definition, signature, docs and available references/tests; coverage gaps reported |
| `trace_flow` | How a request or event flows across services, with evidence |
| `analyze_impact` | What a symbol, file, diff or unapplied patch affects, and which tests to run |
| `contracts` | Endpoints, topics, RPCs, tables, env names: producers, consumers, drift |
| `build_context` | A task-sized, token-budgeted context pack with citations |
| `history` | Indexed co-changing files and saved rationale citing the source |
| `read_memory` / `write_memory` | Scoped team knowledge; agents propose, people accept |
| `resume_task` / `save_checkpoint` | Continue work across sessions and machines |
| `index_status` | Freshness and coverage per project |

Agents connect over stdio (`know mcp`) or Streamable HTTP (`/mcp` on a hub). Repository text
is always returned as untrusted data.

## Command line

| Command | |
|---|---|
| `know init` | Configure the engine (managed, Compose or external PostgreSQL) |
| `know workspace import\|add\|list`, `know project add` | Define workspaces and projects |
| `know serve [--role standalone\|hub\|worker\|edge]` | Run the engine, panel and MCP endpoint |
| `know mcp` | MCP server over stdio for local agents |
| `know index [--rebuild] [--json]` | Index the selected standalone workspace and report incomplete targets |
| `know search QUERY [--project NAME] [--no-snippets] [--json]` | Search the standalone index with versioned evidence and explicit coverage gaps |
| `know status [--project NAME] [--json]` | Read standalone index freshness, tier states and embedding coverage |
| `know trace SYMBOL [--project NAME] [--json]` | Trace sourced relations in the standalone index; `--id` and `--contract` select other starts |
| `know impact [SYMBOL\|--file PATH\|--base REF] [--project NAME] [--json]` | Inspect local graph impact for a symbol, file or committed diff |
| `know context` | Repository session hook; engine-backed context remains planned |
| `know check` | Deterministic contract and rule checks (SARIF for CI) |
| `know memory list\|show` | Read standalone scoped records, their lifecycle states and saved evidence |
| `know task list\|show` | List readable standalone tasks or resume one with checkpoints, decisions and changes |
| `know profile list\|show` | Read registered embedding profile metadata for the local organization |
| `know connect codex\|claude\|cursor`, `know ci init` | Integrations |
| `know login`, `know backup`, `know restore`, `know doctor`, `know eval` | Operations |
| `know token create\|list\|revoke` | Local database administration of scoped hub credentials; values go only to new private files |
| `know update --status\|--plan\|--prepare\|--apply` | Explicit verified software updates for direct installer-owned installations |
| `know maintain --status\|--operation UUID` | Inspect or explicitly finish database maintenance, for every installation method |

Local `index`, `search`, `trace`, `impact` and `status` require an engine configuration with role
`standalone` and a selected `knowell.toml` (use the global `--config` and `--workspace`
options to choose them). `index` processes only that workspace's registered views,
including their expired leases. It exits with 1 when the observed source target or a
required tier remains incomplete; idle jobs alone do not prove completion. Use
`index --rebuild` after changing an embedding profile to rebuild an unchanged target;
configured providers may charge for that work. Search and status register metadata but
do not refresh or index sources. Search exits with 1 for a missing ref or index, and
reports reduced semantic coverage as a gap. Operational errors exit with 2.

### Updating an installation

Source/Cargo, npm, Homebrew, Scoop, winget, system packages, and container images remain
owned by their original installation method. `know update` reports that ownership and
does not replace their executable. Direct bootstrap installers create a dedicated
software root containing a stable launcher and immutable engine version directories;
user data remains under `KNOWELL_HOME`.

Native updates are explicit. There is no startup/background update network traffic or
MCP update banner. The production TUF trust root and metadata host must be provisioned
before release; development builds do not infer a public root. Provision an independently
trusted public root and repository once, then choose an exact version:

```sh
know update --configure-source --trust-root /trusted/root.json \
  --metadata-url https://updates.example.invalid/metadata/ \
  --targets-url https://releases.example.invalid/download/
know update --plan --version 1.1.0
know update --prepare --version 1.1.0
# Close all engine/MCP sessions, then activate the prepared version.
know update --apply
```

`--prepare` verifies complete raw engine and launcher artifacts while current sessions
keep working. `--apply` refuses active local engines and coordinates a persistent
database maintenance operation with participating remote engines. Remote processes must
be stopped by their operator; Knowell does not terminate active MCP sessions for updates.
External databases additionally require `--session-gates-confirmed`: the operator must
use direct or session-preserving connections and stop clients without runtime admission.
Transaction/statement poolers are unsupported for this protocol.

Schema-changing apply requires `--allow-migration` and a fresh managed `--backup FILE`,
or `--external-backup-confirmed` for an external backup/restore-test attestation. The
verified candidate runs its own embedded migrations under exclusive database admission.
The backup contains indexed code and memory; it is a database dump, not a backup of
PostgreSQL roles, global configuration, or every Knowell home. A schema update applies
only to the selected configuration/home; other homes must be explicitly maintained.
Choose a private backup directory: unsafe parent permissions and existing destinations
are refused, and the new dump is protected before it is published.
Ordinary engine startup validates exact schema history and never migrates it.

Inspect an interruption with `know update --status`. Select recovery explicitly with
`--recover old` or `--recover new`; `--rollback` revalidates the retained previous release
against current trusted metadata and persisted formats. Neither restores a database or
discards later writes. Valid interrupted trust-state generations can be resumed with
`--recover-metadata`; corrupt trust history is refused, never deleted/reset automatically.
Pre-migration backups retain the maintenance UUID. After a deliberate database restore,
use a schema-compatible engine and explicitly recover that UUID before reopening runtime
admission; keep the software journal and backup together.
Local offline repositories require `--offline` and two `file:///` directory URLs; signature,
expiry, and replay checks still apply.

To correct repository URLs, repeat `--configure-source` with the exact original public
bootstrap root. Previously accepted root and metadata versions remain enforced; a new
bootstrap root cannot replace the installation's trust history.

Launcher replacement is independent: invoke the active **raw engine** in its version
directory with `update --launcher` and the source options, after every launcher exits.
A pending launcher repair blocks ordinary launch until the explicit repair completes.
Package-managed users can inspect interrupted database maintenance with `know maintain
--status` and recover its recorded UUID with `know maintain --operation UUID`; migrations
add `--migrate` and the same backup/session preconditions. Maintenance never expires by TTL.

`trace` and `impact` query existing generations without indexing or making provider
requests. Graph reports preserve source commits, hashes, line ranges, freshness,
relation evidence and resolution gaps. A project flag selects or disambiguates the
subject; graph expansion can still reach other authorized projects. Use `--format
markdown` for an escaped human report, `--json` for the native structured result,
or `--output FILE` to write the chosen report atomically. Missing requested subjects,
indexes and refs exit with 1. A valid empty committed diff remains a report with gaps.
Trace limits apply to the total returned nodes, and `impact --no-tests` omits the
suggested test list without changing the risk assessment. Committed impact accepts
full commit IDs or typed refs with `--base` (alias `--diff-base`) and optional `--head`.
Hub/OIDC transport and terminal unapplied-patch input remain unimplemented; remote
transport flags fail explicitly before credentials or database access.

Content-policy changes trigger full reconciliation even when the source commit is
unchanged; missing policy manifests are rebuilt conservatively before reporting success.

`memory list` defaults to 20 accepted/proposed records; `memory show ID` includes all
lifecycle states in the native readable scopes. Task-scoped decisions are available
through `task show ID`. `task list` defaults to 10 open/in-progress/blocked tasks;
`task show ID --limit N` limits the newest checkpoints. Both command groups support
`--json`, `--format markdown` and atomic `--output FILE`. They prepare no provider
clients or credentials, make no provider requests and do not schedule indexing.
Empty lists and source gaps remain informative successes; absent requested IDs exit
with 1, and malformed input or operational failures exit with 2. Memory/task writes,
review, pinning, repository write-back and Hub/OIDC transport remain planned.

`profile list` reads the selected organization's registered profiles; `profile show NAME`
or `profile show --id UUID` reads one. It needs standalone engine configuration but no
workspace or source. Reports contain the stored id, name, provider, model, dimensions,
input-format version and UTC registration timestamp. Text/Markdown, JSON and atomic
`--output FILE` are supported. Opening may migrate and set up organization identity;
it does not register profiles, prepare provider clients, resolve credentials or index
sources. Empty lists exit with 0, absent profiles with 1 and input/operational failures
with 2. Activity, locality and budgets remain open there; profile switches and rollback
are available through the REST API (CLI commands remain planned).

These commands currently use the local database. Hub transport remains planned.
Directory sources can be indexed and inspected, but source search cannot yet return
their hits because the MCP evidence schema requires a Git commit; it reports the gap.
Local provider construction currently uses each embedding library's batch and rate
defaults; CLI rate and spending caps remain planned. Provider quota failures stay
explicit and can leave the index incomplete.

## Embeddings and models

- **Gemini Embedding 2** is the primary cloud provider, with profiles of 768, 1536 (default)
  or 3072 dimensions; **Ollama** and any **OpenAI-compatible** endpoint run fully local.
- Profiles are versioned; switching runs blue-green — the old index serves until the new one
  covers every project, survives restarts and can be rolled back — and vectors from
  different profiles are never compared.
- A per-project **data policy** decides which content may reach which provider. Projects are
  `local-only` unless you opt in.
- Without any embedding provider, exact, lexical, graph and memory search keep working, and
  results say that semantic search is off.

## Teams and CI

- **Hub:** `deploy/compose.yml` runs Knowell with PostgreSQL + pgvector for a team: shared
  indexes and memory, users, roles and API tokens, audit log, webhooks from GitHub, GitLab
  and Gitea.
- **GitHub Action — Knowell Check:**

  ```yaml
  permissions:
    contents: read
    security-events: write
  steps:
    - uses: actions/checkout@v5
    - uses: knowell-dev/knowell-action@v1
      with:
        command: check
  ```

  `know ci init github|gitlab|gitea` generates check and remote impact/index workflow
  templates. The remote templates target GitHub OIDC to the hub without stored tokens;
  they remain pending CLI hub transport, and their remote flags currently fail explicitly.

## Privacy and security

- Sensitive files (`.env*`, keys, credentials, tfstate, kubeconfig, …) are excluded **by path
  before their content is read** — even when tracked by git. Everything else is scanned and
  secrets are redacted before anything is indexed, embedded, summarised, stored or returned.
- Configuration holds secret **references** (`env:NAME`, `file:/path`), never values.
- The panel binds to `127.0.0.1`, with origin, CSRF and DNS-rebinding protection.
- Permissions are enforced inside search, graph expansion, context packing and memory.
- Plugins run in a WebAssembly sandbox with explicit capabilities and resource limits.
- **No telemetry.**
- Content an MCP tool hands to a cloud-hosted agent enters that agent's data flow;
  end-to-end locality needs a local agent too.

Report vulnerabilities privately — see [`SECURITY.md`](SECURITY.md).

## Measured, not claimed

Retrieval quality is measured on a synthetic ten-project fullstack workspace with graded
English and Turkish queries (`know eval run`), and every change is compared against a
committed baseline in CI. Numbers are published with the hardware, data size and cache state
they were measured on. See [`crates/knowell-eval`](crates/knowell-eval/README.md).

## Documentation

- [Architecture](docs/ARCHITECTURE.md) · [Roadmap](docs/ROADMAP.md) ·
  [Releasing](docs/RELEASING.md) · [Deploying a hub](deploy/README.md) ·
  [GitHub Action](action/README.md)

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md). Agents and humans follow [`AGENTS.md`](AGENTS.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([`LICENSE-MIT`](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in this work by you, as defined in the Apache-2.0 license, shall be dual licensed
as above, without any additional terms or conditions.
