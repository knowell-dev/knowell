# knowell-link

Cross-project contract linking for Knowell. Declarative **rule packs** extract contracts —
HTTP endpoints, event topics, RPCs, database tables, environment variable **names**, i18n keys
and infrastructure services — from code and contract documents; the **linker** turns the
extractions of every project of a workspace view into contract nodes and evidence-carrying
edges for [`knowell-graph`](../knowell-graph/README.md); **checks** turn gaps and drift into
CI findings (SARIF 2.1.0), with no model or API key involved.

```rust
use knowell_link::{CheckOptions, ExtractOptions, LinkOptions, PackSet, check, extract_project, link, to_sarif};

let packs = PackSet::builtin()?;                       // validated built-in packs (../../packs)
let mut projects = Vec::new();
for project in workspace_projects {
    projects.push(extract_project(&project.name, &project.paths, &packs,
        &ExtractOptions::default(), &mut |path| project.read(path)));
}
let linked = link(&projects, &LinkOptions::default())?;
for delta in linked.deltas(&generations, Some(&graph))? {   // or replacing_deltas(&previous, ..)
    graph.apply(delta)?;
}
let findings = check(&projects, &linked, &CheckOptions::default())?;
let sarif = to_sarif(&findings, env!("CARGO_PKG_VERSION"));
```

## Public API

| Item | Purpose |
|---|---|
| `PackSet::builtin() -> Result<PackSet, LinkError>` | The bundled packs, validated. |
| `Pack::load_dir(&Path)`, `Pack::from_files(label, &BTreeMap<String, String>)` | Load and validate one pack. |
| `PackSet::{new, insert, packs, get}` | Combine packs (names are unique). |
| `extract_project(&Name, &[RepoPath], &PackSet, &ExtractOptions, &mut dyn FnMut(&RepoPath) -> Option<String>) -> ProjectExtractions` | Run the active packs over one project. `read` is only called for paths that pass the secret exclusion policy. |
| `run_pack_on_file(&Pack, support: &[&Pack], &Name, &RepoPath, &str) -> PackRun` | Run one pack (forced active) over one file; used by pack tests. |
| `link(&[ProjectExtractions], &LinkOptions) -> Result<LinkOutput, LinkError>` | Link all projects of a view. |
| `LinkOutput::{deltas, replacing_deltas, graph, contracts, edges, sources, tables, entities}` | Graph deltas per project (fenced by generation), a fresh `CodeGraph`, table histories, ORM mappings. |
| `check(&[ProjectExtractions], &LinkOutput, &CheckOptions) -> Result<Vec<Finding>, LinkError>` | Graph insights + link checks, sorted. |
| `to_sarif(&[Finding], tool_version) -> serde_json::Value`, `fingerprint(&Finding)` | SARIF 2.1.0 log; stable partial fingerprints. |
| `normalize_key(ContractKind, &str) -> Option<String>` | Key normalisation for callers building contract ids from user input. |
| `CHECK_RULES`, `LINK_FORMAT_VERSION` | Finding code metadata; output format version. |

`Extraction` fields: `project`, `path`, `content_hash`, `range`, `kind` (`ContractKind`),
`role` (`producer`, `consumer`, `reads`, `writes`, `definition`), `key` (normalised),
`dynamic`, `symbol` (`qualified_name` + `range`), `evidence`, `pack` (`name@version`), `rule`,
`attrs` (`column`, `columns`, `entity`, `op`, `fields`, `version`, `operation`, `host`,
`locale`, `schema_hash`, `unresolved`, `glob`, ...).

## Graph conventions emitted

| Extraction | Edge |
|---|---|
| server route, RPC implementation | `symbol --Exposes--> endpoint / rpc` |
| client call | `symbol --Consumes--> endpoint / rpc` |
| publish / subscribe | `symbol --Produces--> topic`, `symbol --Consumes--> topic` |
| ORM mapping, SQL `FROM` / `JOIN` | `symbol --Reads--> table` (with `columns` and `schema_hash`) |
| SQL `INSERT` / `UPDATE` / `DELETE` | `symbol --Writes--> table` |
| env read, i18n use | `symbol --Reads--> env_name`, `symbol --References--> i18n_key` |
| OpenAPI / AsyncAPI / JSON Schema / proto / migration / locale file / Compose / Kubernetes | `file --Defines--> contract` (locale files carry `locale`) |

