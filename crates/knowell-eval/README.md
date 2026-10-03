# knowell-eval

The measuring instrument for Knowell's core claim: *agents find the right code
faster than with grep alone, including across projects and for paraphrased or
Turkish queries.* That claim is only ever stated together with numbers produced
here.

The crate provides

1. a **deterministic synthetic multi-repo workspace** (the fixture),
2. a **graded query set** judged against that workspace,
3. a **retrieval interface**, a **grep baseline** and **metrics**,
4. **reports** that can be diffed against a baseline.

```rust
use knowell_eval::{FixtureSpec, GrepRetriever, QuerySet, Scale, generate, run};

let fixture = generate(&FixtureSpec { seed: 42, scale: Scale::Small });
let queries = QuerySet::builtin()?;
let corpus = fixture.to_corpus();
let grep = GrepRetriever::new(&corpus);
let report = run(&corpus, &queries, &[&grep], 10)?;
println!("{}", report.to_markdown());
```

## Measuring the engine from the CLI

`know eval run --retriever grep --retriever bm25 --retriever hybrid
--database-url env:KNOWELL_EVAL_DATABASE_URL` runs all three retrievers over the
same generated fixture and graded query set. The URL must be a secret reference
to a PostgreSQL 17/18 server with pgvector 0.8 or newer and a role with `CREATEDB`.
The command creates a uniquely named `knowell_eval_*` database, indexes all ten
projects with the real engine, then removes that database on success or error.
It never migrates or indexes into the database named in the supplied URL.
A killed process can leave its generated scratch database for manual cleanup.

Hybrid currently uses the deterministic, local `FakeEmbedder` at 64 dimensions,
with default engine chunking, intent weights and quotas. It makes no provider
requests. Indexing is serial to keep approximate vector index insertion stable;
the baseline measures relevance, not indexing speed or latency. Incomplete
embedding coverage and degraded searches fail the evaluation instead of being
reported as a successful hybrid measurement. `--bm25-coordination` is refused
alongside hybrid because it only configures the standalone BM25 retriever.

`eval/baselines/synthetic-small-hybrid.json` records seed 42, Small, depth 10
with grep, BM25 and hybrid. CI compares all three against this baseline. The
original lexical-only baseline is retained for the database-free CLI regression.
These deterministic embeddings test ranking integration; they do not establish
Gemini retrieval quality. Live 768/1536/3072 measurements remain pending.

Measured on Windows with PostgreSQL 17 and pgvector 0.8.7, 249 allowed documents
and 67 queries (61 ranked, 6 absent): hybrid Recall@10 is 0.4238, MRR@10 0.4923,
nDCG@10 0.3822; BM25 Recall@10 is 0.5992 and grep 0.5805. Hybrid answered all six
absent queries with results (abstention 0). This is a measured limitation of the
current deterministic pipeline, not a quality improvement claim. No weights,
quotas or chunk sizes were tuned to this evaluation set.

### Live Gemini evaluation

`know eval live --database-url env:KNOWELL_EVAL_DATABASE_URL
--api-key-ref env:GEMINI_API_KEY --dimensions 768 --max-tokens 500000
--json eval-live.json` explicitly sends the built-in Small fixture (seed 42)
through Gemini Embedding 2. It accepts only 768, 1536 or 3072 dimensions, never
an arbitrary workspace, model or API endpoint. Only the generated fixture's
resolved policy is changed to allow cloud embedding. The normal secret boundary
still excludes sensitive paths and redacts synthetic canaries before provider
calls. The API key is resolved from its reference and is never written to reports.

