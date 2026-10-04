# knowell-store

PostgreSQL storage for Knowell: the single source of truth. PostgreSQL 17/18 with
optional pgvector 0.8 or newer, accessed through sqlx 0.9 on tokio. All queries are checked at run time
(`sqlx::query`, `query_as` + `FromRow`), so building the crate never needs a database;
the integration tests run every query against a real server instead.

## Running the tests

Unit tests need nothing. Integration tests need a PostgreSQL server with pgvector and an
**admin** connection URL in `KNOWELL_TEST_DATABASE_URL`. Every test creates its own
randomly named database (`knowell_test_<uuid>`), runs the migrations, and drops the
database afterwards, also when the test panics. Without the variable, each integration
test prints one `skipping …` line and passes (CI provides a service container).
Set `KNOWELL_TEST_STRICT=1` to make every such skip (a missing database URL or `git`)
fail instead, so a suite that never ran cannot pass; CI's database partitions and the
Docker test runner (`deploy/test/`) set it.

```sh
# Throwaway local server (the password is a test-only default, not a secret):
docker run -d --name knowell-test-pg -e POSTGRES_PASSWORD=knowell-test \
  -p 55432:5432 pgvector/pgvector:pg17

export KNOWELL_TEST_DATABASE_URL=postgres://postgres:knowell-test@localhost:55432/postgres
python scripts/buildlock.py cargo test -p knowell-store
```

`pgvector/pgvector:pg18` works the same way.

The optional-extension tests also need an unmodified PostgreSQL server (no pgvector
files) via `KNOWELL_TEST_PLAIN_DATABASE_URL`. CI supplies both servers. For example:

```sh
docker run -d --name knowell-test-plain-pg -e POSTGRES_PASSWORD=knowell-test \
  -p 55433:5432 postgres:17
export KNOWELL_TEST_PLAIN_DATABASE_URL=postgres://postgres:knowell-test@localhost:55433/postgres
python scripts/buildlock.py cargo test -p knowell-store --test integration migrations::
```

These tests cover concurrent and cancelled migrations, preserved legacy checksums
and vectors, enabling vectors on an existing core schema, and rejecting corrupt
migration history without retaining an advisory lock.
The engine's plain-server test covers lexical search, symbols, graph and memory with
configured providers and no embedding calls.

## Using it

```rust,ignore
// Initialization is an explicit administrator operation while runtimes are stopped.
let admin = Store::connect(&url /* SecretString */, &StoreOptions::default()).await?;
admin.migrate().await?;
admin.close().await;
let store = Store::connect_runtime(&url, &StoreOptions::default()).await?;
let info = store.check_server().await?;          // versions, pgvector, issues()

let mut conn = store.acquire().await?;
let view = views::create_view(&mut conn, project.id, &"branch:main".parse()?).await?;
let generation = views::begin_generation(&mut conn, view.id, Some(commit)).await?;
content::apply_file_changes(&mut conn, view.id, generation, &changes).await?;
views::activate_generation(&mut conn, view.id, generation).await?;
```

Repository functions are plain `async fn`s taking `&mut PgConnection`: pass a pooled
connection, or a transaction (`&mut tx`) to compose several calls atomically. Functions
that need several statements open their own transaction (a savepoint when nested).

`jobs::reclaim_expired_leases_scoped` uses the same view, workspace and optional
unscoped matching rules as `claim_scoped`. It leaves foreign expired leases unchanged;
the existing unscoped recovery function still recovers every expired running job.

`migrate()` creates core tables without requiring pgvector. If pgvector 0.8 or newer
is available, it also installs the extension and vector table; rerun it after adding
the extension files to enable semantic storage. Existing migration checksums and
data are preserved. Unknown checksums, dirty migrations and missing migration
versions remain errors. Migration selection and vector DDL share an advisory lock.
The historical SQL in `migrations/` is immutable; `core_migrations/` holds only the
two vector-free variants and the optional vector DDL.

