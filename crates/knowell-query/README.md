# knowell-query

The query engine behind `search`, `build_context` and the panel's search
playground. It turns a question into ranked, explained, cited evidence and a
token-budgeted context pack.

The crate is **pure logic**. Exact lookups, BM25, vectors, the graph, rerankers
and file text are reached through small traits that the storage crates are
adapted to at integration time. It depends only on `knowell-core`, `serde`,
`serde_json` and `thiserror`.

## Pipeline

```text
                        ┌──────────────── QueryScope ────────────────┐
                        │ workspace · projects · languages · paths    │
                        │ domain · ViewManifest (pinned at start)     │
                        └──────────────────────┬─────────────────────┘
 query ──plan()──▶ QueryPlan                   │
   │  intent (rules, EN+TR)                    ▼
   │  exact terms, words      ┌──────── collect_candidates() ─────────┐
   │  glossary expansions     │ ExactSource   LexicalSource  VectorSource│  weight 0 → not asked
   │  [IntentClassifier]      │ missing / failing → `degraded`         │
   ▼                          └───────────────────┬───────────────────┘
                                                  ▼  SourceLists
                        ┌──────────── fusion (search_with_candidates) ────────────┐
                        │ 1 drop malformed · outside pinned views · scope filters │
                        │ 2 overlay shadows base-view paths                       │
                        │ 3 candidate quota per source × project                  │
                        │ 4 weighted RRF  Σ w_source / (k + rank)                 │
                        │ 5 merge overlapping duplicates (same content hash)      │
                        │ 6 result quota per project (work-conserving), limit     │
                        └───────────────────────────┬─────────────────────────────┘
                                                    ▼
                        7 [Reranker] short list only, off by default
                        8 GraphExpander: depth · edge kinds · fanout · node budget
                        9 explain: why[], score breakdown, coverage gaps,
                          empty-result reasons
                                                    ▼
                                             SearchResponse ──pack()──▶ ContextPack
                                                                 skeletons first, bodies by rank,
                                                                 no repeated text, citations,
                                                                 omitted[], uncertainties[]
```

```rust
let plan = knowell_query::plan(query, &glossary);
let sources = Sources { exact: Some(&symbols), lexical: Some(&bm25), semantic: None, ..Sources::default() };
let response = knowell_query::search(&plan, &scope, &sources, &SearchConfig::default())?;
// response.degraded == ["semantic: provider not configured"]
let pack = knowell_query::pack(&response, 8_000, &snippets);
```

Async integrations fetch candidates themselves, fill `SourceLists` and call
`search_with_candidates`; graph expansion, reranking and snippets are sync
traits (adapters can serve them from a prefetched snapshot or block on the
runtime).

## Principles in the API

- **Evidence, not invention.** Every result has a `Location` (project, view,
  generation, path, line range, content hash), the pinned commit, `why`
  reasons and a per-source score breakdown.
- **No silent fallback.** A missing or failing source, expander or reranker is
  listed in `degraded` as `"<component>: <reason>"`; nothing is substituted.
- **Absence is never claimed.** An empty answer lists `EmptyReason`s and the
  fixed note *"no evidence was found within the searched coverage; this does
  not show that the behaviour does not exist"*.
- **Pinned views.** Evidence from a view or generation not in the
  `ViewManifest` is dropped and counted, so a result is never half old, half new.
- **Deterministic.** Every sort ends in an explicit tie-break (finally
  `Location` order); glossaries sort their entries; the same input gives the
  same response and pack, byte for byte when serialised.

## Scope and manifest

`QueryScope { workspace, projects, languages, paths, domain, manifest }`.

| Check (in order) | Dropped as |
|---|---|
| project pinned in the manifest | `unpinned_project` |
| view is the pinned base or overlay view, generation equals the pinned one | `view_mismatch` |
| project filter | `project_filter` |
| language filter (unknown language is dropped when a filter is set) | `language_filter` |
| path filter: matches an include (if any) and no exclude | `path_filter` |
| base-view path changed in the user's overlay (candidate present in overlay, or listed in `OverlayPin::shadowed_paths`) | `shadowed` |

Path globs: `*` and `?` within a segment, `**` for any number of segments,
`dir/` for everything below `dir`, and a pattern without `/` matches the file
name at any depth. Matching is linear-time (no backtracking blow-up).

`search` rejects a scope whose workspace differs from its manifest, an overlay
that reuses its base view id, and a plan made for a different domain than the
scope (`QueryError::DomainMismatch`).

## Planner

`plan(query, &glossary)` / `plan_with(query, &glossary, &PlanOptions, Option<&dyn IntentClassifier>)`.

### Intent precedence