Source nodes are `NodeId::symbol(project, "<path>#<qualified name>")` (the knowell-parse
qualified name of the enclosing or captured symbol) or `NodeId::file(project, path)` for
file-level code and definitions. Contract nodes are `NodeId::contract(kind, key)`, shared by
all projects. Contract attributes: `schema_hash` (from the newest definition: OpenAPI
operation, proto request/response, JSON Schema, current table column set), `columns`
(tables), `fields` / `version` (event schemas), `external` (from `LinkOptions::external`),
`unresolved` (placeholders).

### Evidence and resolution

Evidence type and resolution are separate axes and never merged:

| Situation | Evidence | Resolution |
|---|---|---|
| contract document (OpenAPI, AsyncAPI, proto, JSON Schema, SQL migration) | `contract_derived` | `resolved` |
| locale entry, env / service declaration | `syntactic` | `resolved` |
| literal code key equal to a contract defined by a contract document | `contract_derived` | `resolved` |
| literal code key, no contract document | the rule's (`syntactic`) | `resolved` |
| heuristic rule, dynamic key (interpolation, unknown base URL), or a pattern / prefix / suffix match | `heuristic` | `resolved` (one candidate), `ambiguous` (several) |
| dynamic key without candidates, fully dynamic key | the rule's | `unresolved`, to a placeholder node with `unresolved = "true"` |

Unresolved uses are never dropped. Table readers carry `schema_hash` = the newest table
version they were *built against*: all their columns exist in it and every column added by
`ALTER TABLE` up to it is mapped. A read model that missed a later migration therefore
shows as `graph.contract_drift`.

### Key normalisation

| Kind | Canonical key |
|---|---|
| endpoint | `METHOD /path`: upper-case method (`*` = any; `ALL`/`ANY` -> `*`), scheme and host stripped (kept as `host`), query and fragment dropped, empty segments collapsed, no trailing slash. Declared parameters `:id`, `{id}`, `{id:guid}`, `<id>`, `<int:id>`, `[id]`, `*` -> `{}`; run-time interpolations `${x}`, `$x`, `\(x)`, `{x}` in f-strings -> `{}` and `dynamic`; a dynamic base URL is stripped (`dynamic`); a segment mixing text and a dynamic part is a pattern (`orders{}`). |
| topic, i18n key, infra | trimmed text; dynamic parts `{}` |
| env name | `[A-Za-z0-9_.-]` only, else rejected |
| table | quotes removed, lower-cased, default schema (`public`, `dbo`, `main`) dropped |
| rpc | `package.Service/Method`; code-side `Service/Method` is rewritten to the unique proto definition (heuristic) |

Matching: exact key first; endpoints then match segment-wise (a declared or dynamic `{}`
matches any segment; the most literal match wins; `*` methods match any method), then with a
`LinkOptions::path_prefixes` entry removed; other kinds glob-match `{}` against defined and
provided keys.

## Pack format

```
packs/<name>/
  pack.toml
  <language>/<query>.scm        # language = knowell-parse Language id
  tests/pos-*.<ext>             # >= 2 positive fixtures
  tests/neg-*.<ext>             # >= 1 negative fixture
  tests/<fixture>.expected      # expected extractions per fixture
```

Query lookup: a rule listing `tsx` looks in `tsx/` then `typescript/`; `javascript` in
`javascript/` then `typescript/`; `jsx` in `jsx/`, `javascript/`, `typescript/`; every other
language in its own directory. The query must compile for every language the rule lists.

### `pack.toml`

