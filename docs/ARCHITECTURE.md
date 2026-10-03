# Knowell architecture

This is the design reference for Knowell. Read it before writing code. Sections describe the
**target** design for 1.0; the [roadmap](ROADMAP.md) says what exists. Where a number or
default is not fixed yet, the text says it is measured or configurable rather than guessing.

Contents: [1 Overview](#1-overview) · [2 Principles](#2-principles) ·
[3 Concept model](#3-concept-model) · [4 Roles and components](#4-roles-and-components) ·
[5 Data layer](#5-data-layer) · [6 Source tracking and indexing](#6-source-tracking-and-incremental-indexing) ·
[7 Code analysis](#7-code-analysis) · [8 Contract graph](#8-cross-project-contract-graph) ·
[9 Embeddings](#9-embeddings-and-model-providers) · [10 Search](#10-search-and-context) ·
[11 Memory](#11-memory-knowledge-and-tasks) · [12 MCP, CLI, REST](#12-mcp-cli-and-rest) ·
[13 Panel](#13-panel) · [14 Advanced modules](#14-advanced-modules) ·
[15 Security and privacy](#15-security-and-privacy)

## 1. Overview

Knowell is an open-source, Rust-based engine that helps AI coding agents (and people) find
the right code in large, multi-project codebases, understand how projects relate, and keep
context across sessions and machines.

- It watches **workspaces** of projects and updates itself incrementally after commits and
  saved changes.
- It indexes code by **meaning** (vectors), **words** (BM25), and **relations** (graph).
- It links cross-project **contracts** (endpoints, events, RPCs, tables, env names, i18n
  keys) in an evidence-carrying graph.
- It keeps decisions, rules, and tasks in a **scoped, versioned, sourced memory**.
- Agents use it through **MCP**; people use the local **panel** and the **`know` CLI**.
- The target experience: a brand-new agent session attached to the same workspace continues
  the work without the project being re-explained.

### The problem, as questions

| An agent asks | Capability needed |
|---|---|
| "Where do we prevent the same payment being processed twice?" | Semantic search |
| "Where is `PaymentService.cancelSubscription` and who uses it?" | Exact symbols and references |
| "Which projects does subscription cancellation pass through?" | Cross-project flow |
| "Who produces and who consumes this event?" | Contract graph |
| "What breaks if I remove this API field?" | Impact analysis (including unapplied patches) |
| "Why did we design it this way?" | Sourced knowledge, decisions, history |
| "Which existing implementation should I copy for this?" | Rules and approved examples |
| "Which tests cover this?" | Code-to-test relations |
| "Search including my worktree's changes" | Version and workspace awareness |
| "What did we decide on the task I left on the other machine, and what changed since?" | Task memory |

The main output is: **the current, source-attributed context needed for this task.**

## 2. Principles

1. **The engine supplies evidence; the agent does the reasoning.** Every result carries
   project, view, commit, file, line range, content hash, symbol, relation evidence, and
   index state. A line number alone is not an identity.
2. **No silent fallback.** If a tracked ref is missing, do not switch to another branch. If
   embeddings are not ready, do not present an old vector as current. "No result" is
   distinct from "this behavior does not exist". Missing analysis is stated.
3. **Everything is versioned**: source views, parsers, embedding profiles, knowledge
   records, rules.
4. **Relation evidence is explicit.** Evidence type and resolution status are never
   collapsed into one "confidence" number. Semantic similarity is not a dependency.
5. **Secrets never enter.** `.env` files and credentials are excluded before reading and
   never reach an embedding, summary, memory, log, panel, MCP response, or CI output.
6. **Alongside grep, not instead of it.** Every feature is measured; one that does not
   contribute stays off by default.
7. **No invented quality or speed claims.** Numbers are published with hardware, data size,
   and cache state, from measurement.

## 3. Concept model

```
Organization (tenant / security boundary)
 └─ Workspace                 ref policy · embedding profile · data policy · memory
     ├─ Project (a repo, or a root inside a monorepo; inherits or overrides workspace settings)
     │   └─ Source (git repo or local directory)
     │       └─ View: local branch · remote branch · worktree HEAD · pinned SHA/tag
     ├─ Domain + glossary (query term <-> code name <-> abbreviation)
     ├─ Rules and approved examples
     └─ Task (goal, progress, decisions, open questions, related symbols, view manifest)
```

- **A project is not a repository.** A monorepo can hold several projects; one repository
  can be in several workspaces; one repository can have several views (for example
  `development`, a release tag, an agent's worktree).
- **Ref policy lives on the workspace**; projects inherit or override it (for example the
  workspace tracks `development`, one service tracks `release/2.x`, a shared SDK tracks a
  tag). No branch name is assumed for any project type. The panel shows where each setting
  comes from. A ref that cannot be found is reported, never replaced.
- **Content sharing happens only within one tenant and one index profile.** Different
  organizations never share a cache.
- **View manifest:** at query start, each project's commit plus its local-change generation
  is pinned, so a commit landing mid-search cannot make a result half old and half new. A
  manifest does not by itself prove compatibility; that is judged from contracts, lockfiles,
  and tests. Named **release views** can be stored.
- **Import:** one command builds a workspace from `.gitmodules`, `go.work`, a pnpm/npm/Cargo
  workspace file, or a directory scan.

## 4. Roles and components

### 4.1 One engine, four roles

Panel, CLI, and MCP all talk to the same engine, so rules, permissions, and the data model
live in one place. The same binary runs in different roles:

| Role | Where | Job |
|---|---|---|
| `standalone` | A single developer | Hub, worker, and edge in one process; resolves configured provider secret references locally |
| `hub` | Team or company server | Shared views, graph, memory, users and permissions, panel, Streamable HTTP MCP. Holds provider API keys for team deployments. |
| `worker` | Next to the hub, scalable | Heavy indexing jobs (parsing, embedding, SCIP) |
| `edge` | Each developer machine | Watches local worktrees and saved changes, builds the personal layer, serves stdio MCP to agents, merges with the hub |

- In team deployments, the edge asks the hub for embeddings and holds no provider keys.
- Uncommitted content does not leave the machine by default; sharing it is an explicit
  choice.
- Two independent local installs do not discover each other. For multiple devices use a
  shared hub or export/import.

```
  Developer machine (edge)                          Hub (team server)
 +------------------------------+  HTTPS + token  +-------------------------------------+
 | agent --MCP(stdio)--> edge   | <-------------> | views, graph, memory, tasks         |
 | worktree/WIP layer, cache    |                 | embeddings/summaries (keys here)    |
 +------------------------------+                 | PostgreSQL+pgvector, Tantivy        |
   ^ file watching, git/agent hooks               | panel, RBAC, audit, workers         |
                                                  +-------------------------------------+
                                                    ^ webhooks (GitHub/GitLab/Gitea), polling
```

### 4.2 Crates (draft)

The crate list is a draft and may be consolidated (crates.io applies rate limits to new
crates). Existing crates are in `crates/`.

| Crate | Responsibility |
|---|---|
| `knowell-core` | Domain types, identifiers, errors |
| `knowell-config` | TOML configuration plus JSON Schema (editor completion); validation shared with the panel |
| `knowell-store` | PostgreSQL (sqlx), migrations, repositories, pgvector |
| `knowell-pg-managed` | Managed embedded PostgreSQL lifecycle |
| `knowell-lexical` | Tantivy BM25, code tokenizer |
| `knowell-source` | Repos, refs, worktrees (`gix`), file watching (`notify`), webhook receivers |
| `knowell-index` | Change planner, durable job queue, incremental pipeline, view activation |
| `knowell-parse` | tree-sitter, chunking, symbols, skeletons |
| `knowell-scip` | SCIP import, language-tool integration |
| `knowell-link` | Cross-project contract rule packs |
| `knowell-graph` | Edges, traversal, in-memory `petgraph` cache |
| `knowell-embed` | Providers, profiles, cache, batch, budgets |
| `knowell-judge` | Optional rerank/classification providers (local models, hosted rerank APIs) |
| `knowell-knowledge` | Memory, knowledge, tasks, domains, rules |
| `knowell-query` | Query plan, hybrid search, fusion, graph expansion, context packing |
| `knowell-secrets` | Exclusion, secret scanning, redaction |
| `knowell-auth` | Users, RBAC, tokens, OIDC |
| `knowell-mcp` | MCP server (official `rmcp`) |
| `knowell-server` | axum: REST, MCP over HTTP, panel assets, webhooks |
| `knowell-plugin` | Wasmtime plugin host |
| `knowell-eval` | Evaluation harness and synthetic fixture generator |
| `knowell` | The `know` binary (CLI); `anyhow` is allowed only here |

Other directories (planned): `panel/` (UI), `packs/` (rule packs), `deploy/` (Compose),
`docs/`, fixtures generators. Main technology: tokio, axum, sqlx (offline query checking),
pgvector, tantivy, tree-sitter, gix, notify, reqwest, rmcp, serde + schemars, secrecy +
keyring, tracing (+ OpenTelemetry export), wasmtime, ts-rs/specta (panel types), clap.

### 4.3 Resilience

- **Durable job queue** in PostgreSQL (`SKIP LOCKED`); jobs are idempotent; restart resumes
  where it stopped; retry with a dead-letter queue; jobs are cancellable.
- **Generation fence:** a late-finishing old job cannot activate a newer view.
- **Priority:** actively edited projects and interactive queries go before bulk indexing;
  long indexing never starves search.
- **Resource budgets:** CPU and memory, parallel jobs, provider request/token/currency
  budgets, view and history retention, per-query result and graph-expansion limits, context
  token budget, and a policy for large, generated, and vendored files.

## 5. Data layer

PostgreSQL is the database, pgvector is its vector extension, and `psql` is a terminal
client. Knowell's single supported backend is **PostgreSQL 17/18** with optional
**pgvector 0.8 or newer** for semantic search. Core schema migration succeeds without
pgvector, and lexical search, symbols, graph and memory remain available. Missing
vector storage is reported explicitly; adding the extension files and rerunning
`know init` enables it without discarding data or rewriting migration history.
There is no second storage implementation.

### 5.1 One backend, three provisioning modes

| Mode | For | How |
|---|---|---|
| **Managed** (default, standalone) | One developer, no Docker | `know` installs and manages its own PostgreSQL (`postgresql_embedded`). Data dir `~/.knowell/pg`, loopback/unix socket only, random password in the OS keychain. The release CI builds and attests pgvector for each platform itself. |
| **Docker Compose** | Hub / team | `deploy/compose.yml`: Knowell plus a `pgvector/pgvector` image |
| **External PostgreSQL** | Existing infrastructure | `database.url` (value in keychain or environment); `know doctor` checks version and pgvector |

Major-version upgrade, backup, and restore of the managed instance are handled by `know`
and covered by acceptance tests.

The managed server outlives the CLI that starts it. `pg_ctl start` uses null standard
streams and writes server diagnostics to `postgres.log`. On Windows an explicit handle
list excludes the CLI's redirected pipes, so piping `know init` does not wait for the
server to stop. Launcher timeout or cancellation still terminates the launcher.

Managed pgvector installation accepts the release bundle's `lib/` and
`share/extension/` directories directly, as well as the legacy flat layout.
Incomplete, mixed or linked bundle members are rejected before content is read.

### 5.2 Layers

| Layer | Choice | Notes |
|---|---|---|
| System of record | PostgreSQL | Identifiers, views, edges, memory, tasks, job queue, permissions, audit |
| Vectors | pgvector HNSW + `halfvec`; partitioned by profile/project; iterative scan for filtered search | `vector` HNSW is limited to 2000 dimensions and `halfvec` to 4000, so 3072 dimensions require `halfvec`. Above roughly 10-20M vectors per partition, pgvectorscale (DiskANN) can be used with the same schema. A full-precision copy may be kept for optional rescoring. |
| Lexical | Tantivy BM25 | Code tokenizer: camelCase, snake_case, paths, exact symbols, error codes. Rebuildable from PostgreSQL; the data version it serves is tracked. PostgreSQL `ts_rank` is never presented as BM25. |
| Graph | Edge tables + bounded-depth `WITH RECURSIVE` + in-memory `petgraph` cache | 1-5 hop traversal; no separate graph database |
| Extension point | Other vector stores | Not supported in 1.0; would need a separate adapter and tests |

### 5.3 Main records (draft)

`organization`, `workspace`, `project`, `source`, `view` (ref, commit, generation, state),
`view_manifest`, `file_version` (path to blob hash), `content` (blob), `chunk` (prepared
input hash, range, parent symbol), `symbol` (persistent logical identity) + `occurrence`,
`edge` (kind, evidence type, resolution status, evidence references, valid view),
`contract` (endpoint/topic/rpc/table/env_name/i18n_key/package), `embedding` (profile x
chunk), `embedding_profile`, `index_generation` (building/active/retired),
`knowledge_item`, `evidence`, `task`, `domain`, `glossary_term`, `rule`, `job`, `policy`,
`principal`/`role`/`grant`, `audit_log`, `tool_usage`.

### 5.4 Scale notes

- Raw vector storage is dimensions x 4 bytes (FP32) or x 2 bytes (`halfvec`) per vector,
  before text, HNSW overhead, metadata, and history. The panel shows estimated and actual
  disk use side by side.
- Under-filled results in filtered vector search and a large project drowning out small
  ones in ranking are measured separately and tuned with per-project candidate quotas,
  partitioning, and iterative scan.

## 6. Source tracking and incremental indexing

### 6.1 Change signals

No single signal is the source of truth.

- The commit of the tracked ref (local branch; remote branch after an authoritative fetch).
- Webhooks (GitHub, GitLab, Gitea; HMAC verified): immediate.
- File watching: saved but uncommitted changes.
- Git hooks, agent hooks, CI notifications: accelerators only.
- Periodic reconciliation (Merkle tree): missed events, deletions, anything that happened
  while the engine was down.

### 6.2 Tracking modes

| Mode | Example | Behavior |
|---|---|---|
| Local branch | `refs/heads/development` | Includes unpushed local commits |
| Remote branch | `origin/development` | After an authoritative fetch |
| Worktree HEAD | An agent's worktree | Follows the branch/commit the worktree is on |
| Pinned | SHA or tag | Reproducible view |

The panel shows three values per project separately: **tracking target**, **latest seen
commit**, and **commit of the active index**. Indexing a remote branch never changes the
user's checkout; the engine reads git objects directly.

### 6.3 Incremental pipeline

1. Determine changed files and source versions.
2. Apply exclusion and data-egress policies (before content is read).
3. Parse changed files; extract symbols and relations.
4. Determine affected dependent analyses (references if a signature changed; consumers in
   other projects if a contract changed). Other projects are not re-embedded.
5. Recompute chunks whose embedding input changed.
6. Update records for deleted or moved sources.
7. Prepare lexical, vector, and relation indexes.
8. Activate the new view when ready (behind the generation fence).

**Cache key:** hash of the prepared embedding input (content plus context header) + parser
version + embedding profile, inside the tenant boundary. When history changes but content
does not (rebase, cherry-pick), vectors are reused.

**Scenarios with dedicated tests:** rename, move, delete, merge, rebase, reset,
force-push, branch switch, deletion of the tracked branch.

Committed diffs select regular-file metadata by project root and exclusion policy
before rename similarity can read blobs. Moves across that boundary expose only the
allowed addition or deletion. Exact and edited renames inside the scope retain their
identity; similarity reads obey the configured file size limit. Temporary filtered
trees stay in memory, and diffing uses no Git attributes, clean filters or external
drivers. Indexing, personal overlays, committed impact analysis, task resumption and
CLI diff checks use this same boundary.

Saved-change status also selects approved source paths before hashing. Recognized
same-repository Git ignore and attribute controls are captured separately with per-file
and aggregate byte limits, without following symlinks or reading outside-repository
controls. Their bytes remain transient control data and do not expand the source scope.
Native line-ending conversion operates only on bounded allowed source bytes; unsupported
filters and transforms produce an error rather than silently altering Git semantics.

### 6.4 Freshness tiers

| Tier | Time | Content |
|---|---|---|
| T0 | seconds | File text and path searchable (Tantivy) |
| T1 | seconds | Symbols, imports, skeletons |
| T2 | minutes (queue) | Embeddings |
| T3 | afterwards | Affected relations, cross-project links, knowledge staleness checks |

Every result states which tier it comes from. While a new index is being prepared, the last
ready view keeps serving.

### 6.5 Worktrees and the personal layer

- Worktrees of registered repositories are discovered automatically; each opens as a
  **personal layer**.
- Existing worktree roots have canonical absolute paths, so Windows short-name aliases
  and the checkout used for discovery do not change a worktree's identity. Missing
  worktrees retain their registered paths and are marked prunable.
- A layer is the difference between the tracked view and the worktree's real state (its own
  HEAD plus saved changes). It does not modify the shared index, so changes on
  `feature/payment` never leak into the `development` view.
- Worktrees that share a feature name (for example `.worktree/{slug}/*`, same branch name)
  are grouped into one multi-repository **task view**.
- Unsaved editor buffers need an editor integration (outside 1.0); the scope the file
  watcher actually sees is shown in the panel.

## 7. Code analysis

- **Chunking by meaningful unit:** function, method, class, interface, type, module,
  endpoint, migration, test, documentation section. The embedding input adds path, package,
  enclosing symbol, signature, and doc comment. Large functions are split into sub-chunks
  linked to the parent symbol; at query time the signature, relevant imports, and
  neighboring code are reassembled. The target is roughly 300-1000 tokens, tuned by
  measurement per language and query type.
- **Two analysis levels:** tree-sitter (syntactic, incremental) and SCIP / language tools
  (definitions, references, implementations). The panel shows them separately. Analysis that
  needs a build or external tool is configured as a separate capability; source-only
  operation is supported.
- **Symbol identity is not content identity;** renames and moves are tracked.
- **Generated code** (protobuf output, `DO NOT EDIT`) is linked to its source and does not
  produce duplicate results; vendored code is excluded by default.

### 7.1 Edge evidence types

| Evidence type | Meaning |
|---|---|
| Semantically resolved | Verified by compiler, SCIP, or language tool |
| Contract-derived | From OpenAPI, proto, schema, or package manifest |
| Syntactic observation | Seen in source structure |
| Heuristic match | Name, structure, or pattern similarity |
| Model suggestion | AI-proposed; needs verification |
| Runtime observation | Observed in a specific version and environment (OpenTelemetry) |

Each edge also has a **resolution status**: resolved / ambiguous / unresolved. Dynamic
dispatch, reflection, names generated at runtime, and missing build information stay
visible. Evidence type and status are never merged into a single score.

### 7.2 Language support

Published as a capability matrix.

| Tier | Coverage |
|---|---|
| Precise (tree-sitter + SCIP/language tool) | Rust, TypeScript/JavaScript, Python, Go, Java/Kotlin, C# |
| Structural (tree-sitter + rule packs) | Dart/Flutter, Swift, PHP, Ruby, C/C++, Scala; those with a SCIP indexer move to the precise tier |
| Text + chunking | Everything else |
| Contract / structure files | SQL (schemas, migrations), proto, OpenAPI/AsyncAPI, GraphQL, Markdown/ADR, YAML/TOML/JSON config, Dockerfile/Compose, Kubernetes, Terraform, i18n |

## 8. Cross-project contract graph

Two mechanisms work together: (1) semantic search in one shared vector space across the
workspace; (2) an **evidence-carrying contract graph** in which contracts are first-class
nodes.

| Link | Evidence |
|---|---|
| Client call to HTTP endpoint | fetch/axios/ky/dio/URLSession/Retrofit calls, route definitions, OpenAPI |
| OpenAPI to generated client | Contract plus generated code |
| Service to RPC | proto plus generated stubs |
| Publisher to event/topic to consumer | Event name, schema (proto/AsyncAPI), subscription (RabbitMQ/Kafka/NATS/Redis) |
| Service to table (reads and writes separate) | ORM schema, queries, migrations |
| Application to package | Manifest, lockfile, import |
| Code to env/config **name** (never the value) | Read sites, Compose/Kubernetes definitions |
| Code to i18n key | Usage versus language files |
| Service to infrastructure | Dockerfile/Compose/Kubernetes: which service runs where, on which port |
| Code to test | References, structure, coverage when available |
| Decision to implementation | ADR / knowledge links |

- **Rule packs** are declarative (TOML + tree-sitter queries) with their own test examples,
  supported versions, and known limits. Executable plugins run in a Wasmtime sandbox with
  explicit file and network permissions, resource limits, and a versioned interface.
- **Planned bundled packs:** NestJS, Express, Next.js, Go (net/http, gin, echo, chi),
  FastAPI, Spring, Flutter dio, Swift URLSession, TypeORM, Prisma, GORM, RabbitMQ, Kafka,
  NATS, protobuf/gRPC, OpenAPI.
- **Graph insights:** endpoint with no client; event with no consumer; table never read;
  consumer on an old schema (contract drift); migration versus entity mismatch; missing i18n
  key; parity gaps between clients (web/Flutter/iOS).
- **`know check`** runs these insights and architecture rules deterministically. It needs no
  embeddings or API key, so it is safe as a CI gate (including on fork pull requests).
  SARIF source locations use URI-encoded absolute file URIs from explicit project roots,
  including monorepo sub-roots, without depending on project URI base IDs. Code-scanning
  uploads use the scanned checkout as their
  repository root; a finding in a different repository retains its external location.

## 9. Embeddings and model providers

### 9.1 Gemini Embedding 2 (primary cloud provider)

- Model `gemini-embedding-2`: 8,192-token input; 128-3072 dimensions (recommended 768,
  1536, 3072); multimodal (image, PDF, audio, video).
- **Adapter rules:**
  - There is no `task_type`; the task is expressed with a **prefix**. Query:
    `task: code retrieval | query: ...`. Document: `title: ... | text: ...`.
  - Putting several chunks into one `content` produces a **single combined vector**, so
    **each chunk is its own input** in a request.
  - Model, dimensions, prefix format, and chunker version together form the **embedding
    profile**; if any changes, it is a new profile.
  - **Vectors from different profiles are never compared** in one similarity computation.
    If the provider is unavailable, another model's query vector is never sent to this
    index; lexical, symbol, graph, and memory search keep working and the gap is reported.
  - Bulk initial indexing uses the Batch API.

### 9.2 Profiles

| Profile | Dimensions | Use |
|---|---:|---|
| Compact | 768 | Lower storage and search load |
| **Balanced** | **1536** | **Default starting point** |
| Extended | 3072 | `halfvec` index; chosen based on evaluation |
| Custom | Whatever the model/backend supports | User experiments |

Profile settings: provider/model, dimensions, precision, chunking, candidate count /
`ef_search`, reranking, concurrency / rate limit / batch, budgets (tokens, currency, disk,
retention).

Provider request and estimated-token rate quotas are reserved together immediately before
sending. Gemini conservatively counts each batch input as one request quota unit; the
OpenAI-compatible and Ollama transports count HTTP batches. A Gemini batch must fit its
configured request quota capacity. These local limits do not reserve shared provider quota
or guarantee a billing total.

- The panel shows dimensions and **measured quality separately**; an unmeasured profile
  gets no quality score.
- **Profile switches are blue-green:** the new index is built in the background while the
  old one serves; it activates after quality and coverage checks; it stays reversible for
  the retention period. The panel first shows affected projects, the amount of data to
  regenerate, and a cost estimate. Reducing dimensions needs no API call (truncate and
  normalize); increasing them means re-embedding.
- A workspace uses one common profile by default (required for cross-project similarity);
  projects on different profiles are searched separately and merged in ranking.

### 9.3 Other providers

- **Ollama** (local; the Qwen3-Embedding family is to be evaluated) and any
  **OpenAI-compatible embedding endpoint** (LM Studio, vLLM, TEI, llama.cpp server).
- An embedded ONNX runtime (fastembed) is an **optional build feature**; the default build
  stays light.
- Hosted embedding providers beyond Gemini are optional and are evaluated on our own
  benchmarks before any claim is made about them.
- **Per-project data-egress policy** decides which content may go to which provider (cloud
  allowed / local only). It applies to every embedding, summarization, and reranking call.

### 9.4 Model-written descriptions ("code meanings")

Short descriptions of symbols or modules can be produced by a separate generative model.
They are stored with evidence and version, indexed as separate vectors, and become stale
when the source changes. Whether this is on by default is undecided; the panel shows a cost
estimate before anything runs. A connected agent can also propose descriptions through
`write_memory`.

### 9.5 Optional rerankers

Reranking and query classification are optional and **off by default**. Providers are local
models or hosted rerank APIs, with a pinned version and a timeout. They are enabled only
if they measurably help on our own evaluation. The data-egress policy applies.

## 10. Search and context

Pipeline:

1. Pin the authorized scope and the **view manifest** (workspace, projects, language, path,
   domain).
2. **Query plan:** classify as exact symbol / endpoint / error trace / behavior / impact /
   "why". Uses rules and the glossary (query-language to code-name mapping, project terms;
   automatic suggestions are kept apart from human-approved synonyms).
3. **Exact matches:** symbol table, path, contract, error code.
4. **Lexical (Tantivy) + semantic (pgvector)** candidates; the personal layer shadows its
   base view.
5. **Fusion:** reciprocal rank fusion (RRF); weights set by measurement; per-project
   candidate quotas for fair ranking.
6. **Graph expansion:** callers, types, tests, contracts, documents; bounded by depth,
   edge type, and budget.
7. **Optional reranking** of the short list only.
8. **Context packing:** signatures and skeletons first, deduplication, source citations,
   uncertainties, and what is missing.

**Explainability:** each result says why it came (exact symbol / semantic / "this test
references this function" / graph path). An empty result says why ("project not indexed",
"no reference resolution for this language", "no candidates in the selected ref"). Finding
English code from a non-English question is part of the evaluation set.

## 11. Memory, knowledge, and tasks

Honest limit: code does not become part of a model's mind. Knowell provides the right
starting pack, details on demand, and decisions that are not forgotten. It does not inherit
an agent's internal conversation history; continuity comes from task records, decisions
written through tools, and sourced summaries.

### 11.1 Scopes

| Scope | Stored |
|---|---|
| Organization | Shared terminology, approved engineering rules, general decisions |
| Workspace | Cross-project flows, system architecture, shared contracts |
| Project | Architecture, run/test knowledge, constraints, project decisions |
| Task | Goal, progress, decisions, open questions, related symbols, view manifest |
| User | Personal preferences, private notes, unshared working knowledge |

### 11.2 Record kinds and states

- **Kind:** observed from code, human-written (ADR, decision), model description / agent
  finding.
- **State:** proposed, then accepted or rejected; stale (the evidence code changed);
  superseded.
- Each record has: source, owner/author (human, agent, session), scope, date, related
  projects and symbols, evidence (file + range + view), and a current version.
- Observations extracted from code update automatically. Agent comments are drafts.
  Knowledge that becomes a team rule goes through an **explicit acceptance process** (which
  operations auto-accept is configurable). An agent's remark such as "this endpoint needs no
  auth" does not become a rule by itself.
- When the source changes, dependent records are queued for re-evaluation. Invalid
  decisions are kept historically but not presented as current rules. Conflicts are surfaced,
  never silently merged.
- The panel supports review, correction, pinning, changing sharing, and deletion. Secrets
  cannot enter memory.

### 11.3 Link to the repository

- Accepted knowledge can be written back to the repository **as a pull request** (AGENTS.md,
  CLAUDE.md, KNOWLEDGE.md, ADRs), so it survives even without the engine.
- **Documentation drift:** symbols, files, or endpoints named in those documents but no
  longer present are detected and an update is suggested.

### 11.4 Session bootstrap

1. **`know connect codex|claude|cursor`** writes the MCP configuration, adds a short
   start-up instruction to AGENTS.md / CLAUDE.md, installs a SessionStart hook for Claude
   Code, and a start-up instruction or skill for Codex. Connecting MCP does not guarantee
   that memory is read; the instruction, the hook, and the server `instructions` field are
   used together and tested.
2. The agent's identity and reachable workspace are determined; `open_workspace` returns the
   project map and roles, ref policy, current rules, index freshness and coverage, open
   tasks, a budgeted and sourced summary of recent decisions, and a `context_id`.
3. `resume_task` returns progress, **sources changed since then**, and knowledge that went
   stale.
4. While working, details come on demand; progress goes to `save_checkpoint`, findings to
   `write_memory` in the right scope.

### 11.5 Another machine

- Most robust: both machines connect to the same authoritative **hub**.
- Personal use: a personal hub (for example on your own server), or portable export/import.
- What does not travel: unshared files and work in progress on the other machine, and chat
  details not saved to the engine. Sharing work in progress is an explicit choice.

## 12. MCP, CLI, and REST

### 12.1 MCP tools

| Tool | Function | Type |
|---|---|---|
| `open_workspace` | Start-up pack, `context_id` | read |
| `search` | Search code, docs, memory, contracts | read |
| `fetch` | Versioned source detail from a result ID | read |
| `inspect_symbol` | Definition, signature, doc, references, implementations | read |
| `trace_flow` | Flow over evidenced relations (cross-project) | read |
| `analyze_impact` | Impact of a symbol/file/diff **or an unapplied patch**; risk; tests | read |
| `contracts` | Endpoint/topic/RPC/table/env name; producers/consumers; drift | read |
| `build_context` | Source pack for a task and token budget; why relevant; what is missing | read |
| `history` | Blame, frequently/co-changed files, "why is it like this" history | read |
| `read_memory` | Scoped memory read | read |
| `write_memory` | Proposal/record (by permission) | write |
| `resume_task` | Task list / resume | read |
| `save_checkpoint` | Task progress / decision | write |
| `index_status` | Freshness, coverage, jobs | read |

- **Transport:** stdio (edge/standalone) and **Streamable HTTP** (hub), using the official
  `rmcp` SDK. Supported protocol and client versions are in the test matrix.
- Tools carry an explicit workspace / view / `context_id`; concurrent agents do not change
  each other's selection.
- Read, write, and admin permissions are separate and use MCP tool annotations so client
  approval policies work. Results are structured and sourced, with durable result IDs; long
  operations return a job ID.
- MCP resources (a file in a view) and prompts (`/onboard`, `/impact-review`) are provided.

HTTP middleware supplies a typed authenticated principal and credential scopes to MCP.
The engine's asynchronous `StoreAccess` resolves current grants for its organization on
every call. Context reuse rechecks those grants. User callers become agents; delegated
agents and service accounts keep their verified identity. Read-only tokens cannot write
memory, and a token from another organization cannot authenticate to the hub.

Graph tools retain explicit gaps for authorized projects without an index or with a
missing tracked ref. Missing diff refs produce gaps; corrupt objects and other source
failures remain operational errors. When changed content is skipped by source policy,
impact analysis does not infer removed symbols from an absent text. Trace node limits
apply to the whole returned graph, including all starts, and every returned edge has
both endpoints present.

### 12.2 CLI (`know`), main commands

`init`, `serve [--role standalone|hub|worker|edge]`, `workspace import|add|list`,
`project add|config`, `search`, `trace`, `impact`, `check`, `context`, `task`, `memory`,
`status`, `profile`, `connect codex|claude|cursor`, `ci init github|gitlab|gitea`, `login`
(hub), `token create|list|revoke`, `export|import`, `backup|restore`, `doctor`, `eval`.

Token administration uses the installation's database administrator connection. The
configured `server.token_pepper` is a secret reference resolved by both issuance and the
server. Creation writes a new owner-only credential file and stores only a keyed hash;
list and revoke expose identifiers and metadata. Creation and revocation are audited in
the same transaction. Login verifies authenticated hub health before saving references;
remote connections require HTTPS, and redirects are refused.

Implemented local `index`, `search`, `trace`, `impact` and `status` commands construct the same engine
with the standalone caller and selected workspace. Configuration, project selection
and provider profiles are validated before opening the database. A configured provider
cannot silently become an unconfigured lexical-only profile. Registration issues,
generation state and embedding coverage stay explicit in JSON and terminal output.
Search and status register metadata without scheduling source indexing or starting
watchers. A fresh Git search resolves the exact authorized target before using its
saved generation; existing contexts retain their original pins. Explicit project
filters narrow source probes before unrelated source failures can affect the query.
If a Git target has advanced since indexing, fresh results retain the indexed commit
and report its stale or catching-up state from the observed target without queuing work.

Local graph commands call `trace_flow` and `analyze_impact` on that engine. They report
native evidence and gaps as JSON or escaped text/Markdown, and optionally write a
complete report atomically to an explicit output path. A subject's project flag does
not narrow cross-project graph expansion. Missing subjects/indexes/refs exit with 1;
operational and input failures exit with 2. Empty committed diffs and bounded traces
remain informative reports. Hub/OIDC and terminal unapplied-patch input remain open.

Local `memory list/show` and `task list/show` call `read_memory` and `resume_task` on
the same engine through an explicit record-read opening path. It preserves configured
profiles and data policy without constructing provider clients or resolving their
credentials. Reports retain native records, source/index gaps and historical evidence,
with safe text/Markdown, JSON and optional atomic files. Empty lists and incomplete
source information remain successful reads; absent requested ids exit with 1, while
input and operational errors exit with 2. Review, pinning and repository write-back
commands remain open. Task-scoped decisions are read through task show rather than
implicitly widening memory show's native scope selection.

Persisted evidence retains its exact project/workspace identity at the repository
boundary, including metadata for sources not configured in this process. Mapping those
ids reads database hierarchy metadata without registering or opening the source. The write
context binds new evidence to its workspace, including user and organization records.
In-memory repositories retain that origin across Engine instances; unknown origins are
omitted with a generic gap. Record reviews preserve canonical project ids rather than
resolving names again.
Current grants filter returned evidence, project links, replacement ids and historical task
manifests before resumption; saved records and history are unchanged. Personal history
in an owned task remains private to that owner; a shared task's personal pins require a
matching currently authorized overlay. Missing or malformed saved pointers produce a
generic gap without hidden identifiers. History rationale matches saved file evidence
only after workspace identity and grant checks. Retry receipts without a gap field
fail explicitly when linked data is unavailable. Freeform symbol labels and progress
text remain untrusted scoped record content; they are not parsed as source identities.
Task ownership filtering precedes the limit, and database pagination keeps full
timestamp precision.

`index` refreshes each selected workspace view, then drains and recovers leases only
within its frozen registered view scope. Its completion report checks the observed
commit or directory tree, required tiers and embedding coverage. Missing or superseded
targets, retry delays and failures produce exit code 1; operational errors produce 2.
Changed or unavailable content-policy manifests trigger full reconciliation even on an
unchanged source target. `index --rebuild` explicitly rebuilds an unchanged target when
the configured embedding profile changes. Local providers currently use their library
batch, rate and spending defaults; configurable CLI caps remain open. These local commands
do not yet implement hub transport. Directory source search remains limited by the
commit-required MCP evidence schema and reports that missing evidence instead of
inventing a commit.

### 12.3 REST

A versioned REST API with an SSE progress stream serves the panel and integrations, under
the same permission model.

## 13. Panel

| Screen | Content |
|---|---|
| Overview | Engine health, queue, freshness, errors, resource use |
| Workspaces | Projects, shared settings (ref, profile, data policy), access |
| Projects | Source, root, tracked ref, worktrees, exclusions; where each setting comes from |
| Indexes | Views, generations, tiers, analysis coverage, DLQ, profile switches, reindexing |
| Search playground | Try queries, filters, score breakdown (BM25 / vector / graph / rerank), sources |
| Code graph | Service, module, symbol drill-down; contract map |
| Domains and glossary | Concepts, synonyms, approval status |
| Memory | Decisions, notes, tasks, proposal queue, conflicts, stale records |
| Rules | Architecture rules, approved examples, violations |
| Model profiles | Provider, dimensions, locality policy, budgets, cost |
| Quality | Query sets, profile comparison, bad results |
| Agents and usage | MCP calls, tokens returned, latency, spend |
| Integrations | MCP connection, start-up flow, connection diagnostics |
| Administration (hub) | Users, roles, tokens, audit log |

Index detail shows the source chunk, the prepared text sent for embedding, the profile, and
linked relations (subject to the data policy).

**Technology:** axum serving a panel embedded in the binary; Svelte 5 + TypeScript, with
API types generated from Rust (ts-rs/specta). Graph (sigma.js / WebGL) and code viewer
(CodeMirror) are in the JS ecosystem.

**Security:** binds `127.0.0.1` by default; session authentication; Origin, CSRF, and DNS
rebinding protection; stored keys are never shown again. Hub: TLS, RBAC, audit.

## 14. Advanced modules

Opt-in modules planned for 1.0. Each is tested; experimental ones are behind flags.

| Capability | Result |
|---|---|
| Concept / domain map | Links business terms to code, docs, and projects |
| Behavior-focused change explanation | Structural impact of a commit/PR and changed contracts; exact structural results kept apart from model commentary |
| Patch preview | A proposed change in a temporary analysis view; impact before applying |
| Architecture rules + approved examples | Allowed dependencies, violations; the right examples for "add a new endpoint" (owned, scoped) |
| Historical rationale | Decisions, ADRs, commits, PRs tied to code; current / changed / cancelled |
| Multi-repo change groups | PRs that must move together; compatible version combinations |
| Failure source finding | Stack trace or build error to release identity to the symbol, change, and tests at the right commit |
| Test impact | Relevant tests, missing evidence |
| Dependency knowledge | Types/docs matching the lockfile version (budgeted) |
| Agent activity log | Which agent works on which task/area; shared-dependency warnings (no prevention guarantee) |
| CI index bundles | Safe reuse of attested indexes for the matching source/profile version |
| Runtime evidence | Calls observed in OpenTelemetry traces (selected, sanitized fields only; an unobserved path is not "absent") |
| Screenshot to code | Screenshot to UI component and sources (multimodal); inexact links are suggestions. **Experimental, off by default** |
| Plugin system | Language, framework, source, and provider adapters (Wasmtime) |
| Ownership map | Blame + CODEOWNERS: "who knows this area" |
| Infrastructure graph | Compose/Kubernetes/Dockerfile to service to env name to port |

## 15. Security and privacy

- `.env*`, credential files, private keys, certificates, tfstate, and kubeconfig are
  excluded **without reading content**; being tracked by git does not lift the rule. Allowed
  content also passes secret scanning (patterns plus entropy).
- Secret values do not reach embeddings, summaries, memory, logs, error reports, or
  diagnostic/support bundles.
- Provider keys come from the OS keychain, a secret store, or runtime values.
  Configuration holds a **reference** (`env:NAME`, `file:/path`), never the value. Code uses
  `SecretString`; logging has a redaction layer; error messages, connection URLs, and HTTP
  traces must not leak. Errors about rejected secret input never echo the input.
- Besides source code, vectors, summaries, caches, and task records are private data.
- Permissions are enforced in the storage and search layers (search, graph expansion,
  context packing, memory); permission changes affect caches and later access.
- Repository text is **data**: instructions inside code comments or docs do not become
  rules by themselves. Content returned to an agent is labeled untrusted data, and
  instruction-like text is flagged.
- Per-project provider policy (embedding, summarization, reranking). Fully local models are
  supported, but **content given to a cloud-based agent over MCP enters that agent's data
  flow**; end-to-end locality requires a local agent too.
- Deletion propagates to derived summaries, vectors, caches, and exported bundles; backup
  retention is managed.
- **No telemetry.** Off by default; no data is sent without explicit consent.
- Tests use synthetic data. Private project code is never a public fixture or benchmark.
- Supply chain: dependencies are permissive-licensed only (`cargo-deny`), CI actions are
  pinned to commit SHAs with least-privilege permissions, and PR workflows have no secrets.