Every rule that fires is kept in `plan.signals` (intent, strength, rule id,
evidence). The intent is the one with the lowest tier; ties go to the earlier
signal. Other intents are listed in `plan.secondary`.

| Tier | Intent | Rules (rule ids) |
|---:|---|---|
| 0 | `error_trace` (strong) | `python-traceback`, `python-frame` (`File "x.py", line 3`), `stack-frame` (`at f (a.ts:1:2)`, `at a.b.C.d(C.java:42)`), `rust-panic`, `exception-header` (`Exception in thread`, `Caused by:`, `Uncaught`), `go-goroutine`, `go-frame`, `native-backtrace` (`#3 0x…`), `compiler-error` (`error[E0308]`), `error-line` (`Error: `, `fatal: `, `panic: ` …), `error-type-colon` (`TypeError:`), `exception-type` (`…Exception`), `error-code` (`E0308`, `TS2345`, `ERR_X`, `ECONNREFUSED`, `ORA-00942`) |
| 1 | `endpoint` (strong) | `http-method-route` (upper-case `GET`/`POST`/… + route), `route-shape` (`/api/v1/users/{id}`, `:id`, `<id>`), `url-route` (`https://host/path`) |
| 2 | `impact` | `impact-phrase` (*who calls/uses, used by, depends on, what breaks, if I/we change/remove/delete/rename, blast radius, kim kullanıyor, kim çağırıyor, ne bozulur*), `impact-word` (*impact(ed), affected, callers, usages, dependents, consumers, etki/etkisi/etkiler/etkilenir…, bozulur, bozar, kırılır, kullanan, kullanılıyor, çağıran, bağımlı*), `where-used` (*where … used/referenced*), `conditional-change` (Turkish *-rsem/-rsam/-rsek/-rsak*: *silersem, değiştirirsek*) |
| 3 | `why` | `why-word` (*why, rationale, reasoning, motivation, decided, adr, blame, neden, niye, niçin, gerekçe, tarihçe, sebebi*; *neden olan/olabilir* = "causing" is excluded), `why-phrase` (*the reason, reason for, design decision, history of, git history, who changed, when did/was, ne zaman, kim değiştirdi, karar verildi*) |
| 4 | `error_trace` (weak) | `error-word` (*error, exception, crash, panic, throws, fails, stack trace, hata/hatası/hataya…, istisna, çöküyor, patlıyor, fırlatıyor*) |
| 5 | `path_or_file` | `file-location` (`src/a.ts:42:7`), `path-shape` (contains `/` and: names a file, or ≥ 3 segments, or starts with a source dir such as `src/`, `crates/`, `./`), `file-name` (known source/config extension or `Dockerfile`, `Makefile`, …; `Node.js`-style names excluded) |
| 6 | `exact_symbol` (strong) | `camel-case`, `pascal-case` (≥ 2 capitals), `snake-case` (also `SCREAMING_CASE`), `qualified-name` (`Foo::bar`, `a.b.c`, `Class#method`), `call-shape` (`foo()`), `backtick-code` |
| 7 | `behavior` | `question-word` (*where, how, what, which, nerede, nasıl, hangi, ne, nedir, nereye, nereden, mi/mu*), `question-mark`, `natural-language` (≥ 3 content words) |
| 8 | `exact_symbol` (weak) | `short-keyword`: 1-2 ASCII words and nothing else fired (`checkout`) |
| — | `endpoint` (weak, informational) | `endpoint-word` (*endpoint, route, api, rota, uç nokta*) |
| default | `behavior` | no signal at all (e.g. a single Turkish business word) |

Turkish apostrophe suffixes are stripped from names (`PaymentService'i` →
`PaymentService`). Extracted outputs: `exact_terms` (kind: identifier,
qualified name, path, route, error code, error type, quoted phrase),
`words` (lower-cased, EN + TR stopwords removed), `trace_frames`
(path, line, column, symbol), `http_method`. Limits: 16 384 characters,
4 096 tokens, 64 exact terms, 128 words, 64 frames; `truncated` says when hit.

`QueryPlan::lexical_terms()` = exact terms + words + applied expansions
(de-duplicated by folded form). `QueryPlan::semantic_text()` = the query plus
applied expansions in parentheses.

**Classifier hook** (`IntentClassifier`, off unless passed): sees the rule
plan, may override the intent (`decided_by: classifier`, rule intent kept) or
abstain; a failure keeps the rule intent and adds `classifier: …` to
`plan.degraded`.

### Glossary

`Glossary::new(Vec<GlossaryEntry>)`; an entry links `term → expansion` with a
relation (`synonym`, `translation`, `abbreviation`, `code_name`), a status
(`approved` / `suggested`), an optional domain, and optional bidirectionality.

