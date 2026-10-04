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
| `Access`, `AccessResolver`, `StaticAccess`, `StoreAccess` | async identity resolution; MCP users become **agents**, verified agents and service accounts retain their identity |
| `MemoryRepo` (+ `StoreMemory`, `InMemoryMemory`, `RecordQuery`, `RecordRow`, `TaskRow`, `CheckpointRow`, `MemoryError`) | persistence seam for knowledge records and tasks; the store implementation is the default |
| `EngineSettings` (+ `DomainConfig`, `RelationStageInfo`) | search knobs, caches, context TTL, glossary, domains, eval report directory, prices, acceptance policy, role |
| `HybridRetriever`, `HYBRID_RETRIEVER` | `knowell_eval::Retriever` named `hybrid` for `know eval run` |
| `Engine::list_embedding_profiles`, `get_embedding_profile`, `ProfileMetadata`, `ProfileSelector` | organization-authorized, provider-free persisted profile catalogue |
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
| `SnippetSource` | `SnippetAdapter` over selected pinned text | Source bodies preserve original bytes in query-centered or complete declaration ranges (capped at `max_fetch_lines`); explicit comparator skeletons remain extracted doc/signature or file outlines |

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
| `inspect_symbol` | version-verified definitions/signatures plus bounded stored occurrences and evidenced calls, references, implementations and actual test associations; compiler analysis requires matching per-file import coverage; import navigation remains an import, and `references_complete` remains `false` with explicit inventory and clipping gaps |
| `trace_flow` | walk from a symbol/file, verified result id or contract key; opt-in `navigation` gates expansion by evidence/resolution, bounds fanout and candidate leaves, and reports omitted or unavailable analysis; synchronous (job ids are not issued) |
| `analyze_impact` | symbol / file → impact of the symbol and its file; **diff** → `git diff` between two refs of the project (renames tracked), changed symbols by parsing both blob versions; **patch** → the unified diff is parsed (untrusted input: validated paths and counts), applied to the pinned version in memory, changed and added symbols found by parsing both sides; then `CodeGraph::impact`; risk factors: cross-project consumers, public contract, many dependents, untested code, unresolved references |
| `contracts` | the generation's contract participations (store `contracts_with_origins`) grouped by (kind, key), filtered by query substring, kinds and project; endpoint-without-client / event-without-consumer findings |
| `build_context` | focus paths first, then accepted rules/decisions matching the task (≤ ¼ of the budget), then bounded body-only Source selection over hybrid results and related code; every shown region cites its version; caveats follow selected evidence |
| `history` | co-changed files from indexed generations (files that changed in the same generations as the subject), rationale = memory records citing the file or the symbol |
| `read_memory` | records in readable scopes (organization and workspace with workspace `read_memory`, visible projects, the caller's own user scope and tasks), filters, conflicts |
| `write_memory` | scope authorised with `ProposeMemory`; evidence ids resolved to exact versions; `KnowledgeRecord::write` (secret guard, policy: agents → `proposed`, always); idempotency key → deterministic record id |
| `resume_task` | lists the caller's open tasks or resumes one with `knowell_knowledge::resume`; `changed_since` comes from `git diff` between the checkpoint's and the context's commits of every moved project; stale decisions and stale records of moved projects |
| `save_checkpoint` | creates the task (deterministic id with an idempotency key) or adds to it: progress note, decisions (task-scope, proposed for agents), open questions (replaced), related symbols, status, the current manifest, then a checkpoint. Decisions, task update, checkpoint and the idempotency receipt are stored in one transaction; a retry with the same caller and key returns the original checkpoint, also after a restart or when retries race |
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

Search coverage reports one `no_reference_resolution_for_language` gap per project,
listing its affected languages in sorted order without duplicates. All affected
languages remain explicit, including document and configuration languages. The gap
states that references use structural import matches only, so missing callers do not
prove none exist. Ranking, result identities and evidence fields are unchanged.

### Permission enforcement points

The CLI uses `StoreAccess` for HTTP identities on every role, preserving token scopes
and loading grants per call. A hub rejects untyped subjects and unauthenticated local
callers. Standalone stdio retains its explicit machine-owner access. HTTP authentication
uses `knowell_server::AuthenticatedCallers`; client-supplied metadata cannot supply a
principal or widen a token's scopes.

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
| `Profiles` | `EmbeddingProfile[]` from the store (`provider` is the stored kind, e.g. `fake`); `active` when a visible project's view serves it; `switch` is the state and progress of the newest switch to it |
| `SwitchEstimate` | `SwitchEstimate`: chunks × bytes of the affected projects whose pinned generation the target does not cover yet (no vectors are derived from another profile), tokens ≈ bytes/4, cost = tokens × `prices_usd_per_million_tokens[provider]` (µUSD), disk = chunks × dims × 2 B; duration not measured (0, with a warning) |
| `Switches` | `ProfileMigration[]` from the stored switches, newest first, with origin, requester, member views and their coverage, failures, activation time and `reversibleUntil` |
| `CancelSwitch`, `RollbackSwitch` | the cancelled switch / the reverse switch (see below) |
| `EvalReports` | `EvalReport[]` from `*.json` reports in `eval_reports_dir` |
| `Usage` | `UsageReport` from the stored hourly MCP usage plus calls not yet flushed (every `usage_flush_interval`, default 5 s, and by `Engine::flush_usage` at shutdown); latency percentiles are histogram bucket bounds (at most 25 % high); each agent label counts as one session |
| `Integrations` | `IntegrationsStatus` (MCP status and `lastCallAt` from stored and buffered usage; agent and webhook checks belong to the CLI/server: empty) |
| `Admin` | hub only: principals, tokens (prefix only), last 100 audit entries |
| `Domains`, `Glossary` | from `EngineSettings::domains` / `glossary`; empty arrays when none are configured |

Native REST dispatch requires organization `ReadCode` for profiles, switch estimates,
switch lists, evaluation reports, usage and integrations before selector validation or
I/O. `StartSwitch`, `CancelSwitch` and `RollbackSwitch` require organization
`ManageProviders`; Hub administration requires
organization `ManageUsers`. Grants, token scopes and the agent action ceiling apply.
Non-Hub administration still returns its constant unavailable response. Usage periods
are validated as 1–365 days after authorization, before date arithmetic.

Engine-defined bodies:

- `Trace` (`POST /graph/trace`) → the `trace_flow` output (`{nodes, edges, truncated, gaps}`);
  `from` is a result id (`kn:…`), a contract key (contains a space, `/` or `:`) or a symbol.
- `Impact` (`POST /graph/impact`) → the `analyze_impact` output; `target` `project/path`
  is a file, anything else a symbol; `patch` needs one project (`projectIds` or a
  `project/…` target).
- `Context` (`POST /context`) → the `build_context` output.
- `StartSwitch` (`POST /profiles/switch`, 202) → `{switchId, fromProfileId, toProfileId,
  views, jobs, startedAt, state, switches}`: one stored switch per workspace for the
  visible projects that do not serve the target (`knowell_index::Indexer::start_switch`).
  The target must match a configured embedder's provider, model, dimensions and complete
  embedding/prepared/parser input format before anything is stored; unsupported formats
  are rejected with one static message, and estimates warn that the switch cannot start.
  While it builds, T2 builds the old and the target profile, so the old one keeps
  answering every query; once the target covers every member view's active generation,
  one transaction makes it serve all of them. The switch is stored: a restarted engine
  resumes it when it indexes. A context pins the profile its views served when it was
  opened, so an open context never changes profile mid-task, and an old generation is
  searched through its retired (still complete) vector index generation.
- `CancelSwitch` (`POST /profiles/switches/{id}/cancel`) stops a building switch; the old
  profile keeps serving. `RollbackSwitch` (`POST /profiles/switches/{id}/rollback`, 202)
  starts the reverse switch within the retention (7 days); when the old vectors still
  cover the active generations it activates at once without provider calls.
- No vectors are derived from another profile: a target without vectors for a
  generation is embedded again, also for a dimension reduction (truncation and
  normalization are not implemented).

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
| `parse_product_cache` | disabled; opt-in private persisted local parse products |
| `max_fetch_lines` | 2000 lines per fetched item or packed body |
| `search` | `knowell_query::SearchConfig::default()` (unmeasured defaults) |

Fresh requests resolve each authorized Git target before pinning its saved index;
missing refs, commits and unborn HEADs produce `ref_not_found` without old source hits.
Explicit search project filters narrow this check before unrelated source access.
Existing contexts retain their pinned versions and still recheck current permissions.
Fresh queries observe the resolved source commit for freshness without writing it to
the store or indexing it; an advanced ref cannot make its old generation appear current.

Memory reads retain saved commits/hashes and report current, stale, catching-up or
not-indexed source state without repinning the record. Returned evidence and task
history obey current source grants even in cached contexts. Persisted project ids retain
their workspace namespace, so equal names cannot authorize a different source; exact
unregistered ids are mapped through database metadata without opening or registering
sources. Hidden or malformed references produce one generic gap. New user/organization
evidence is bound to the originating workspace, including shared in-memory repositories;
an unknown origin is omitted. Reviews reuse canonical project ids rather than resolving
equal names again. Read filtering does not rewrite records or history. History rationale
matches saved file evidence after workspace identity
and grant checks. Personal task pins require ownership or a currently matching authorized
overlay. Task ownership is filtered before limits, using raw database timestamps for
keyset pagination. Retry receipts fail explicitly when linked records are unavailable.

Local memory/task CLI reads use configured profiles and policy without preparing
provider clients or credentials. Source gaps do not prevent unsourced record reads.
Freeform symbol labels and progress remain untrusted record content, not source ids.

The native profile catalogue resolves the real caller and requires `ReadCode` on the
organization before acquiring a profile connection. Workspace/project-only grants do
not authorize it, and current grants and token scopes apply on every call. UUID lookups
filter the tenant in SQL before decoding; missing and foreign ids return the same empty result.
Metadata retains stored dimensions and input-format version, with an RFC3339 UTC
registration timestamp preserving fractional precision. Unsupported stored times
return a safe error without a panic. It uses no provider clients,
source registration or configuration-derived activity/locality claims. The standalone
CLI opens this catalogue without a workspace or provider credentials.

## Known limits

- Directory source views have no Git commit. Search reports missing commit evidence
  rather than returning source hits with invented commit ids.
- Full graph and inspection snapshots load every file in a generation in bounded
  batches. Ordinary Source search reads metadata and hydrates only admitted paths.
  Optional persisted parse products can avoid unchanged-file parsing; the metadata
  catalog still grows with the generation, so large-corpus scaling remains unmeasured.
- Lexical search serves only the active generation of a view; a context pinned before an
  activation reports `lexical: … call open_workspace again` instead of using another
  generation.
- Tool usage not yet flushed (at most
  `usage_flush_interval`) is lost if the process crashes; provider token spend is not
  recorded yet (`spendUsdMicros` is 0).
- `history` has no git log or blame (knowell-source has no reader for them).
- MCP tool results never contain jobs: every tool computes synchronously.

## Tests

```sh
python scripts/buildlock.py cargo test -p knowell-engine
```

Unit tests run anywhere. `tests/engine/` indexes the acme-goods fixture (seed 42, Small,
written with git) with `FakeEmbedder` against `KNOWELL_TEST_DATABASE_URL` (see the
knowell-store README) and drives every tool, the REST engine and the `hybrid` retriever;
without the variable it prints one skip line and passes (it fails under
`KNOWELL_TEST_STRICT=1`; see the knowell-store README).

`KNOWELL_TEST_PLAIN_DATABASE_URL` enables the separate unmodified-PostgreSQL test:
the same fixture serves lexical search, symbol references, graph and sourced memory
with configured providers, explicit semantic-unavailable gaps and zero embedding
calls. See the store README for the second test server's setup.

## Retrieval experiments and measured work

Search admits project, generation, personal-overlay shadowing, language, literal
source-root path prefixes and requested kinds before candidate quotas. Context section
filters also precede source quotas. Bounded lexical and ANN refill reports degradation
when its work cap prevents filling the shortlist. `EngineSettings.lexical_spans_per_file`
defaults to 1 and accepts only 1 through 3; invalid values fail engine construction.
Values 2 and 3 are explicit experiments with distinct non-overlapping spans. Their
file-first waves admit the best span from each pinned file before its alternatives;
all spans share the existing global and per-project candidate quotas. Exact full paths
already found at the requested pin skip query embeddings; mixed, missing and basename
queries keep normal retrieval.

The global `know --lexical-spans 2` or `--lexical-spans 3` option selects the same
experiment for local commands, stdio MCP and `serve`. `know connect` retains nondefault
values in the client launch and startup-hook arguments; the default stays portable.
For example, compare `know search "decode files" --diagnostics` with
`know --lexical-spans 3 search "decode files" --diagnostics` on the same pinned index.

On the synthetic-small fixture, the one-span ablation reproduced every unchanged
hybrid baseline metric. Both tested three-span variants failed that same subgroup
regression gate despite improving overall recall: score-first spans regressed eight
subgroup metrics; file-first waves still regressed five contract/history metrics.
The baseline and its gate were not weakened. These results motivate the one-span
default; they do not establish quality on public repositories or natural agent tasks.

`open_workspace` reads the pinned language catalog without building source snapshots.
Source search hydrates admitted candidate paths and selects complementary shown bodies,
including bounded related sources. `search.include_snippets` defaults to `true`.
Setting it to `false` selects locator retrieval: matched source coordinates, symbols,
reasons and exact typed identities remain, while selected display-body hydration,
parsing, graph expansion and body packing are skipped. This does not eliminate indexed
lexical work or ordinary semantic query embeddings. Empty admitted catalogs and an
answered exact full-path query need no embedding request. `include_handles` defaults
to `false` for Source presentation; request it when an exact pinned fetch handle is
useful. Handles for omitted continuation ranges remain available without requiring
another read of source already shown.

Opt-in `include_diagnostics` returns phase durations in whole milliseconds, retrieval
work, embedding calls/failures and successful provider usage. `elapsed_ms` measures
engine tool execution; MCP rendering, serialization, transport and client/model time
are outside it. `snippet_read_ms` includes selected hydration and body acquisition/
packing work, and is zero for locator retrieval. The phase durations do not partition
all tool work. `prepared_file_occurrences` counts represented base and personal file
occurrences, including warm cached metadata; it is not a count of database reads.
`source_hydrated_paths` counts admitted pending base paths submitted to hydration;
`source_hydration_skipped_paths` counts those intentionally left unhydrated in locator
retrieval. Personal source is already held by its overlay and is excluded from both
hydration counts. These counters do not establish complete source coverage or distinguish
cache hits from database body reads. Failed-operation token usage is unknown, not zero;
reported/estimated counts remain distinguishable. Durations describe one call, not
latency percentiles, a measured speedup or a quality score.

`build_context.projects`, `path_prefixes` and `languages` are hard source restrictions.
They apply to preferred focus sources, retrieval, relation expansion and packing;
requested sections also constrain source admission. Empty lists preserve the reachable
workspace scope. Prefixes are normalized once, remain case-sensitive literal UTF-8
source-root-relative prefixes and never interpret `%`, `_`, `*` or `?` as patterns.
A trailing `/` restricts a directory; `src/pay` can also match `src/payments`.
`focus_paths` and `focus_symbols` remain preferred evidence inputs, not restrictions on
the rest of the pack. Explicit path/ranges and qualified names reduce ambiguity;
ambiguous symbol names are reported rather than selecting an arbitrary component.

`build_context.selection_strategy` defaults to body-only `source`. Explicit `rank`,
metadata `mmr`, `role_coverage` and `bounded_bundles` remain research comparators. Source
packing uses actual body cues and source redundancy with bounded work and truthful
contiguous excerpts; it does not certify semantic sufficiency. Normal `search` uses the
same selector with a default 4000 estimated-token response budget. Inspection steps refer
only to bodies actually shown at exact versioned spans. Missing roles remain unknown.
Source budgeting uses a UTF-8 byte estimate (four bytes per token) with a framing
allowance per selected body and space reserved for shared text. The renderer separately
admits complete entries, provenance and limitation text under
`min(requested * 4, 800000)` UTF-8 bytes; it does not silently cut a shown source body.
`budget.used` is the engine's selected-body estimate, not final response consumption:
headers, notes, memory hits and locator metadata are not included, and rendering can
omit additional entries. Locator results can therefore report zero estimated body
tokens while returning useful metadata. Neither estimate covers the MCP envelope or
guarantees a real model-token limit; benchmark client-reported token usage separately.
Source acquisition and packing share at most 64 admitted candidates. Region selection
scans at most 20,000 lines within a retrieved declaration; query cues guide excerpts,
not semantic completeness. Omitted declaration ranges have disjoint pinned fetch handles.
Imports and generic symbol references are never relabeled as calls or test coverage.
Call/test roles require stronger proven relationships and located source endpoints;
entry/config roles remain missing without an authoritative source relationship.
Memory/rules-only context skips source retrieval and query embeddings.

Supported source-only Rust calls are syntactic observations, not a compiler-complete
or runtime call graph. Ambiguous or unsupported bindings retain their resolution and
coverage limits. Imported SCIP compiler observations are admitted only when their
generation, source path and hash match the pinned base snapshot; evidence type and
resolution status remain separate. A personal overlay has no matching compiler-input
identity, so base compiler graph evidence is not reused for it, including unchanged
files whose bindings may depend on edited build inputs. Neither empty traversal nor
a `resolved` label proves complete call/test coverage or runtime behavior.

## Optional persisted parse products

`EngineSettings.parse_product_cache` is disabled by default. To experiment with cold
process reuse, set it to `Some(ParseProductCacheSettings::new(private_directory))`.
The `know` binary exposes the same opt-in as the global `--parse-cache` flag; local
search, stdio MCP and `serve` use `$KNOWELL_HOME/cache`. Passing it to `know connect`
retains it in the client's launch arguments. For example:

```sh
know --parse-cache --config /private/config.toml --workspace /repo/knowell.toml search "symbol" --diagnostics
```

Each organization gets a separate schema-versioned namespace. Keys contain the full
repository-relative path, BLAKE3 hash of the exact redacted body, parser tag and every
`ParseLimits` field. Identical bytes at a renamed path are not reused: names affect
language detection, Compose dialects and generated-file classification.

Entries contain only local `ParsedFile` products. Stable store symbol ids, generation
pins, current source grants and resolved relationships are not persisted in this cache;
they are acquired for the requested snapshot. Reuse validates the key, product digest,
path/body/language, resource limits and source-range/parent bounds. Malformed, truncated,
oversized or mismatched entries produce a debug `error_kind` and reparse that same source.
Partial/degraded products are never cached. A payload hash detects corruption; it is not
authentication against a writer who controls the private cache directory.

Default limits per organization are 8 MiB per serialized entry, 128 MiB total payload
and 2048 entry/temporary files. Serialization and reads are byte bounded. Writers use
an OS file lock across processes and publish a synchronized temporary file with atomic
rename. Budget accounting includes temporary and abandoned files; a busy writer or full
budget skips the cache write. There is no automatic eviction or project-deletion cleanup;
the configured directory must remain private and its retention is managed by its owner.
Unix-created cache directories/files use owner-only modes; Windows inherits directory ACLs.

Search snapshots read file, chunk and relationship metadata in 1000-file keyset pages
without transferring source bodies or parsing the generation. Source/locator search
preparation applies explicit path-prefix and stored occurrence-language restrictions
before the database page limit. Immutable names, kinds and source anchors on versioned
defines evidence preserve symbols in whole-file/grouped chunks without loading bodies;
historical metadata does not borrow a later logical symbol name. Invalid or conflicting
labels are rejected, while unlabeled older indexes retain conservative chunk recovery.
Only admitted files contribute chunks, definitions and
relationship/contract origins to that catalog. Blob-level language hints cannot replace
the pinned file occurrence's language. Tenant predicates, generation validity and
bytewise exclusive path cursors remain unchanged.

Partial catalogs are cached separately by generation pin and normalized prefix/language
scope; an unrestricted metadata catalog and a full graph snapshot cannot be replaced
by a partial one. Equivalent scope ordering and duplicates share a cell. All cells
share the configured count-bounded LRU capacity, and retirement removes older cells
for the view. Immutable language counts are prepared once rather than walking every
file on a warm query.

Source is hydrated only for admitted pinned paths, with a 1000-path and 64 MiB
distinct-body bound per project hydration, and cached text is reused within the existing
byte-bounded text cache. Locator search never performs that selected hydration. Legacy
metadata may have an unknown line count; selected source resolves it instead of inferring
file length from chunk coverage. Ordinary source fetches also use metadata and actual
selected text. Full graph and inspection tools retain their explicit full snapshot path.

Unrestricted catalogs still grow with the pinned generation; restricted catalogs grow
with the admitted scope. Hydration currently clones and rebuilds the admitted catalog
to attach selected parser products. Metadata cache capacity bounds the number of cells,
not aggregate heap bytes. SQL scope filtering reduces unrelated row transfer; it does
not guarantee scope-independent database execution cost. This is not distributed
storage or a billion-line scale guarantee. Measure cold preparation, selected body
reads, parse products, CPU and memory before making performance claims. Debug counters
contain no source text or secret values.