Migration 14 makes `FileVersion.language` authoritative for each path and historical
interval. Identical body hashes at `.md`, `.rs` and `.ts` paths retain their own languages;
`Content.language` remains a legacy blob hint. New writes and same-hash no-op repairs
classify occurrence metadata inside the building-generation transaction. A rename
classifies its destination path. Language-filtered ANN queries use this occurrence
metadata before their result limit, including the legacy chunk-input fallback.
Unknown language stays SQL `NULL`, so explicit `languages=["text"]` does not admit it.
The lightweight workspace catalog presents unknown occurrences in its `text` count only.

Migration 15 adds optional source hierarchy beside content-level chunks without
rewriting chunk identities, prepared inputs or embeddings. `content::ChunkStructure`
retains the declaration and enclosing-symbol ranges, the parent chunk ordinal and
whether the embedding chunk is exact contiguous source. Elided headers and joined
regions are explicitly distinguishable from raw source. Existing indexes have no
hierarchy row until that content is analyzed again; absence never implies exact source.
`chunk_structures` reads selected keys and `chunk_structures_of` reads selected blobs,
both within the supplied tenant and without transferring source text.

`files_metadata_at_page` uses exclusive bytewise path cursors at a tenant's exact
generation; `files_metadata_in_paths` reads only selected paths. Both return file
occurrences, original byte sizes, text presence and optional redacted line counts.
New content records line counts during ingress. Legacy counts remain unknown: migration
15 deliberately does not load every stored body to backfill them. Missing referenced
content is corruption, not an empty file. `get_content_bounded` checks the exact UTF-8
redacted-body byte length inside PostgreSQL before transferring text; oversized bodies
produce an explicit error and are not silently truncated.

Before upgrading, stop all Knowell writers and older running processes. Mixed old and
new writers are unsupported: the migration advisory lock serializes schema upgrades,
but does not fence an old binary inserting unclassified rows after the backfill.
`migrate()` backfills every historical interval under that lock before returning, in
committed batches of at most `content::BATCH_ROWS`. Classification version 1 records
inconclusive results too, so subsequent startup does not reread unknown bodies. A
cancelled backfill resumes remaining version-0 rows on the next migration run.

Detection uses the existing parser's filename/extension and tenant-scoped redacted
source rules, never the shared blob hint. Conclusive paths require no body read;
`.h` refinement reads at most 65,536 UTF-8 characters, then the parser applies its
65,536-byte prefix boundary. Ambiguous paths use the complete first line for shebang
detection, one occurrence at a time. A pathological first line can therefore require
an allocation as large as that line, and PostgreSQL may scan the stored text to find
the newline; this is not a hard memory bound beyond upstream ingress limits. The line
is not silently truncated. Available text follows `Language::detect` exactly,
including `text` for unrecognized extensions. Missing ambiguous text produces
classified unknown. An explicit same-path/hash upsert can repair that unknown result
when its own tenant's redacted source becomes available, without changing the interval.
This metadata-only upgrade does not rewrite content, chunks, prepared inputs or vectors,
and does not call an embedding provider.
Runtime constructors (`connect_runtime`, `connect_runtime_with`) validate exact
schema 16, accepting the immutable original and core-only migration checksums.
Every physical pooled connection, including a reconnect, holds a database-wide
shared advisory admission lock until it closes. `validate_schema` never applies
DDL. `inspect_schema` accepts known incomplete history for administrator planning
(version zero for uninitialized storage), but still rejects corrupt/dirty/unknown
history. Runtime `migrate` and `begin_maintenance` calls are rejected explicitly.