- Matching is on folded text (Unicode lowercase; `ç ğ ı İ ö ş ü â î û` → ASCII),
  phrases up to 4 words, longest first.
- A word also matches a term it starts with when the term has ≥ 4 characters
  and the suffix ≤ 6 (`ödemenin` → `ödeme`, `payments` → `payment`);
  `inflected: true` marks such matches.
- Only **approved** links expand by default; suggested ones are reported in
  `plan.suggestions` (`PlanOptions::include_suggested` applies them too).
- Domain entries apply only when `PlanOptions::domain` matches.
- One hop only; expansions already present in the query are skipped.

## Candidate sources

```rust
trait ExactSource   { fn search_exact(&self, r: &SourceRequest) -> Result<Vec<Candidate>, SourceError>; }
trait LexicalSource { fn search_lexical(&self, r: &SourceRequest) -> Result<Vec<Candidate>, SourceError>; }
trait VectorSource  { fn search_semantic(&self, r: &SourceRequest) -> Result<Vec<Candidate>, SourceError>; }
```

`SourceRequest { plan, scope, limit, per_project_limit }`.
`Candidate { id, project, view, generation, path, range, content_hash, symbol,
language, source, source_rank (1-based), raw_score (finite, source-native),
detail: Exact{term,target} | Lexical{terms} | Semantic{profile} }`.

| Situation | Response |
|---|---|
| source not configured | `degraded: "<kind>: source not configured"` (`"semantic: provider not configured"`) |
| `SourceError::Unavailable(reason)` | `degraded: "<kind>: <reason>"` |
| `SourceError::Failed(message)` | `degraded: "<kind>: failed: <message>"` |
| weight 0 for the intent, empty query, empty scope | not asked, not degraded |
| malformed candidate (kind mismatch, rank 0, NaN score) | dropped, counted |

## Fusion

Weighted reciprocal rank fusion over each result's best rank per source:

```text
fused(d) = Σ_source  weight[intent][source] / (k + rank_source(d))      k = 60 by default
```

Default weights — **starting points to be tuned by measurement** on the
evaluation set, not tuned values:

| Intent | exact | lexical | semantic |
|---|---:|---:|---:|
| exact_symbol | 1.0 | 0.8 | 0.3 |
| path_or_file | 1.0 | 0.7 | 0.2 |
| endpoint | 1.0 | 0.8 | 0.4 |
| error_trace | 0.9 | 1.0 | 0.4 |
| behavior | 0.4 | 0.6 | 1.0 |
| impact | 1.0 | 0.6 | 0.4 |
| why | 0.5 | 0.8 | 0.8 |

- **Order**: fused score ↓, number of contributing sources ↓, best single rank
  ↑, `Location` ↑.
- **Duplicates**: hits with the same `Location` are grouped; then a hit whose
  content hash equals a better-ranked result's and whose line range overlaps it
  is merged into that result (each source keeps its best rank, so agreement is
  rewarded without double counting). Merged locations are listed in
  `also_at` — including identical content in other paths or projects. A
  file-level hit (no range) only merges with other file-level hits.
- **Overlay**: a candidate from the user's worktree layer replaces base-view
  candidates for the same path; paths the overlay declares changed or deleted
  drop base-view evidence even without an overlay candidate.
- **Quotas**: per source, at most `candidate_quota_per_project` candidates per
  project enter fusion; in the result list each project keeps at most
  `result_quota_per_project` places before other projects' results —
  work-conserving: deferred results still fill the list when nothing else is left.

## Graph expansion

`GraphExpander::neighbors(&ExpandRequest { node, edges, limit, scope })`
returns `Neighbor { node, edge, evidence, resolution }`.

- Breadth-first from the top `seeds` results, up to `max_depth` hops, at most
  `fanout` neighbours per node (sorted by edge kind, evidence strength,
  resolution, location before truncation) and `node_budget` items in total.
- Edge kinds per intent (`caller`, `callee`, `type`, `test`, `contract`, `doc`):

  | Intent | Edges |
  |---|---|
  | exact_symbol | caller, callee, type, test |
  | path_or_file | test, doc |
  | endpoint | contract, caller, test |
  | error_trace | caller, test |
  | behavior | callee, test, doc |
  | impact | caller, contract, test |
  | why | doc |

- Each expanded item carries its `path` (edge kind, evidence type, resolution
  and node per hop) and `why` (`graph_path`; `test_references` for test edges).
  Evidence type and resolution are never collapsed into a number.
