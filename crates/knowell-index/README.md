# knowell-index

The indexing engine of Knowell: it keeps every tracked view of every project current as
commits land and files are saved. It resolves track targets (never substituting another
ref), plans the change set of each build, runs the build as durable jobs, writes text,
symbols, references, chunks, edges and contracts into a new **generation**, and activates it
behind the store's generation fence as soon as text, symbols and relations are stored.
Embeddings follow as enrichment of the active generation. Queries keep using the previous
generation until the new one is activated.

```rust,ignore
let indexer = Indexer::builder(store, IndexerConfig::new(data_dir, org_name))
    .engine(&engine_config)                     // provider kinds and default models
    .embedder(provider_name, Arc::new(embedder)) // any knowell_embed::Embedder
    .build()?;                                   // contract linking (T3) is on by default
let (registration, outcomes) = indexer.index_workspace(&resolved, Priority::Interactive).await?;
let status = indexer.status(view).await?;      // tiers, commits, lag, last error
let coverage = indexer.embedding_coverage(view).await?; // vectors of the active generation
let mut events = indexer.subscribe();          // progress for SSE

// Long-running: a worker, repository watchers, periodic reconciliation.
let shutdown = CancellationToken::new();
let worker = Worker::new(&indexer, WorkerConfig::default()).spawn(shutdown.clone());
let watch = indexer.watch(shutdown.clone())?;
```

## Pipeline

```text
 trigger: refresh_view / index.sync job / watcher (HEAD, refs, rescan) / reconcile
    │
    ▼
 sync ── resolve target (git refs, exact name only; directory → tree hash)
    │      missing ref → view failed with the reason, nothing else indexed
    │      same as active + matching content-policy manifest → up to date
    ▼
 index.text (T0) ── begin (or resume) generation g
    │   plan: initial | incremental (git diff) | rewrite (full re-walk) | rebuild | directory
    │   exclusion by path → size limit → binary/UTF-8 → redaction   (knowell-source)
    │   upsert_contents (redacted text only), apply_file_changes
    │   lexical index g: copy of the active one + changes (or rebuild from the store)
    ▼
 index.symbols (T1) ── parse changed files (knowell-parse)
    │   content-level chunks + per-path prepared inputs (chunk_input)
    │   source hierarchy: declaration/enclosing ranges, parent ordinal, exact-source flag
    │   symbols (path-qualified), definitions, references (conservative, see below)
    │   syntactic edges: file defines symbol, container contains member, file imports
    │   renamed files: rename_symbol keeps symbol ids
    │   dependents: unchanged files whose imports / references this build invalidated
    ▼
 index.relations (T3) ── contract linking (LinkRelationStage, default) or a custom stage
    │   activate_generation(g)  ── fence: only if newer than the active one
    │   after activation: switch and garbage-collect lexical indexes, prune history,
    │   emit Activated + StalenessEvent, queue T2
    ▼                                   ── g is searchable (text, symbols, graph) here ──
 index.embeddings (T2) ── only while g is active; data policy, provider, profile, budget
        inputs of changed files (all files when the previous vectors are incomplete)
        → missing_embeddings → embed only those → activate the vector index generation
```

Every stage before activation first checks that `g` is still building and that the view's
latest seen commit is still the target; otherwise the build is obsolete, `g` is failed
with the reason, and the job ends quietly (it is not retried). Every store write is fenced
by the store as well. T2 checks instead that `g` is still the active generation, at its
start and before every provider batch.

T1 writes optional source hierarchy immediately after content-level chunks. Split
declarations keep their full declaration range and immediate enclosing symbol; continuation
pieces retain their earlier parent chunk ordinal. A byte comparison identifies whether
the embedding chunk is contiguous source, so synthetic container headers and joined
top-level regions are never mistaken for literal source. This metadata does not change
prepared embedding inputs or call a provider. Older indexed content remains explicitly
without hierarchy until it is analyzed again.