An administrator starts or recovers an operation with `begin_maintenance(uuid)`.
Its detached `Maintenance` connection persists that UUID in the fixed
`_knowell_maintenance` protocol table before draining runtimes. New runtime
admissions and subsequent `Store::acquire`/`begin` operations report deliberate
maintenance. `acquire_exclusive(timeout)` proves all participating physical runtime
connections have closed before `migrate` can run. The guard uses its own connection
for schema inspection/migration, avoiding a shared-pool/exclusive-gate deadlock.
`finish` validates the exact schema and explicitly clears ownership. Dropping or
timing out the guard retains the UUID for recovery; no timestamp expires it.
`maintenance_owner` lets existing runtimes inspect the request through a bounded,
read-only control connection even when their pool is exhausted or new data
connections are refused. It does not admit data access or automatically stop an
engine; operators must close other hosts' active runtime pools before maintenance.

This is a cooperative application protocol. `connect`, `connect_with`,
`from_pool`, externally created raw SQL connections and old builds are
administrative/legacy access and do not carry runtime locks; bootstrap upgrades
must stop them explicitly. Direct `pool()` access on a runtime retains physical
connection fencing but bypasses the per-operation intent check. Engine entry
points use the runtime constructors and must close the pool after draining jobs,
requests, watchers and lexical handles. The application schema migrations remain
unchanged; the maintenance table is protocol infrastructure rather than a domain
schema version.

`check_server().supports_core()` reports PostgreSQL compatibility;
`semantic_enabled()` reports a supported installed extension. Use
`embeddings::available()` to check both the extension and vector table. Vector
operations return `StoreError::SemanticUnavailable` when that storage is absent.
Profiles and index-generation metadata remain core tables.

`embeddings::get_profile_in_organization` filters UUID lookups by tenant in SQL
before decoding metadata. Profile queries decode registration times through a
checked epoch projection: unsupported or infinite PostgreSQL timestamps return
`StoreError::Corrupt` without a panic or echoing the stored value.

| Module | Functions |
|---|---|
| `hierarchy` | organizations, workspaces, sources, projects (create / get / find / list / rename / delete) |
| `views` | views, generations (`begin_generation`, `activate_generation`, `fail_generation`, `prune_history`), manifests |
| `content` | content (`upsert_contents`, `get_content`, `redacted_texts` batched, `missing_contents`), content-level chunks (`upsert_chunks`, `chunks_of`, `locate_prepared_inputs`), per-path chunk inputs (`replace_chunk_inputs`, `chunk_inputs_at`, `locate_chunk_inputs`), file versions (`apply_file_changes`, `files_at`, `file_at`, `file_history`) |
| `symbols` | symbols (`upsert_symbols`, `get_symbol`, `find_symbols`, `rename_symbol`), occurrences (`replace_occurrences`, `occurrences_of`, `occurrences_in_file`, `definitions_in_paths`) |
| `graph` | edges (`replace_edges`, `walk_edges`, `edges_at`, `edges_into`, `edges_into_name_tails`, `edges_with_origins`), contracts (`replace_contracts`, `contract_parties`, `contracts_with_origins`) |
| `embeddings` | profiles, vectors (`upsert_embeddings`, `missing_embeddings`, `nearest`), index generations (`begin_index_generation`, `index_generation_at`, `activate_index_generation`, `active_index_generation`, ...), `input_coverage` |
| `jobs` | `enqueue`, `enqueue_scoped`, `claim`, `claim_scoped`, `heartbeat`, `complete`, `fail`, `cancel`, `reclaim_expired_leases`, `requeue_dead`, `delete_finished_jobs`, `get_job`, `find_job_by_key`, `job_counts`, `list_jobs`, `oldest_queued` |
| `knowledge` | `insert_record`, `update_record`, `get_record`, `delete_record`, `list_records`, `search_records`, `records_citing_files`, `records_about_symbols`, `record_versions`, `record_history` |
| `tasks` | `create_task`, `get_task`, `list_tasks`, `update_task`, `delete_task`, `append_checkpoint`, `list_checkpoints`, `latest_checkpoint` |
| `identity` | principals (`create_principal`, `get_principal`, `find_principal`, `list_principals`, `set_principal_disabled`, `delete_principal`), grants (`create_grant`, `list_grants`, `grants_for_principal`, `delete_grant`), API tokens (`insert_api_token`, `get_api_token`, `tokens_with_prefix`, `list_api_tokens`, `revoke_api_token`, `touch_api_token`, `delete_api_token`) |
| `audit` | `append_audit`, `list_audit`, `prune_audit_log` |

