# knowell-engine

The facade that makes Knowell one product. One `Engine` owns the store pool, the
`knowell_index::Indexer`, the embedders, per-generation snapshots, the code-graph cache, the
memory repository and the settings, and implements both:

- `knowell_mcp::KnowellTools` — the 14 agent tools (`open_workspace` … `index_status`), and
- `knowell_server::Engine` — the REST operations behind the panel (`/api/v1/search`,
  `/graph`, `/memory`, `/profiles`, …).

```rust,ignore
let engine = Engine::builder(store, IndexerConfig::new(data_dir, org))
    .engine_config(&engine_config)                       // provider kinds and models
    .embedder(name("local"), Arc::new(any_embedder))     // shared with the indexer
    .workspace(resolved_workspace)                       // registered on build
    .access(Arc::new(StaticAccess::local_admin(user)))   // who MCP callers are
    .settings(EngineSettings::default())
    .build()
    .await?;
engine.indexer().index_workspace(&resolved_workspace, Priority::Interactive).await?;
let server = knowell_mcp::KnowellServer::new(Arc::new(engine.clone()));   // MCP
let state = AppState::builder(config).with_engine(Arc::new(engine));      // REST
```

## Public API

| Item | Purpose |
|---|---|
| `Engine::builder(store, indexer_config)` → `EngineBuilder` | `engine_config`, `embedder`, `workspace`, `settings`, `access`, `memory`, `relation_stage`, `build().await` |
| `Engine::indexer()`, `store()`, `settings()`, `workspace_names()`, `add_workspace(&resolved)` | the parts, and late registration |
| `Access`, `AccessResolver`, `StaticAccess` | identities; MCP callers resolve to **agents** acting for their user |
| `MemoryRepo` (+ `StoreMemory`, `InMemoryMemory`, `RecordQuery`, `RecordRow`, `TaskRow`, `CheckpointRow`, `MemoryError`) | persistence seam for knowledge records and tasks; the store implementation is the default |
| `EngineSettings` (+ `DomainConfig`, `RelationStageInfo`) | search knobs, caches, context TTL, glossary, domains, eval report directory, prices, acceptance policy, role |
| `HybridRetriever`, `HYBRID_RETRIEVER` | `knowell_eval::Retriever` named `hybrid` for `know eval run` |
| `EngineError` | construction and non-tool failures; maps to `ToolError` and `knowell_server::EngineError` |

## Data flow

```text
 pin manifest (permission-filtered)        snapshot per (view, generation), cached
 ───────────────────────────────────       ─────────────────────────────────────────────
 visible projects only (knowell-auth)      files_at · get_content · chunks_of (per file)
 active generation + commit per view       edges_with_origins · definitions_in_paths ·
 or the ref an MCP `views` pin names        contracts_with_origins (batched)
 personal overlay (owner only)             parsed symbols (signature, doc, parent)
          │                                chunk term sets for lexical → chunk mapping
          ▼
 knowell_query: plan ─▶ Exact / Lexical / Semantic candidates ─▶ fusion ─▶ graph expansion
                         │        │          │                              │
                 snapshots  Tantivy of   pgvector `nearest` per        stored imports +
                 + overlay  the pinned   profile over views whose       references/calls
                            generation   vectors cover the pinned       (GraphAdapter)
                            (+ overlay)  generation
          ▼
 SearchResponse ─▶ MCP hits with evidence / REST SearchResponse / pack() for build_context
```

### Search adapters (`knowell-query` traits)

| Source | Implementation | Notes |
|---|---|---|
| `ExactSource` | `ExactAdapter` over snapshots and overlays | symbol names (qualified exact 6 > qualified suffix 5 > case-insensitive 4 > short name 3 > case-insensitive short 2), paths (exact, suffix, file name), contract keys from the generation's contract participations |
| `LexicalSource` | `LexicalAdapter` over `Indexer::lexical(view)` | only when the view's active generation **is** the pinned one (otherwise reported, never substituted); file-level hits are mapped onto the chunk sharing most `matched_terms` (ties: symbol chunk, shorter, earlier); overlay hits come from the overlay's in-memory index; lists of several views are merged by BM25 score |
| `VectorSource` | async `semantic()` (candidates gathered before fusion, as the query README prescribes for async sources) | one `nearest` search per embedding profile over the views whose **active index generation covers exactly the pinned generation**; lists of different profiles are interleaved by rank, never compared; the query is embedded with the profile's own embedder (Gemini adds its query prefix); a cloud provider is never asked on behalf of a local-only project |
| `GraphExpander` | `GraphAdapter` over snapshots | callers/callees from stored `references`/`calls` edges of the node's symbol, plus file-level importers/imports; callers in test files are `test` edges |
| `SnippetSource` | `SnippetAdapter` over prefetched text | bodies are line slices of the cited version (capped at `max_fetch_lines`); skeletons are doc + signature of the symbol, or `knowell_parse::skeleton` for whole files |

`semantic` reports one degradation per project it could not use: `X has no embedding
provider`, `X is local-only and its provider is a cloud service; nothing was sent`,
`embeddings of X generation g are not ready`, `X: <unavailable reason>`; overlay files are
reported as not embedded.