Indexing and queries share the token budget. Live calls conservatively count each
Gemini batch entry as one request quota unit, capped at 40 units and 10,000 estimated
tokens per minute. A batch holds at most 40 inputs and 8,192 estimated tokens.
Rate buckets initially hold one minute of capacity and refill continuously; these
settings constrain bursts as well as sustained traffic. They do not reserve provider
quota against other applications or bypass daily quotas. Provider retries and job retries
are disabled. The cap is based on token estimates before sending and provider usage
after success; it is not an exact provider billing ceiling. Each dimension may
account for at most 500,000 estimated tokens, approximately USD 0.10 at the
[standard text price of USD 0.20/million tokens](https://ai.google.dev/gemini-api/docs/pricing#gemini-embedding-2)
checked on 2026-10-03. All three dimensions therefore have a combined estimated
cap of USD 0.30 per workflow run. Provider billing remains authoritative.

Reports include model, requested dimensions, the actual request field
`requests[].outputDimensionality`, dimension-validation success, token accounting,
estimated cost, provider rate and batch caps, input count, database versions,
OS/architecture and cold-cache conditions. Every returned vector is checked against the
requested dimension; wrong dimensions, partial coverage, budgets and provider failures prevent a
successful report. The top-level dimension field is documented as deprecated in
the [API reference](https://ai.google.dev/api/embeddings). One synthetic 768-dimensional
request accepted it on 2026-10-03; complete engine measurements across all three
dimensions remain pending. No live relevance result has been recorded yet.

The Nightly `live-embeddings` matrix runs serially, only on this repository's
`main`, in the protected `live-embeddings` environment. The owner must provision
`GEMINI_API_KEY` as an environment secret; never add it as a repository secret or
paste it into chat. The build step does not receive the key. An explicitly enabled
`live_embeddings` workflow dispatch runs the experiment once. Scheduled runs
remain disabled until the owner sets `LIVE_EMBEDDINGS_ENABLED=true` after approving
recurring spending. Results are retained as dimension-specific artifacts for 30
days and summarized in the workflow run. No real-embedding baseline or ranking
tuning is justified until those measurements have completed.

## Why a synthetic fixture

Retrieval quality depends heavily on the code it runs on, and the only code
that may ever appear in this repository is code we wrote or that is openly
licensed. Private or company code is never used as a fixture, benchmark or
example. A generated workspace also gives us things a real repository cannot:
complete ground truth, planted defects with known answers, secret canaries we
can search for, and any size on demand.

## The fixture: Acme Goods

A fictional online shop with an "Acme Plus" membership. Ten projects, each its
own directory (and optionally its own git repository), wired together by real
contracts: REST endpoints, Kafka events, a gRPC service and a shared Postgres
database.

| Project | Language | Role | Core files |
|---|---|---|---:|
| `storefront-web` | TypeScript / React | website, API client, en/tr translations | 15 |
| `mobile-app` | Dart / Flutter | app with dio client, cancel screen, ARB translations | 12 |
| `billing-api` | TypeScript / NestJS | plans, subscriptions, checkout, JWT guard, event publisher | 19 |
| `orders-service` | Go | order history, Postgres repository, event consumers, gRPC client | 11 |
| `ledger-service` | Python / FastAPI | payment capture with idempotency keys, refunds, gRPC server | 14 |
| `notification-worker` | Rust | Kafka consumer, SMTP mailer, en/tr e-mail templates | 11 |
| `contracts` | protobuf / OpenAPI / JSON Schema | shared interfaces | 7 |
| `db-migrations` | SQL | migrations of the shared database | 11 |
| `infra` | YAML | docker compose, Kubernetes manifests, a committed `.env` | 6 |
| `handbook` | Markdown | ADRs (one in Turkish), runbook, glossary | 9 |

Same-named symbols across projects (e.g. `cancelSubscription` in the web
client, the mobile client and the billing service) make "which one?" questions
hard on purpose.

**Core files** (115) are hand-written and carry the semantics; they are
identical for every seed and scale. **Noise files** are generated per project
in its own language (catalog, inventory, shipping, coupons, analytics, …) and
contain distractor words — analytics code counting subscription
cancellations, retry and dedupe comments, Turkish comments — so lexical
matching is not trivially perfect. Noise never carries the semantics of a core
file and is never judged relevant; tests enforce that noise does not define
core symbols and that no file implements behaviour asked about by an `absent`
query.

| Scale | Files | Size |
|---|---:|---:|
| `small` | 250 | ~0.3 MB |
| `medium` | 2 500 | ~2.7 MB |
| `large` | 25 000 | ~27 MB |

Small generates in a few milliseconds (debug build), Large in about a second.

### Planted issues

`planted_issues()` lists defects with known answers, for future checks:

| Id | What |
|---|---|
| `schema-drift-cancel-reason` | migration 0009 adds `cancel_reason`; billing-api's entity maps it, ledger-service's read model of the same table does not |
| `contract-drift-resume-endpoint` | `POST /v1/subscriptions/:id/resume` is served and called but missing from the OpenAPI file |
| `committed-env-file` | `infra/.env` is tracked and holds a canary SMTP password |
| `hardcoded-access-key` | the Go order export job hard-codes a fake AWS-style access key id |

The two secret values are derived from the seed at generation time
(`KNOWELL_CANARY_<16 hex>` and `AKIA` + 16 letters) and are available through
`Fixture::canaries()`, so tests can assert that they never reach an index, a
log, an error message or a report. No real secret is ever involved.

### Writing to disk

`Fixture::write_to(dir, &WriteOptions { git })` writes one directory per
project plus a `knowell.toml` workspace file, and returns a `FixtureManifest`
(per-file BLAKE3 hash and size, per-project commit id). The output directory
must be new or empty.

With `git: true` every project becomes a repository with one commit on `main`.
The commit id is the same on every machine: author, committer and date are
fixed, the object format is SHA-1, and the user's system and global git
configuration, attribute and ignore files, templates and hooks are bypassed
(`GIT_CONFIG_NOSYSTEM`, `GIT_CONFIG_GLOBAL` pointing at the null device,
`--template=`, explicit `-c` overrides; `git add --force` so ignore rules
cannot drop `.env`). A missing `git` binary is reported before anything is
written.

### Determinism guarantees

- All text uses `\n`; output is byte-identical on Windows, Linux and macOS.
- Randomness comes from a SplitMix64 generator implemented in this crate, so
  neither the platform nor a dependency upgrade can change the output. Each
  project draws from its own stream derived from the seed and project name.
- Every collection is sorted before it is hashed or written.
- `Fixture::tree_hash()` (BLAKE3 over sorted project/path/content) for seed 42
  at Small is pinned by a golden test, and so are the ten commit ids. Changing
  core files, templates or vocabulary changes these values on purpose; update
  the pins in the same change and say so, because it invalidates baselines.

## The query set

`queries/acme-goods.toml` holds 67 graded queries (embedded in the crate and
loaded with `QuerySet::builtin()`):

| Kind | Count | Tests |
|---|---:|---|
| `symbol` | 10 | exact identifiers, including same-named ones |
| `behavior` | 14 | behaviour described in other words than the code uses |
| `cross_project` | 7 | flows spanning several repositories |
| `contract` | 9 | producers and consumers of endpoints, events, RPCs, tables |
| `config` | 7 | environment variables, compose, Kubernetes |
| `i18n` | 7 | translations and locale handling |
| `history_doc` | 7 | ADRs and runbooks ("why …?") |
| `absent` | 6 | behaviour that does not exist; the right answer is nothing |

40 are English and 27 Turkish (40 %). Several deliberately avoid the code's
vocabulary ("charging a customer twice" vs. idempotency keys; "oturum
anahtarı" vs. login token); their `notes` say "paraphrase".

```toml
[[query]]
id = "beh-prevent-double-charge"
lang = "en"            # en | tr
kind = "behavior"      # see the table above
text = "where do we prevent charging a customer twice for the same purchase?"
relevant = [
  { doc = "ledger-service/ledger/payments/idempotency.py", grade = 3 },
  { doc = "ledger-service/ledger/api/payments.py", grade = 2 },
]
notes = "Paraphrase: …"
```

Grades: **3** the answer, **2** directly involved, **1** useful context.

### Adding a query

1. Ask the question the way a developer or an agent would, in English or
   Turkish. Do not copy identifiers from the code unless the kind is `symbol`.
2. List every core file that answers it, with grades. Only hand-written core
   files qualify — never noise, never `infra/.env` (excluded by path, so it
   could never be retrieved).
3. For behaviour that does not exist use `kind = "absent"` and no `relevant`,
   and make sure nothing in the fixture implements it (extend the marker list
   in the `absent_behaviour_never_appears` test).
4. Run `python scripts/buildlock.py cargo test -p knowell-eval`. Loading
   checks unique slug ids, non-empty text, grades 1–3, well-formed and unique
   doc ids, `relevant` empty exactly for `absent`, and that every judged doc
   is a core file of the fixture at every scale.

Any change to the set changes its hash; reports made with the old set can no
longer be compared and baselines must be regenerated.

## Retrievers

```rust
pub trait Retriever {
    fn name(&self) -> &str;
    fn search(&self, query: &str, k: usize) -> Result<Vec<RankedDoc>, EvalError>;
}
```

Return at most `k` documents, best first, without duplicates (duplicates are
an error because they would inflate nDCG). Returning nothing is a valid answer.

### Grep baseline

`GrepRetriever` approximates what a coding agent does with `grep` today:
lowercase the query; split it into words (runs of letters, digits and `_`);
drop words shorter than three characters and English/Turkish stopwords and
question words; then, per document, count which query words occur as
case-insensitive substrings of `path + "\n" + text`. Documents are ranked by
the number of distinct matching words, then total occurrences, then id;
documents without any match are not returned.

## Metrics

All metrics are file-level and "higher is better". Ranking metrics are
computed for every non-`absent` query and macro-averaged:

- **Recall@k** (k = 1, 5, 10): fraction of the relevant documents (grade ≥ 1)
  that appear in the first k results.
- **MRR@10**: mean of `1 / rank` of the first relevant result, 0 when it is
  not in the top 10.
- **nDCG@10**: graded gain `2^grade − 1`, discount `log2(position + 1)`,
  normalised by the ideal ordering of the judgments.
- **Abstain rate**: over `absent` queries only, the fraction for which the
  retriever returned nothing. These queries are excluded from the ranking
  metrics.

Every metric is reported overall, per query kind and per language. A judged
document missing from the corpus (for example excluded by the file walker)
still counts as relevant and is listed in the report.

Read the abstain rate together with recall: a retriever that cannot match
Turkish words at all "abstains" on Turkish absent queries for the wrong
reason.

## Reports and comparisons

`run(corpus, queries, retrievers, depth)` returns a `Report` (depth ≥ 10). It
records what was measured — fixture name, seed, scale and tree hash, query-set
hash, corpus hash and size — next to the metrics and the top results of every
query. `to_json()` is stable (fixed field order, sorted maps, values rounded to
4 decimals); `to_markdown()` renders the tables.

`compare(current, baseline, tolerance)` lists regressions and improvements per
retriever, scope (overall, kind, language) and metric, plus retrievers that
appeared or disappeared. It refuses to compare reports of different fixtures
or query sets, and flags `corpus_changed` when the same fixture was walked
differently.

Grep baseline on Small (seed 42), for orientation:

| Retriever | R@1 | R@5 | R@10 | MRR@10 | nDCG@10 | Abstain |
|---|---:|---:|---:|---:|---:|---:|
| grep (all) | 0.1963 | 0.4798 | 0.5805 | 0.5940 | 0.5025 | 0.3333 |
| grep (en) | 0.2223 | 0.5601 | 0.6856 | 0.7266 | 0.5958 | 0.0000 |
| grep (tr) | 0.1563 | 0.3559 | 0.4184 | 0.3896 | 0.3586 | 0.6667 |

Print the full report with
`python scripts/buildlock.py cargo test -p knowell-eval --test end_to_end -- --ignored --nocapture`.

## Measured runs and secrets

`Fixture::to_corpus()` is the raw in-memory workspace for unit tests; it
applies no secret exclusion or redaction and therefore contains `infra/.env`
and the hard-coded key. Measured runs write the fixture to disk and build the
corpus through Knowell's real file walker, which excludes sensitive files by
path and redacts secrets before indexing; only that path represents what
Knowell actually searches.

## Limits

- Relevance is judged per **file**. Chunk-level judgments (line ranges) come
  later; until then a retriever that finds the right function in the right
  file and one that finds only the file score the same.
- One fictional domain, judged by one author. Judgments can be incomplete or
  biased toward the author's mental model; disagreements are fixed by editing
  the query file, which invalidates baselines by design.
- Noise is templated. It is realistic enough to compete lexically, but a
  learned model may find it easier to discount than real code.
- The grep baseline is an approximation of agent behaviour, not a replay of
  real agent sessions.
- The grep baseline scans every document per query word; on the Large scale
  use a release build.