Graph lookups for incremental indexers: `graph::edges_into(pin, kind, targets)` (who
imports these files, who references these symbols), `graph::edges_into_name_tails(pin,
kind, project, tails)` (unresolved names whose last `/` segment is one of `tails`, for
re-resolving imports when a file appears), `graph::edges_with_origins` /
`contracts_with_origins` (what an analysis step wrote, to compare before replacing), and
`symbols::definitions_in_paths(pin, paths)`.

Listings are keyset-paged: each listing has a `*Cursor::after(&last_item)` to pass as
`before`, and limits are checked (`MAX_JOBS_LISTED`, `MAX_RECORDS_LISTED`, ...).

Errors (`StoreError`) never contain the connection URL or its password: URLs are
validated without being echoed, unknown URL parameters are rejected (the driver would
log them with their values), and connection errors are scrubbed.

## Schema

Migrations live in `migrations/` and are embedded with `sqlx::migrate!()`. They are
append-only; each file documents how to revert it by hand. Ids are UUIDv7 (generated by
`knowell_uuidv7()`, which works on PostgreSQL 17 and 18), timestamps are `timestamptz`,
hashes are BLAKE3 digests stored as 32-byte `bytea`.

| Table | Purpose |
|---|---|
| `organization` | Tenant and security boundary; nothing is shared across tenants. |
| `workspace` | Name unique per organization. |
| `source` | Git repository or directory; location unique per organization; never stores credentials. |
| `project` | Workspace + source + root path; name unique per workspace. Composite foreign keys keep workspace and source in the same organization. |
| `view` | One project following one track target (`branch:…`, `tag:…`, `worktree`, …): generation counter, active generation and commit, latest seen commit. |
| `view_generation` | One indexing run: commit, state `building/active/retired/failed`, error. At most one building and one active per view (unique partial indexes). |
| `view_manifest`, `view_manifest_entry` | Pins project → view generation + commit, for consistent multi-project queries and named release views. Pinned generations cannot be pruned. |
| `content` | Redacted text by (organization, hash): size, language. |
| `chunk` | (content, parser version, ordinal) → lines, bytes, kind, symbol path, prepared input hash (of the first path that wrote the content; see `chunk_input`). |
| `chunk_input` | (file version = view, path, `file_valid_from`; parser version; ordinal) → prepared input hash, `embed` flag. Per-path embedding inputs (migration 0010); cascades with its file version. |
| `file_version` | View path → content hash over a generation interval; `renamed_from` lets history follow renames. |
| `symbol` | Stable logical identity (project, kind, qualified name); renames keep the id. |
| `occurrence` | Symbol at a path and line range (definition/reference) over a generation interval. |
| `edge` | (kind, id, key) node triples, relation kind, `evidence_type`, `resolution`, evidence JSON, origin, generation interval. |
| `contract` | A project's producer/consumer participation in an endpoint/topic/rpc/table/env_name/i18n_key/package. |
| `embedding_profile` | Provider, model, dimensions (≤ 4000), input format version; immutable (update trigger). |
| `embedding` | (profile, prepared input hash) → untyped `halfvec`. |
| `index_generation` | Which view generation a profile's vectors cover: building/active/retired, counts. |
| `job` | Durable queue: kind, payload, priority, state, attempts, run_after, lease, idempotency key, last error; optional tenant (`organization_id`, `workspace_id`, migration 0007) and view (`view_id`, migration 0010; deleting the view deletes its jobs). |
| `task` | Caller-chosen id, organization, optional workspace and owner (user key), title, goal, status; notes, open questions, related files and the view manifest as JSON arrays; decisions (record ids), related symbols; `revision`. |
| `task_checkpoint` | (task, seq 1, 2, ...): time, summary, decisions, next steps, manifest pins (JSON array). Immutable. |
| `profile_switch`, `profile_switch_view` | Blue-green embedding profile switches (migration 0013): workspace, from/to profile, origin (request, configuration, rollback), requester label, state (building, active, cancelled, rolled_back), retention, member views. One switch builds per workspace. `switches::activate_switch` moves every member view in one transaction once the target's active vector index generation covers each view's active generation; `start_rollback` reverses an active switch within its retention. |
| `view_embedding` | The profile each view serves (its queries use), the profile its configuration named at the last registration, and the switch that set it (migration 0013). |
| `tool_usage_hour` | (organization, UTC hour, tool, agent label) → calls, errors, tokens returned, latency sum and a 96-bucket latency histogram, last call (migration 0012). `usage::record_tool_usage` adds sums in key order (concurrent flushers are safe); `usage::prune_tool_usage` drops old hours. |
| `checkpoint_receipt` | (organization, receipt id) → the checkpoint an idempotent save produced (migration 0011). The engine derives the id from the caller and their key; the key is not stored. Written in the save's transaction by `tasks::save_checkpoint`; immutable, deleted with its task. |
| `knowledge_record` | Caller-chosen id, organization, scope (`scope_kind` + workspace / project / task id or user key, canonical `scope_key`), kind, subject, current title / body / tags, state, content `version`, row `revision`, author (jsonb), pinned, related symbols, superseded_by, timestamps, generated `search` tsvector. |
| `knowledge_record_version` | (record, version): title, body, tags, when it became current. Immutable. |
| `knowledge_evidence` | (record, version, ordinal): project, view key, commit (7-64 hex), path, line range, content hash. Indexed by (project, path, content hash). Immutable. |
| `knowledge_history` | Per record, in order: time, actor (jsonb), action, from / to state, version, reason. Immutable. |
| `principal` | User or service account of one organization (id = knowell-auth `UserId` / `ServiceAccountId`), name, display name, `disabled_at`. Agents are not principals. |
| `access_grant` | Principal x role x scope (organization / workspace / project, by id). Named `access_grant` because `grant` is an SQL keyword. |
| `api_token` | Id (= knowell-auth `TokenId`), principal, optional agent client + session, lookup prefix, 32-byte keyed hash, scopes, label, created_by, expires / revoked / last used. Never plaintext. |
| `audit_log` | Append-only: organization (no foreign key), time, actor text, acting principal, action, resource, allowed, reason code, request id. |