### Code graph

`knowell-graph::CodeGraph` per set of pins (cache of 4, retired on activation events):
files, parsed symbols, `defines`/`contains` structure, stored `imports` (files or unresolved
placeholders), every other stored edge (symbol `references`/`calls`, contract edges) and the
contract participations as contract edges (`Exposes`, `Produces`, `Writes`, `Consumes`,
`Reads`, `DependsOn`, `Defines`). `trace_flow` walks it; `analyze_impact` runs
`CodeGraph::impact`.

## MCP tools over real data

| Tool | What it does |
|---|---|
| `open_workspace` | pins every visible project's active generation (or the ref `views` names; a missing ref is `ref_not_found`), attaches the caller's personal overlay when `working_directory` is inside a worktree of a project's repository, builds the start-up pack with `knowell_knowledge::bootstrap_pack` (accepted rules, decisions, open tasks; omitted items → `budget_exhausted`; conflicts reported) and returns a `context_id` (idle TTL 2 h, bound to the caller) |
| `search` | the hybrid pipeline above; hits carry `Evidence` (project, ref, layer, 40-hex commit, path, lines, content hash, symbol, why, freshness T0–T2, index state) and a snippet; memory hits come from full-text search over readable scopes |
| `fetch` | resolves result ids `kn:{project}:{commit12}:{hash16}:{path}#L{a}-L{b}` against the context: same version → `current`; another version found in `file_history` → `changed` + `current_id`; path gone → `deleted`; paths read the overlay first, then the pinned view; sensitive paths → `excluded_by_policy` |
| `inspect_symbol` | snapshot symbols by name or id: signature, doc, references (stored `references`/`calls` edges, then file importers), tests (the same from test files); `references_complete` is always `false` and a `no_reference_resolution_for_language` gap says why |
| `trace_flow` | walk of the code graph from a symbol (and its file), result id or contract key over the requested relations; synchronous (job ids are not issued) |
| `analyze_impact` | symbol / file → impact of the symbol and its file; **diff** → `git diff` between two refs of the project (renames tracked), changed symbols by parsing both blob versions; **patch** → the unified diff is parsed (untrusted input: validated paths and counts), applied to the pinned version in memory, changed and added symbols found by parsing both sides; then `CodeGraph::impact`; risk factors: cross-project consumers, public contract, many dependents, untested code, unresolved references |
| `contracts` | the generation's contract participations (store `contracts_with_origins`) grouped by (kind, key), filtered by query substring, kinds and project; endpoint-without-client / event-without-consumer findings |
| `build_context` | focus paths first, then accepted rules/decisions matching the task (≤ ¼ of the budget), then `knowell_query::pack` of a hybrid search in the remaining budget; every entry cites its source; `uncertainties` from the pack |
| `history` | co-changed files from indexed generations (files that changed in the same generations as the subject), rationale = memory records citing the file or the symbol |
| `read_memory` | records in readable scopes (organization and workspace with workspace `read_memory`, visible projects, the caller's own user scope and tasks), filters, conflicts |
| `write_memory` | scope authorised with `ProposeMemory`; evidence ids resolved to exact versions; `KnowledgeRecord::write` (secret guard, policy: agents → `proposed`, always); idempotency key → deterministic record id |
| `resume_task` | lists the caller's open tasks or resumes one with `knowell_knowledge::resume`; `changed_since` comes from `git diff` between the checkpoint's and the context's commits of every moved project; stale decisions and stale records of moved projects |
| `save_checkpoint` | creates the task (deterministic id with an idempotency key) or adds to it: progress note, decisions (task-scope, proposed for agents), open questions (replaced), related symbols, status, the current manifest, then a checkpoint |
| `index_status` | `ViewStatus` per visible project: T0–T3 states (`ready`, `building`, `queued`, `disabled` for no provider, `unavailable` for skipped/failed), latest-seen vs indexed commit, languages with analysis level, embedding profile, last activation, running jobs of visible projects |

### Gap reasons used

`project_not_indexed`, `ref_not_found`, `embeddings_not_ready` (semantic degradations),
`relations_not_ready` (no relation stage; git log/blame unavailable),
`no_reference_resolution_for_language`, `no_candidates_in_selected_ref`,
`filters_excluded_all`, `excluded_by_policy` (sensitive paths; plain-directory projects have
no commit to cite), `not_found`, `budget_exhausted`, `limit_reached`, `no_matches`, and
`no_rule_pack_for_framework` with the message prefix **`contracts_not_extracted:`** while no
relation stage is configured (`EngineSettings::relation_stage`). `GapReason` has no
`contracts_not_extracted` value; the prefix keeps it machine-detectable.

### Permission enforcement points

1. **Pinning** (`scope.rs`): only projects `visible_projects` + `authorize(ReadCode)` allow
   enter a manifest; invisible projects, ids and pins answer exactly like missing ones.
   Contexts are bound to the identity that opened them and re-filtered on every use.
2. **Search, expansion, packing** (`search.rs`): adapters read only prepared views, which
   exist only for pinned projects; overlays only when
   `authorize(ReadUncommittedOverlay(owner))` allows (admins do not see others' overlays).
3. **Memory and tasks** (`tools/memory.rs`): reads limited to readable scopes; writes need
   `ProposeMemory` / `WriteTask` on the concrete scope; agents never accept (policy +
   `authorize`); REST decisions need `AcceptMemory` on the record's scope and are audited.

## REST (`knowell_server::Engine`)

Shapes follow `panel/src/lib/api/types.ts`:

| Request | Answer |
|---|---|
| `HealthDetail` | `{freshness: TierFreshness[] \| null, recentErrors: ErrorEntry[], resources: null}` |
| `Search` | `SearchResponse` (all visible workspaces, or those of `projectIds`; `score.bm25` / `vector` are fusion contributions, `null` when the source did not run) |
| `Graph` | `GraphSlice`: hierarchy = services → top-level directories (`module:<projectId>:<dir>`, with aggregated import edges) → top-level symbols; contracts = contract nodes |
| `GraphInsights` | `GraphInsight[]` from `CodeGraph::insights` |
| `Memory`, `DecideMemory` | `MemoryRecord[]` / `MemoryRecord` (`stillValid` compares evidence hashes with the active view); accepting a proposal tagged `supersedes:<id>` supersedes that record |
| `Tasks` | `TaskRecord[]` (`changedSinceCheckpoint` = projects whose commit moved) |
| `Rules` | `ArchRule[]` from records tagged `rule` (no architecture-rule engine yet: `violations` empty) |
| `Profiles` | `EmbeddingProfile[]` from the store (`provider` is the stored kind, e.g. `fake`) |
| `SwitchEstimate` | `SwitchEstimate`: chunks × bytes of the affected projects, tokens ≈ bytes/4, cost = tokens × `prices_usd_per_million_tokens[provider]` (µUSD), disk = chunks × dims × 2 B; duration not measured (0, with a warning) |
| `EvalReports` | `EvalReport[]` from `*.json` reports in `eval_reports_dir` |
| `Usage` | `UsageReport` from in-process MCP counters (reset at start) |
| `Integrations` | `IntegrationsStatus` (MCP status from usage; agent and webhook checks belong to the CLI/server: empty) |
| `Admin` | hub only: principals, tokens (prefix only), last 100 audit entries |
| `Domains`, `Glossary` | from `EngineSettings::domains` / `glossary`; empty arrays when none are configured |

Engine-defined bodies:

- `Trace` (`POST /graph/trace`) → the `trace_flow` output (`{nodes, edges, truncated, gaps}`);
  `from` is a result id (`kn:…`), a contract key (contains a space, `/` or `:`) or a symbol.
- `Impact` (`POST /graph/impact`) → the `analyze_impact` output; `target` `project/path`
  is a file, anything else a symbol; `patch` needs one project (`projectIds` or a
  `project/…` target).
- `Context` (`POST /context`) → the `build_context` output.
- `StartSwitch` (`POST /profiles/switch`, 202) → `{switchId, fromProfileId, toProfileId,
  views, jobs, startedAt, state: "building"}`. The workspaces are re-registered with the
  target profile and the affected views are rebuilt in the background; the active
  generation keeps serving with the old profile's vectors (the semantic source picks, per
  view, the profile whose active index generation covers the pinned generation) until a
  rebuild with complete new vectors activates. `Profiles` shows `switch.progress`.

## Evaluation hook

```rust,ignore
let hybrid = HybridRetriever::new(&engine, &access, &workspace).await?; // multi-thread runtime
let report = knowell_eval::run(&corpus, &queries, &[&grep, &bm25, &hybrid], 10)?;
```

File-level ids `<project>/<path>`; graph expansion off; the manifest is pinned once per run.

## Settings and limits

| Setting | Default |
|---|---|
| `context_ttl` / `max_contexts` | 2 h idle / 1024 |
| `text_cache_bytes` | 64 MiB of redacted text |
| `snapshot_cache` | 32 generations |
| `max_fetch_lines` | 2000 lines per fetched item or packed body |
| `search` | `knowell_query::SearchConfig::default()` (unmeasured defaults) |

## Known limits

- A snapshot costs two store round trips per file (text, chunks) plus parsing; fine for
  thousands of files, slow for very large views on first use. A store query returning text
  and chunks of a generation in bulk would remove that.
- Lexical search serves only the active generation of a view; a context pinned before an
  activation reports `lexical: … call open_workspace again` instead of using another
  generation.
- Usage counters, checkpoint idempotency keys (beyond deterministic task ids) and profile
  switch records are in process memory.
- `history` has no git log or blame (knowell-source has no reader for them).
- MCP tool results never contain jobs: every tool computes synchronously.

## Tests

```sh
python scripts/buildlock.py cargo test -p knowell-engine
```

Unit tests run anywhere. `tests/engine/` indexes the acme-goods fixture (seed 42, Small,
written with git) with `FakeEmbedder` against `KNOWELL_TEST_DATABASE_URL` (see the
knowell-store README) and drives every tool, the REST engine and the `hybrid` retriever;
without the variable it prints one skip line and passes.