```toml
name = "nestjs"              # [a-z0-9-], unique
version = "1.0.0"            # MAJOR.MINOR.PATCH; recorded on every extraction (name@version)
description = "..."
languages = ["typescript", "tsx"]
limits = """Known limits, Markdown."""

[detect]                     # any match activates the pack; no [detect] = always active
npm = ["@nestjs/common"]     # also: go, python, pub, cargo, maven ("group:artifact"), nuget
imports = ["@nestjs/"]       # file-level: import specifier prefixes
files = ["**/*.proto"]       # project path globs
always = false

[[rules]]
id = "controller-route"
query = "routes.scm"
languages = ["typescript", "tsx"]
kind = "endpoint"            # endpoint | topic | rpc | table | env_name | i18n_key | package | infra
role = "producer"            # producer | consumer | reads | writes | definition
key = "{verb|upper} /{prefix}/{path}"
symbol = "handler"           # capture naming the symbol (default: enclosing symbol)
anchor = "route"             # capture used for the line range and de-duplication
evidence = "syntactic"       # syntactic | heuristic | contract (definitions only)
normalize = "http"           # default by kind: http, topic, env, i18n, table, rpc, plain
resolve = ["topic"]          # captures decoded as values: constants, lists, concatenation
key_shape = "dotted"         # optional: dotted | upper-snake (drop keys of another shape)
route = "nextjs-app"         # optional: nextjs-app | nextjs-pages (provides {route})
glob = "all"                 # optional: a `{}` key stands for every match (service registration)
postprocess = "sql"          # optional: the key is SQL text; tables are extracted from it
requires_rule = "model"      # optional: keep only if that rule matched the same symbol
files = ["**/*.controller.ts"]
exclude = ["**/*.spec.ts"]
require = ["service"]        # variables that must be non-empty
[rules.attrs]                # templates; empty results are omitted
handler = "{handler}"
[rules.lookup]               # variable = value bound to a capture's text (or "=literal")
prefix = { binding = "class-prefix", by = "class" }
[rules.defaults]             # templates used when a variable is empty
verb = "GET"
[rules.tokens]               # literal tokens replaced in the rendered key
"[controller]" = "{class|strip_suffix:Controller|lower}"
[rules.where]                # variable must equal the template
req = "{method}Request"
[rules.unless]               # drop when a variable matches the glob
method = "_*"
[rules.detect]               # rule-level hints (same keys as [detect])
npm = ["axios"]

[[bindings]]                 # name -> value tables used by lookups and constants
id = "class-prefix"
query = "class_prefix.scm"
languages = ["java", "kotlin"]
scope = "file"               # symbol | file | project
name = "class"               # capture holding the bound name
value = "{prefix}"           # template
resolve = ["prefix"]
constant = false             # true: also used to resolve identifiers in `resolve` captures

[[extractors]]               # structured (Rust) extractors for document formats
id = "openapi"
extractor = "openapi"        # openapi | asyncapi | event-schema | proto | sql-ddl | compose-env
files = ["**/*.yaml"]        #   | kubernetes-env | infra | locale-json | arb | prisma
```

Unknown keys are rejected. Loading validates the schema, ids, versions, languages (and that
they have a grammar), globs, that every query compiles, that every template variable is a
capture of the query (for every listed language), a lookup, a default or a built-in, that
`symbol`, `anchor`, `resolve`, `lookup.by` name existing captures, and that query strings do
not contain escapes tree-sitter would drop (`"\s"` becomes `s`; write `"\\s"`). Errors name
the pack, entry, file, language, row and column.

### Captures and templates

- Captures starting with `_` are predicate-only: they do not count toward the evidence range.
  Use tree-sitter predicates (`#eq?`, `#any-of?`, `#match?`, `#not-any-of?`) freely.
- A capture's value is its decoded text: string literals lose their quotes and escapes, and
  interpolations become dynamic parts. Captures listed in `resolve` are decoded as
  *expressions*: identifiers and member expressions resolve through constant bindings,
  `+` concatenations are joined, arrays / lists / composite literals fan out into one
  extraction per element. Anything not understood is dynamic, never guessed.
- Templates: `{var}`, `{var|filter|filter:arg}`, `{}` (an explicitly dynamic part), `{{` /
  `}}` for literal braces. Filters: `upper`, `lower`, `snake`, `plural`, `last_segment`,
  `trim`, `strip_prefix:X`, `strip_suffix:X`, `default:X`, `else:VAR`,
  `gotag:<tag>:<option>` (Go struct tags), `grpc_service` (`NewXClient`, `XStub`,
  `UnimplementedXServer`, ... -> `X`), `http_method` (`http.MethodPost`, `RequestMethod.PUT`
  -> `POST`, `PUT`).
- Built-in variables: `path.dir` (parent directory name), `path.stem`, `path.name`, `route`.
- Several matches with the same `anchor` node are merged keeping the one with the most
  captures, so optional parts are written as alternative patterns.