### Generations, the fence and validity intervals

Generation-scoped rows (`file_version`, `occurrence`, `edge`, `contract`) carry
`valid_from` / `valid_to`: a row belongs to generation `g` when
`valid_from <= g AND (valid_to IS NULL OR valid_to > g)`. A new generation writes only
what changed; older generations stay readable until `prune_history`.

- `begin_generation` allocates the next number; only one generation per view builds at
  a time.
- Every generation-scoped write first takes a shared lock on its generation and checks
  that it is still building (the *write fence*); re-applying a write in the same
  generation replaces the earlier attempt, so retries are idempotent.
- `activate_generation` succeeds only when the generation is newer than the active one
  (a compare-and-set in one statement), so a late job of an old generation can never
  replace newer data. `fail_generation` rolls back every row the generation wrote.

### Per-path chunk inputs

The prepared embedding input of a chunk includes project and path context, so identical
content at two paths (or a file after a rename) has different inputs. `chunk` rows stay
content-level (one set per content and parser version: lines, bytes, kind, symbol path);
the input of each chunk **per file version** lives in `chunk_input` (migration 0010):

- `replace_chunk_inputs(pin, parser_version, paths, inputs)` sets the inputs of the file
  versions visible at `pin` for those paths (a path listed without inputs ends up with
  none). Each input names the content it was cut from, which must be the file's content at
  `pin`. Rows describe their file version, not the generation, so they are **not fenced**:
  they can be written while the generation builds or after it was activated (backfill), and
  they disappear with the file version (failed generation rollback, history pruning) through
  `ON DELETE CASCADE`.