If pgvector storage is unavailable, registration reports T2 as unavailable with
installation guidance. T0, T1 and T3 still run; no embedding inputs are sent.
After installing pgvector, run `know init` and restart the engine to register the
configured profile and backfill vectors.

### Searchable in seconds

A generation is activated right after T3, so lexical, symbol and graph search serve a new
commit after T0 + T1 + T3, without waiting for a provider. T2 then runs as enrichment:

- **Vectors are keyed by profile and prepared input, never by generation**, so writing them
  after activation is safe, and whatever a run wrote is reused by every later run.
- `ViewStatus` shows T2 as `pending` / `running` on the active generation (also in a
  process that did not run the build: it reads the vector index generation and the T2 job
  from the store), then `done`, `skipped` or `failed`.
- `Indexer::embedding_coverage(view)` returns `EmbeddingCoverage { inputs, embedded,
  complete }` for the active generation and the view's profile: chunks with a vector out
  of the chunks meant to be embedded. Queries use it to say "semantic coverage partial"
  instead of presenting older vectors as current; the vector index generation of `g` is
  only activated when T2 finished.
- A T2 of a generation that a newer one replaced **stops early** (before its first or next
  provider batch), fails its vector index generation as superseded and ends quietly; the
  newer generation's T2 embeds what is still missing. A run that finished all batches still
  activates its vector index generation (the fence refuses an older one), so the next run
  can stay incremental.
- T2 does not hold the view's stage lock: slow provider calls never delay the next build's
  T0..T3. T2 runs of one view are serialized in process.
- Reconciliation queues the T2 of an active generation that has neither vectors nor a T2
  job (a crash between activation and queueing).

Measured on the Small fixture (10 projects, 249 files, `FakeEmbedder` behind a simulated
remote provider with 40 ms per call of at most 16 inputs, the local throwaway PostgreSQL +
pgvector test server, debug build, one Windows 11 developer machine, `run_until_idle_with(2)`,
warm OS caches; test `searchable::time_to_searchable_on_the_small_fixture`):

| | all 10 views searchable (T0+T1+T3, activated) | embeddings complete (T2) |
|---|---|---|
| default (contract linking on) | 3.8 s | 5.4 s |
| `NoRelations` | 2.3 s | 3.9 s |

Before this ordering, activation waited for T2, so "searchable" equalled "embeddings
complete". Run alone; under parallel test load the numbers roughly double. Stage totals
(sum over views, default stage): T0 2.8 s, T1 1.3 s, T3 2.8 s, T2 3.1 s.

### Planning

| Situation | Plan |
|---|---|
| no active generation | **initial**: every file of the commit |
| active commit is an ancestor of the target | **incremental**: `git diff` names the changed paths (with renames); only those blobs are read |
| not an ancestor (force-push, rebase, reset) | **rewrite**: full re-walk of the target's tree; history-based assumptions are dropped. Blobs known from the previous manifest are not read again; stored content, chunks and vectors are reused by hash |
| forced (`rebuild_view`, reconciliation) or content policy changed | **rebuild**: full re-walk ignoring the blob cache |
| plain directory (`track = "worktree"`, no `.git`) | **directory**: walk the directory |

Remote-branch, tag and commit views are read from git objects; the user's checkout, index
and branch are never touched. Single files (overlays, re-reads of directory sources) are
read with `knowell_source::read_file`, the same pipeline as the walkers.

### Symbol identity

`knowell-parse` names symbols by their container path inside the file
(`SubscriptionService.cancel`). The store identifies a logical symbol by (project, kind,
qualified name), so the engine qualifies names with the path:
`src/billing/service.ts#SubscriptionService.cancel` (`symbol_key` / `split_symbol_key`).
Unrelated same-named symbols in different files stay distinct; when a file is renamed or
moved, symbols that still exist (same kind and in-file name) are renamed with
`rename_symbol`, keeping their ids, occurrences history and memory links.