## Structured extractors

Written in Rust where structure, not code patterns, carries the meaning (and documented as
such): OpenAPI / Swagger (paths x methods, server base path, operation hashes with local
`$ref`s resolved, documentation keys ignored), AsyncAPI 2/3 channels, per-event JSON Schema
files, Protocol Buffers services (request / response structure hashes), SQL DDL (a tokenizer:
the bundled SQL grammar drops columns on valid PostgreSQL), docker-compose and Kubernetes env
**names** (values are never decoded or copied), compose / Kubernetes services and ports,
locale JSON and ARB files, Prisma schemas (no bundled grammar).

## Testing a pack

1. Add fixtures `tests/pos-<case>.<ext>` (at least two) and `tests/neg-<case>.<ext>` (at
   least one), each with `<fixture>.expected`.
2. `.expected` lists one line per extraction in `Extraction::describe()` form, in any order:
   `<role> <kind> <key>[ (dynamic)| (unresolved)] @ <symbol>|<file>[ {k=v, ...}]`, plus
   `bind <id> <name> = <value> | <value>` lines for the pack's bindings. `#` lines are
   comments; `# path: src/app/api/x/route.ts` gives the fixture a virtual repository path.
   Negative fixtures expect nothing.
3. `cargo test -p knowell-link --test packs` runs every fixture (with the `constants` pack as
   support) and checks the built-in set matches the directory.
4. Developer aids: `cargo test -p knowell-link --test packs print_pack_fixtures -- --ignored
   --nocapture` prints actual results; `cargo test -p knowell-link --test dump_trees --
   --ignored --nocapture` prints the tree-sitter trees (node kinds, field names) of every
   fixture.
5. After adding or removing a pack or query file, regenerate the embedded list with
   `cargo test -p knowell-link --test packs regenerate_builtin -- --ignored`
   (`embedded_file_list_is_current` fails until you do).

## Checks

| Code | Severity | Meaning |
|---|---|---|
| `link.migration_entity_mismatch` | error | an ORM entity misses a column a migration added (`ALTER TABLE ... ADD COLUMN`), maps a column no migration defines, or maps a dropped table; columns present at `CREATE TABLE` may be left out (read models) |
| `link.endpoint_undocumented` | warning | a service exposes an endpoint its OpenAPI document does not list (a document covers a service when they share an endpoint or the document is named after the project) |
| `link.endpoint_unimplemented` | warning | an OpenAPI document lists an endpoint its service does not serve |
| `link.consumer_field_mismatch` | warning | an event consumer's payload type (a struct / class named after the topic) has fields the event schema does not define (names compared case- and separator-insensitively) |
| `link.endpoint_without_provider` | info | an endpoint is called but no indexed project exposes or documents it |
| `link.unresolved_reference` | info | a key is only known at run time; the use is kept as unresolved |
| `graph.*` | per knowell-graph | contract drift, topics without producer / consumer, tables never read, endpoints without client, i18n keys undefined / missing in a locale / unused, env names read but declared nowhere |

Graph insights about placeholder nodes are replaced by `link.unresolved_reference`. Findings
are sorted by code, project, subject, location and message. `to_sarif` lists every code as a
rule, gives each result a physical location relative to its project root (`uriBaseId` = the
project name), related locations, and a `partialFingerprints` entry (`knowellFinding/v1`)
that ignores line numbers.

## Safety

- Paths rejected by `knowell_secrets::ExclusionPolicy` (`.env*`, keys, credentials, ...) are
  never read; they are listed in `ProjectExtractions::skipped`.
- Readable content is redacted with `knowell_secrets::scan` before analysis, keeping line
  numbers; constants and keys can therefore never carry a secret value.
- Environment variables are recorded by name only. Findings and errors contain identifiers,
  never source text or values.
- Parsing, queries and match counts are bounded (`ExtractOptions`); malformed input yields
  fewer extractions, never a panic.

## Limits

- No inter-procedural analysis: keys passed as function parameters are unresolved.
- Tables and i18n keys are workspace-wide names; migration order is file path order.
- `LinkOutput::replacing_deltas` removes stale edges but keeps contract nodes that no longer
  have edges (other views may use them).
- See [`packs/README.md`](../../packs/README.md) for per-pack limits.