- `chunk_inputs_at(pin, parser_version, paths?)` lists them; `embed` tells whether the
  content policy lets the chunk be embedded.
- `locate_chunk_inputs(org, pins, hashes)` goes from vector hits (prepared input hashes)
  back to every (view generation, path, line range) holding them; the returned chunk
  carries the per-path hash that was looked up. File versions without recorded inputs
  (data indexed before 0010) fall back to the content-level `chunk.prepared_input_hash`,
  as `locate_prepared_inputs` does.
- `embeddings::input_coverage(pin, profile, parser_version)` counts the inputs meant to be
  embedded and those with a vector in the profile (per path).
- The scoped `nearest` search matches hits through `chunk_input` (or, for file versions
  without inputs, through the content-level rows).

### Vectors

`halfvec` HNSW indexes need a fixed dimension (up to 4000; `vector` stops at 2000, so
3072 dimensions need `halfvec`). The `embedding` column has no fixed dimension and every
profile gets its own partial expression index, created by `register_profile`:

```sql
CREATE INDEX CONCURRENTLY embedding_hnsw_<profile> ON embedding
  USING hnsw ((embedding::halfvec(<dims>)) halfvec_cosine_ops)
  WHERE profile_id = '<profile id>';
```

`nearest` repeats exactly that expression and predicate (the profile id is inlined so
the planner can match the partial index), so vectors of different profiles are never
compared. It sets `hnsw.ef_search` per query and, when the search is restricted to
pinned view generations, `hnsw.iterative_scan = relaxed_order` so filtering does not
under-fill the result. A missing or invalid index is reported, never silently replaced
by a full scan.

Building a large HNSW index in parallel needs shared memory; Docker's default
`/dev/shm` (64 MB) is too small for that. Give the container more (`--shm-size`,
Compose `shm_size`) or set `max_parallel_maintenance_workers = 0` before a rebuild.
Indexes created at registration start empty and are not affected.

### Job queue

`enqueue` is idempotent by key; `claim` uses `FOR UPDATE SKIP LOCKED` ordered by
priority (larger first), then `run_after`; workers `heartbeat` to extend their lease and
end with `complete` or `fail` (exponential backoff until `max_attempts`, then `dead`);
`cancel` stops a job (its worker sees `LeaseLost`); `reclaim_expired_leases` returns
jobs of crashed workers to the queue. `find_job_by_key` looks a job up by its
idempotency key.

`claim_scoped(conn, worker, kinds, lease, &ClaimScope { views, workspaces,
include_unscoped })` claims only jobs of the given views (enqueued with
`JobScope::View`), view-less jobs of the given workspaces, and, if asked, unscoped jobs.
Organization-only jobs never match, and a view-scoped job of a listed workspace matches
only when its view is listed, so processes serving different views of one workspace
never take each other's jobs.

Listings: `list_jobs(conn, &JobFilter { states, kinds, scope, limit, before })` returns
jobs newest first (`created_at`, then id, descending). `enqueue_scoped(conn, &job,
JobScope::{Unscoped, Organization, Workspace, View})` attributes a job to a tenant (a
view resolves to its project's workspace and organization, and is recorded as the job's
view). `JobScopeFilter` selects one
organization and/or workspace; jobs without a tenant (enqueued with `enqueue`) match only
with `include_unscoped`, and a filter naming neither matches the whole queue.
`oldest_queued(conn, &scope)` is the creation time of the oldest job in state `queued`.