### Per-path embedding inputs

The prepared embedding input of a chunk includes the project and the path, so it belongs to
a file version, not to the content: T1 records every chunk's input per path in the store's
`chunk_input` table (with whether the content policy lets it be embedded), and T2 resolves
a file's inputs through it. Content-level `chunk` rows stay one set per content (lines,
bytes, kind, symbol path). Consequences:

- Identical content at two paths gets two inputs and two vectors; a vector hit is located
  back to its own path (`content::locate_chunk_inputs`), not to every copy.
- **A renamed or moved file is embedded again**, because its input changed with its path.
  This is the trade-off for path-aware inputs; symbol ids, text, chunks and the old
  vectors (still reachable from older generations) are kept.
- Files indexed before per-path inputs existed have no rows; T2 analyses them again from
  stored text and records them (`content::locate_chunk_inputs` and the scoped vector search
  fall back to the content-level rows for such file versions).

### References

`knowell-parse` reports declarations and imports, not identifier uses, so T1 parses each
exact-tier file (Rust, TypeScript / JavaScript, Python, Go, Java, Kotlin, C#) a second time
to list its identifiers (outside comments, strings and object keys). A use is matched **by
name** against, in order, the file's own definitions, the definitions of files it imports
(import edges resolved to files of the view), and those of other files of the same
directory and language family. Targets are functions, methods, types, constants and macros
(not fields, variables, modules, `impl` blocks or constructors).

| Match | Evidence | Resolution | Recorded as |
|---|---|---|---|
| free name, same file or imported file, one definition | `syntactic` | `resolved` | `references` edge + occurrence (role `reference`) |
| same, several definitions (≤ 4) | `syntactic` | `ambiguous` | one edge per candidate, no occurrence |
| member name (`x.name`) in those scopes | `heuristic` | `resolved` / `ambiguous` | edges only |
| only a file of the same directory defines it | `heuristic` | `resolved` / `ambiguous` | edges only |
| more than 4 candidates, or no match | — | — | nothing |

Edges start at the innermost enclosing symbol (or the file) and carry the name, scope,
first line and use count as evidence. An occurrence with role `reference` therefore never
presents a guess. Bounds per file: 20 000 distinct identifier uses, 2 000 000 syntax nodes,
1 000 reference edges, 2 000 reference occurrences, 64 imported files, directories of at
most 256 same-family files; what is cut is logged. No semantic resolution (shadowing,
overloads across files, re-exports, dynamic dispatch) happens here.

### Dependents: import and reference re-resolution

Edges of unchanged files are resolved against the files of the generation they were
analysed in. T1 therefore also re-analyses (from stored text, at most 500 per build) the
unchanged files that import a path the build removed or renamed away, that have an
unresolved import whose specifier now resolves to a path the build added (found through
`graph::edges_into_name_tails`), or that reference a symbol whose definition the build
removed. Their edges and occurrences are rewritten; they are not re-chunked or re-embedded.
`IndexStats::dependents_reresolved` / `dependents_skipped` count them.

### Contract linking (T3)

The default relation stage is `LinkRelationStage` (`knowell-link` with the bundled rule
packs; `IndexerBuilder::link_stage` configures it, `relation_stage(Arc::new(NoRelations))`
turns relations off, any other `RelationStage` replaces it). For a build that changed files:

1. Every file of the project at `g` is extracted from the redacted text in the store (never
   re-read from the source; excluded files never reached the store). Extraction is per
   project because pack activation and constant bindings are project-wide.
2. It is linked with the other projects of the workspace that this indexer serves, at their
   active generations (cached in memory per view and generation; extracted from stored text
   on a cold cache).
3. The project's link edges (`exposes`, `consumes`, `produces`, `reads`, `writes`,
   `defines`, ... to contract nodes) and contract participations are written with origin
   `link:<path>`, sources mapped to the store's symbol ids (by path, qualified name and
   lines; the file when nothing matches). Only origins whose rows differ from the stored ones
   are replaced, so an unchanged file writes nothing.

