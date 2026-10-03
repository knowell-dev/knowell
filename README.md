<p align="center">
  <img src="docs/knowell-icon.png" alt="Knowell" width="120">
</p>

<h1 align="center">Knowell</h1>

<p align="center">
  <strong>Evidence-backed code context and shared memory for AI coding agents, across all your repositories.</strong>
</p>

<p align="center">
  <a href="https://github.com/knowell-dev/knowell/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/knowell-dev/knowell/actions/workflows/ci.yml/badge.svg"></a>
  <a href="#license"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue"></a>
</p>

> The first stable release is **1.0**. Until it is tagged, install from source.

---

Coding agents such as Codex, Claude Code and Cursor are excellent inside one file and lost
inside a real system. The behaviour you ask about is named differently in the code. A
feature crosses ten repositories. The agent reads a stale branch. A search hit comes without
its callers, contracts or tests. And whatever the last session decided is gone.

**Knowell** is one engine, written in Rust, that indexes a whole workspace of projects and
gives agents what they need for a task: the current, relevant code — with a source for every
statement.

## What Knowell does

- **Finds code by meaning, words and structure.** Hybrid retrieval fuses exact symbol and
  path matches, a code-aware BM25 index (Tantivy) and semantic vectors (PostgreSQL +
  pgvector), then expands along the code graph to callers, types, tests and contracts.
  It complements `grep`; it doesn't replace it.
- **Connects projects through their contracts.** HTTP endpoints, events and topics, RPCs,
  database tables, environment variable names and i18n keys become graph nodes that link
  producers to consumers across repositories. Every edge says how it is known — compiler
  or SCIP resolved, contract-derived, syntactic, heuristic, model-suggested or observed at
  runtime — and whether it is resolved.
- **Stays current by itself.** Each project follows the ref you choose (`branch:development`,
  `remote:origin/release/2.x`, a tag, a commit or a worktree's `HEAD`). Commits, saves,
  webhooks and periodic reconciliation update only what changed; a rebase or force-push
  reuses unchanged work. A missing ref is reported — never silently replaced by another
  branch.
- **Keeps your work-in-progress private.** Uncommitted changes in a worktree form a personal
  overlay on top of the shared view, visible only to you and your agents.
- **Remembers.** Decisions, rules and task progress live in a scoped, versioned memory
  (organisation, workspace, project, task, user) with sources and a review flow: agent
  findings are proposals until a human accepts them; records go stale when their evidence
  changes. A fresh agent session — even on another machine — resumes a task with what
  changed since the last checkpoint.
- **Checks pull requests.** `know check` reports contract drift, migration/entity
  mismatches, missing i18n keys, orphan endpoints and events, and architecture rule
  violations — deterministically, with no API key, as SARIF annotations in GitHub.
- **Explains itself.** Every result carries project, view, commit, path, line range, content
  hash, why it matched and how fresh the index is. An empty answer says why it is empty.

## Quick start

```sh
# Install (pick one)
brew install knowell-dev/tap/knowell          # macOS, Linux
winget install Knowell.Knowell                # Windows
cargo install knowell                         # from source
curl -fsSL https://raw.githubusercontent.com/knowell-dev/knowell/main/scripts/install.sh | sh

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
Provider API keys live only on the hub.

The full design is in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## For agents: MCP tools

| Tool | Purpose |
|---|---|
| `open_workspace` | Start a session: project map, rules, freshness, open tasks, recent decisions |
| `search` | Code, docs, memory and contracts by meaning and by words |
| `fetch` | The exact, versioned source behind any result id |
| `inspect_symbol` | Definition, signature, docs, references, implementations |
| `trace_flow` | How a request or event flows across services, with evidence |
| `analyze_impact` | What a symbol, file, diff or unapplied patch affects, and which tests to run |
| `contracts` | Endpoints, topics, RPCs, tables, env names: producers, consumers, drift |
| `build_context` | A task-sized, token-budgeted context pack with citations |
| `history` | Why code looks the way it does: commits, ADRs, co-changing files |
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
| `know search`, `know trace`, `know impact`, `know context` | Query from the terminal |
| `know check` | Deterministic contract and rule checks (SARIF for CI) |
| `know task`, `know memory` | Tasks, checkpoints and team knowledge |
| `know status`, `know profile` | Index freshness; embedding profiles and blue-green switches |
| `know connect codex\|claude\|cursor`, `know ci init` | Integrations |
| `know login`, `know backup`, `know restore`, `know doctor`, `know eval` | Operations |
| `know token create\|list\|revoke` | Local database administration of scoped hub credentials; values go only to new private files |

## Embeddings and models

- **Gemini Embedding 2** is the primary cloud provider, with profiles of 768, 1536 (default)
  or 3072 dimensions; **Ollama** and any **OpenAI-compatible** endpoint run fully local.
- Profiles are versioned; switching runs blue-green — the old index serves until the new one
  is complete — and vectors from different profiles are never compared.
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

  `know ci init github|gitlab|gitea` generates this and the impact/index workflows (GitHub
  OIDC to the hub, no stored tokens).

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