### Knowledge records

The domain model is `knowell-knowledge`; the store keeps rows and the engine maps them
(actors and task lists are jsonb in the domain's serialisation). The store does not depend
on the domain crate and does not scan for secrets: the domain does that before writing.

- **Versions.** `insert_record` stores the record row, its version's content
  (`knowledge_record_version`), that version's evidence and the history entries in one
  transaction. An `update_record` with `content: Some(...)` appends version `n + 1` with its
  evidence; stored versions, evidence and history rows are never updated (an update trigger
  rejects it). `record_versions` returns every version with its evidence, and
  `replaced_at` is the next version's creation time.
- **Optimistic concurrency.** `RecordUpdate` names the content `expected_version` and the
  row `expected_revision` it read; `revision` is bumped by every update, so two reviewers
  who both read revision 3 cannot both accept / reject (the loser gets
  `StoreError::Conflict` and nothing is written). Tasks use `expected_revision` the same
  way.
- **Scopes and tenancy.** Scopes reference workspaces, projects and tasks by id (renames
  keep records attached; deleting the scope deletes its records). A project scope stores
  its workspace too. Scopes, superseding records and evidence projects of another
  organization are reported as not found. `RecordFilter.scopes` matches exact scopes:
  pass every scope a caller may see (organization, its workspaces and projects, its tasks
  and its user key).
- **Search.** `search_records` uses the generated `search` column (`simple` configuration:
  no stemming, no stop words; title and subject weigh more than the body; subjects are
  split at dots) with `websearch_to_tsquery` syntax. The returned rank is `ts_rank_cd`,
  comparable only within one search; it is not BM25 and is not presented as such.
- **Staleness.** `records_citing_files(org, changes, states)` returns the records whose
  *current* version cites one of the (project, path, old content hash) triples, using the
  `(project_id, path, content_hash)` index; `records_about_symbols` matches related symbol
  ids (GIN index). The domain's `compute_staleness` then decides.

### Identity and audit

- `tokens_with_prefix(org, prefix)` returns every token record with the lookup prefix in
  the organization, including revoked and expired ones; the caller verifies the presented
  token in constant time (knowell-auth `verify`). A disabled principal's tokens report the
  disable time through `StoredApiToken::effective_revoked_at`, and `grants_for_principal`
  returns no grants for it.
- `touch_api_token(id, at, min_interval)` writes `last_used_at` only when the stored value
  is older than `min_interval`, so busy tokens cause one write per interval even across
  replicas.
- Agent tokens belong to the user they act for (`TokenAgent { client, session }`); the
  schema enforces the knowell-auth rules again (expiry within 24 hours, no admin scope).
- `audit_log` accepts only identifiers and codes: actor, action and resource are limited to
  `[A-Za-z0-9._:/@-]`, reasons to `[a-z0-9_]`, request ids to `[A-Za-z0-9._:-]`, so free
  text (and with it secrets) cannot be written. UPDATE, DELETE and TRUNCATE are rejected by
  triggers; `prune_audit_log(org, before)` is the retention path (it opens the guard for
  its own transaction only). Entries survive the deletion of their organization.

## Version-bound analysis

Schema 16 adds immutable, tenant/view/revision-bound prepared SCIP imports and
exact-generation per-file analysis coverage. Callers stage sanitized, source-free
prepared data; the indexer validates artifact, source and compiler-input identities
before the T1 transaction activates its generation. Coverage is not inherited by
unchanged text, is hidden for failed generations and is deleted by generation pruning.

Syntax and compiler occurrences have distinct origins. Retrying syntax analysis does
not erase imported occurrences; ordinary new analysis builds retire prior compiler
edges and occurrences unless an explicit matching import is attached. Existing
migration checksums remain unchanged. Runtime schema admission still requires the
exact current schema; applying this migration is an administrative operation.