Each file is parsed twice per build (knowell-parse in T1, the packs' queries in T3:
`extract_project` takes text, not trees). Projects above `with_max_files` (default 20 000)
fail T3 with the reason; the generation still activates and the link rows of its changed and
removed files are dropped rather than left as if current. Rows of *other* projects are
refreshed when those projects are rebuilt. `infra` contracts have no store kind yet and are
skipped (counted in the log).

## Job kinds

| Kind | Tier | Idempotency key | Priority |
|---|---|---|---|
| `index.sync` | — | none (cheap, coalesced by callers) | class |
| `index.text` | T0 | `index.text:<view>:<target>:n<last generation>[:force]` | class |
| `index.symbols` | T1 | `index.symbols:<view>:<target>:g<generation>` | class |
| `index.relations` | T3 | `index.relations:<view>:<target>:g<generation>` | class |
| `index.embeddings` | T2 | `index.embeddings:<view>:<target>:g<generation>`; `...:p<profile>` when a profile switch catches up on one profile | class − 50 |

`<target>` is the commit id, or `tree-<hash>` for directories. Classes: `Interactive` 300
(explicit refresh), `Active` 200 (watcher events), `Background` 100 (initial indexing,
reconciliation). The embedding stage runs below the other stages of its class so slow
provider calls do not starve text, symbols and relations of other views.

- **Scoping:** every job is enqueued with `jobs::enqueue_scoped(JobScope::View(view))`, and
  workers (and `run_until_idle`) claim with `jobs::claim_scoped` only the jobs of the views
  this indexer registered (plus unscoped jobs left by earlier versions). Processes sharing a
  queue but serving different views never claim, fail and dead-letter each other's jobs; a
  job of a view nobody serves stays queued until a process registers the view (or the view
  is deleted, which deletes its jobs).
- **One-workspace runs:** `run_until_idle_scoped_with(concurrency)` freezes the registered
  view scope at entry, excludes unscoped legacy jobs and recovers expired leases only in
  that scope. The local `know index` command uses this stricter runner. Concurrent
  registrations do not expand its scope; foreign job state, attempts and leases remain
  unchanged. The existing runner and workers retain their legacy recovery behavior.
- **Idempotency:** two triggers for the same target and state share one job. A target
  whose earlier build ended without result (it was superseded, then the ref moved back, or
  it was dead-lettered) gets a fresh job.
  An unchanged target is up to date only when its active manifest also matches the
  configured content policy. Changed exclusions or roots, and missing or corrupt policy
  manifests, trigger a full reconciliation before another completion report.
- **Retries / DLQ:** failed attempts are retried with exponential backoff
  (`JobSettings`); after `max_attempts` the job is dead and its generation is failed with
  the error, so the view reports it and the next trigger starts over. A dead T2 fails only
  the vector index generation: the view stays active and searchable.
- **Leases:** a running job renews its lease every third of `jobs.lease`; a lost lease
  (cancelled, or expired and reclaimed) cancels the job (parsing included).
- **Crash recovery:** workers and `run_until_idle` call `reclaim_expired_leases` on start
  (and workers once per lease period). A reclaimed T0 resumes the building generation of
  the same commit; every write replaces the earlier attempt.
- **Concurrency:** a `Worker` runs `WorkerConfig::concurrency` jobs at once; T0, T1 and T3 of
  one view are serialized in process, and across processes by the store's single building
  generation per view; T2 runs of one view are serialized separately.

## Tier semantics

| Tier | Done means | Skipped | Failed |
|---|---|---|---|
| T0 | file text and paths of the generation are stored and lexically indexed | — | the build fails (no activation) |
| T1 | chunks, per-path inputs, symbols, definitions, references and syntactic edges of changed files (and dependents) are stored | — | the build fails |
| T3 | the relation stage ran and the generation is active | — | the relation stage failed (the generation is still activated) |
| T2 | every input of the active generation that could be embedded has a vector in the project's profile, and the vector index generation is active | `no_provider`, `data_policy_local_only` (nothing is ever sent), `budget_exhausted` | provider unusable after all retries, or configuration mismatch (model, dimensions, undefined provider) |