- A neighbour that already is a result annotates that result instead of being
  repeated; neighbours outside pins or scope are dropped and counted; a
  failing expander stops expansion and is reported once.
- Expanded item score = seed fused score × `score_decay` ^ depth (ordering only).

## Rerank

`Reranker::rerank(plan, items) -> Vec<f64>` reorders only the top `top_n`
(stable for ties). **Off by default.** Enabled without a reranker →
`rerank: reranker not configured`; wrong score count / non-finite score /
error → fused order kept and reported.

## Explainability

- `why`: `exact_match`, `lexical_terms`, `glossary_expansion`,
  `semantic_similarity` (similarity + embedding profile), `graph_path`,
  `test_references`, `personal_overlay`.
- `score`: `rrf_k`, per source `{rank, raw_score, weight, contribution}`,
  `fused`, optional `rerank {reranker, score}`.
- `searched`: views (project, view, generation, commit, layer), sources
  consulted and answered.
- `coverage_gaps`: `no_reference_resolution {project, language}` whenever the
  intent is impact or expansion follows call edges.
- `empty.reasons` (several can hold): `empty_query`, `unknown_project`,
  `project_not_indexed`, `no_projects_in_scope`, `sources_unavailable`,
  `no_reference_resolution_for_language` (impact queries),
  `no_candidates_in_selected_ref`, `all_filtered_by_scope`,
  `all_outside_pinned_views`, `all_shadowed_by_overlay`, `malformed_candidates`.
- `stats`: received per source, drops per reason, merged duplicates,
  deferred by quota, truncated by limit, expansion counters.

## Context packing

`pack(&response, budget_tokens, &dyn SnippetSource)` (≈ 4 chars/token) or
`pack_with(…, &dyn Tokenizer)`.

1. **Skeletons first** (signature, doc, outline) for every result, then every
   expanded item, in rank order.
2. **Bodies by rank**: each item is upgraded to its body when the remaining
   budget covers the net cost (body − its skeleton − other packed text the
   body contains, which is then not repeated and listed in `covers`).
3. Items that do not fit are skipped and listed in `omitted`
   (`over_budget {needed_tokens, remaining_tokens}`); smaller later items may
   still fit. A body partially overlapping another packed body is
   `duplicate_of`; a snippet from another file version is `stale_content`;
   `no_snippet`, `snippet_failed` likewise.
4. Each item's cost includes its citation label
   (`project@view#generation commit12 path:L1-L9 hash12`).
5. `uncertainties`: degradations, coverage gaps, weak graph evidence
   (heuristic / model suggestion / ambiguous / unresolved), expansion budget
   exhausted, more results than the limit, empty-result explanation.

Packed text is untrusted repository data; the MCP layer labels it as such.

## Knobs (`SearchConfig`, all `serde(default)`)

| Key | Default | Range | Meaning |
|---|---|---|---|
| `fusion.rrf_k` | 60 | u32 | RRF constant |
| `fusion.weights.<intent>.{exact,lexical,semantic}` | table above | finite, ≥ 0 | 0 = source not asked |
| `fusion.candidate_limit` | 100 | 1-10 000 | candidates requested per source |
| `fusion.candidate_quota_per_project` | 40 | ≥ 1 or unset | per source × project before fusion |
| `fusion.result_limit` | 20 | 1-1 000 | results returned |
| `fusion.result_quota_per_project` | 10 | ≥ 1 or unset | fair share before other projects (work-conserving) |
| `expansion.enabled` | true | | graph expansion on/off |
| `expansion.seeds` | 5 | | top results expanded |
| `expansion.max_depth` | 1 | 1-5 | hops |
| `expansion.node_budget` | 20 | ≤ 1 000 | expanded items in total |
| `expansion.fanout` | 10 | 1-1 000 | neighbours per node |
| `expansion.score_decay` | 0.5 | (0, 1] | per-hop score factor |
| `expansion.edges.<intent>` | table above | | edge kinds followed |
| `rerank.enabled` | false | | rerank the short list |
| `rerank.top_n` | 20 | 1-200 | short-list size |

Planner options: `PlanOptions { include_suggested: false, domain: None }`.
Packing: `budget_tokens`, `CharsPerToken { chars: 4 }` or any `Tokenizer`.

## Known limits

- Weights, quotas, decay and the planner vocabulary are unmeasured defaults;
  the evaluation harness decides their values.
- One list per source kind: projects on different embedding profiles must be
  merged by the vector adapter (or `SourceLists` grows per-profile lists).
- Result freshness tier (T0-T3) is implied by the contributing sources, not
  stated per result yet.
- The planner is rule-based by design; queries outside its vocabulary fall
  back to `behavior` (semantic-heavy) rather than guessing.