A T2 that is skipped or failed leaves an active, searchable generation: lexical, symbol and
graph search use it, while its vector index generation is not activated, so queries never
present stale vectors as current. The next build re-embeds everything that is missing (not
only its own changes) when the previous vector generation is incomplete. Inputs a provider
refuses as too long are dropped and counted (`inputs_rejected`), never truncated; they keep
`EmbeddingCoverage::embedded` below `inputs`.

`ViewStatus` reports, per view: track target, latest seen commit, active commit and
generation, building generation, the four tier states (of the build in progress, or of the
active generation), lag (time behind the target) and the last error. Progress events
(`ProgressEvent`: queued, up to date, tier changes, activated, superseded, failed, overlay
updated) go to a broadcast channel.

## Watching, reconciliation, overlays

- `Indexer::watch` starts one `knowell_source` watcher per git working tree. Ref and `HEAD`
  moves sync the views of that repository; saved files rebuild the personal overlay of
  `worktree` views; a rescan does both.
- Reconciliation (`IndexerConfig::reconcile_interval`, default 10 min, or
  `Indexer::reconcile`) re-resolves every target, compares the Merkle tree hash of the
  store's files of the active generation with the one recorded at build time (mismatch →
  forced rebuild), restores missing lexical indexes, and queues a missing T2. Plain
  directories are compared by tree hash on disk.
- `Indexer::build_overlay(view, worktree)` builds a personal layer in memory: the
  worktree's committed differences from the view plus saved, uncommitted changes, parsed
  and lexically indexed in RAM. `Overlay::shadowed_paths` lists the base paths it replaces.
  The shared store and lexical index are only read.

## On-disk layout

```text
<data_dir>/
  lexical/<view-id>/<generation>/   Tantivy index, one document per file (id = path)
                                    KNOWELL_COMPLETE marks a finished build
  views/<view-id>/manifest-<generation>.json
                                    path → git blob → content hash (or blob-based skip),
                                    content-policy hash, Merkle tree hash
```

Lexical indexes are built **copy-on-write**: generation `g` starts as a file copy of the
active generation's complete index (Tantivy segments are immutable), then deletes and adds
the changed documents, so the active index is never written while `g` builds. Without a
usable base it is rebuilt from the store's redacted text. After activation, directories
other than the active one and `Retention::lexical_previous` older ones are deleted
(retried later when Windows still has them mapped). Manifests are accelerators only: losing
them costs a full read of the next build.

## Limits and content policy

- `Limits::max_file_bytes` (1 MiB): larger files are skipped as too large, never truncated.
- `Limits::max_files_per_view` (200 000): a larger view fails its build with a message
  naming the limit; no silent subset is indexed.
- `ContentPolicy::excluded_dirs` (`vendor`, `node_modules`, `bower_components`): excluded
  by path before reading, on top of the built-in sensitive-file rules and project excludes.
- `GeneratedPolicy` for files `knowell-parse` flags as generated or minified: `Full`,
  `SkipEmbeddings` (default: text, symbols and chunks, no vectors) or `TextOnly`.
- `EmbeddingSettings`: batch size (64) and an optional engine-wide `Budget`; a batch that
  does not fit is not sent.
- `parse_parallelism` (4) blocking parse threads; `build_cache_bytes` (256 MiB) of parsed
  artifacts handed between stages, up to T2 (beyond it, later stages recompute).
- `concurrency` (2): jobs `index_workspace` runs at once in standalone mode
  (`WorkerConfig::concurrency` for workers). Each running T0 holds a Tantivy writer
  (about 50 MB).
- Reference, dependent and link bounds: see the sections above.

## Failure modes

| Failure | Behaviour |
|---|---|
| tracked ref missing or deleted | sync fails the view with the git error ("… no other ref is used in its place"); recorded durably as a failed generation; the last good generation keeps serving |
| worker crash | lease expires, job reclaimed, building generation resumed; a T2 lost between activation and queueing is queued by reconciliation |
| newer commit while building | the older build is superseded at its next stage (or by the newer T0) and never activates |
| newer generation while embedding | the older T2 stops before its next provider batch; vectors written so far are reused |
| late job of a superseded build | ends quietly; writes and activation are refused by the fence |
| provider outage | T2 retried with backoff; after the last attempt T2 is failed; the generation is active and searchable throughout, `embedding_coverage` reports partial coverage |
| budget exhausted / local-only + cloud provider / no provider | T2 skipped with the reason; nothing is sent |
| parse problems | never fatal: degraded files keep text and whatever symbols were found |
| link stage error or project above its file bound | T3 failed with the reason; the generation activates; link rows of changed files are dropped |
| store rows lost or lexical index deleted | reconciliation rebuilds |
| a job of a view the claiming process did not register | not claimed: it waits for a process that serves the view |

## Known limitations

- A renamed file is embedded again (its prepared input contains the path).
- References are name-based within a file, its imports and its directory; there is no
  semantic resolution. An unchanged file whose identifiers could match a *new* definition
  elsewhere is only re-resolved through an import of the new file, not by a project-wide
  search. More than 500 dependents in one build: the rest keep their edges until they change.
- Each exact-tier file is parsed twice in T1 (definitions, then identifiers) and once more
  by the link packs in T3.
- Link rows of other projects are refreshed only when those projects are rebuilt; a cold
  link cache re-extracts the other projects of the workspace from stored text.
- Line numbers refer to the redacted text; a redacted multi-line secret shifts later lines.
- Plain-directory projects are not watched (reconciliation only) and are re-read in full
  on every sync.
- Tier states of builds run by another process are reported as pending until they finish
  (T2 of an active generation is read from the store).

## Profile switches

The store records which embedding profile each view serves and which blue-green
switches are building (`knowell_store::switches`); that record, not the process, is the
truth. T2 builds the serving profile and, while the view belongs to a building switch,
the switch's target, each only with the configured embedder whose provider kind, model,
dimensions and input format match that profile (never a substitute). After a profile's
vectors activate, the switch activates if its target covers the active generation of
every member view: one transaction moves them all.

- `Indexer::start_switch`, `cancel_switch`, `rollback_switch`, `switch`, `switches` and
  `switch_progress` manage and report switches; starting one queues catch-up T2 jobs for
  the active generations the target does not cover.
- Registration only records the configured profile (the first one serves). Index runs
  (`index_workspace`) and reconciliation start a switch when the configured profile
  changed and resume building switches, so a restarted process completes them.
- A failed build of a switch target is retried by explicit requests (index runs,
  starting or rolling back a switch), never by the periodic reconciliation, so a
  permanent provider error is not retried in a loop.

## Running the tests

Unit tests need nothing. Integration tests need git and a PostgreSQL server with pgvector:
set `KNOWELL_TEST_DATABASE_URL` as described in the knowell-store README; without it each
test prints one skip line and passes (it fails under `KNOWELL_TEST_STRICT=1`).
Embeddings use a counting wrapper around
`FakeEmbedder` (optionally with simulated latency); no real provider is contacted.

```sh
python scripts/buildlock.py cargo test -p knowell-index
```

Lexical generation updates share recognized immutable Tantivy segment components
with hard links when the filesystem permits it, copying as a fallback. Metadata is
always copied, locks/completion markers are not inherited, and mutable metadata
never shares an inode between generations. Debug counters distinguish shared bytes
from copied bytes. This reduces repeated segment copying; it does not establish
billion-line capacity or eliminate compaction and retention costs.
