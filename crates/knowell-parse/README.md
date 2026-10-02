# knowell-parse

Code analysis for Knowell: language detection, tree-sitter parsing, symbol and
import extraction, semantic chunking, embedding inputs and signature skeletons.
See `docs/ARCHITECTURE.md` §7 for where this sits in the pipeline.

```rust
use knowell_core::RepoPath;
use knowell_parse::{ChunkContext, ChunkOptions, chunks, parse, prepared_input, skeleton};

let path = RepoPath::new("src/billing/service.ts")?;
let parsed = parse(&path, &text);                    // never fails, never panics
for chunk in chunks(&parsed, &text, &ChunkOptions::default())? {
    let context = ChunkContext::for_chunk("billing-api", &parsed, &chunk);
    let input = prepared_input(&chunk, &context);    // embedding text + cache hash
}
let outline = skeleton(&parsed, &text)?;             // bodies elided
```

## Public API

| Item | Purpose |
|---|---|
| `parse(&RepoPath, &str) -> ParsedFile` | Analyse one file with default limits. |
| `parse_with(&RepoPath, &str, &ParseLimits, Option<&AtomicBool>) -> ParsedFile` | Explicit limits and cooperative cancellation. |
| `chunks(&ParsedFile, &str, &ChunkOptions) -> Result<Vec<Chunk>, ParseError>` | Meaningful units for embedding. |
| `prepared_input(&Chunk, &ChunkContext) -> PreparedInput` | Exact embedding input (`title`, `text`, `hash`). |
| `ChunkContext::for_chunk(project, &ParsedFile, &Chunk)` | Derives the container signature and doc for a chunk. |
| `skeleton(&ParsedFile, &str) -> Result<String, ParseError>` | Declarations with bodies elided. |
| `Language::detect(&RepoPath, &str)`, `Language::tier()` | Detection by file name, extension, shebang, `.h` sniffing. |
| `PARSER_VERSION`, `PREPARED_FORMAT_VERSION` | Cache-invalidation versions (see below). |
| `ts_language(Language) -> Option<tree_sitter::Language>` | The bundled grammar, for dependents running their own queries (rule packs); `None` for text-only languages and the Dockerfile. |
| `parse_tree(Language, &str, &ParseLimits) -> Option<tree_sitter::Tree>` | A raw tree under the same bounds as `parse` (`None` when skipped or out of time). |
| `pub use tree_sitter` | The tree-sitter crate the grammars are built for, so versions match. |

`ParsedFile` carries `symbols`, `imports`, `blocks` (SQL statements),
`dialect` (OpenAPI, AsyncAPI, Compose, Kubernetes), `is_generated`,
`has_errors`, `degraded`, `content_hash`, `byte_len` and `line_count`. It is
serde-serialisable. `chunks` and `skeleton` check (by length and content hash)
that they are given the text the file was parsed from and return
`ParseError::TextMismatch` otherwise.

`Symbol`: `name`, `qualified_name`, `kind`, `range` (lines, including leading
doc comments and attributes, like LSP's `DocumentSymbol.range`), `byte_range`,
`name_line`, `signature` (declaration without body or docs, ≤ 12 lines /
600 bytes), `doc` (markers stripped, ≤ 2000 bytes), `visibility`, `parent`
(index of the enclosing symbol) and `has_body`.

Qualified names are the container path within the file joined with `.`
(`SubscriptionService.cancelSubscription`); Markdown headings and JavaScript
test blocks join with ` > `, CSS rules with a space. Go receivers, C++
qualifiers (`Service::cancel`) and SQL `ALTER TABLE` targets become part of
the path. Packages (Java, Kotlin, Go, Scala) are not containers; block
namespaces and C# / PHP file-scoped namespaces are.

## Language matrix (as implemented)

All grammars are tree-sitter 0.27 crates (ABI 14 / 15) except the Dockerfile
scanner, which is hand-written. Queries live in `queries/<grammar>/` and are
compiled once per process.

### Exact tier (tree-sitter today; SCIP / language tools later)

| Language | Detection | Symbols | Imports | Visibility |
|---|---|---|---|---|
| Rust | `.rs` | functions, methods (impl / trait members), structs, unions, enums, traits, `impl` blocks (named after the type), inline modules, type aliases (incl. associated types), `const` / `static`, `macro_rules!`, struct fields | `use`, `extern crate`, `mod x;` | `pub`, `pub(crate/super/in)` → internal, trait and trait-impl members public; docs only from `///` / `/** */` |
| TypeScript, TSX | `.ts .mts .cts`, `.tsx`; `deno` / `ts-node` shebang | functions (incl. overload signatures and arrow / function expressions bound to module-level names), classes (incl. abstract), interfaces (+ member methods / properties), enums, type aliases, namespaces, `declare module`, methods, constructors, class fields, module-level `const`s, test blocks (`describe` / `it` / `test`, `.only` / `.skip`) | `import`, `export … from`, `import x = require()`, `require()`, `import()` | `export` → public; `private` / `protected` / `public` / `#name`; class members public; non-exported module members private |
| JavaScript, JSX | `.js .mjs .cjs`, `.jsx`; `node` / `bun` shebang | as TypeScript without types | as TypeScript | as TypeScript, except non-exported module members are unknown (`None`): CommonJS exports are assignments |
| Python | `.py .pyi .pyw`, shebang | functions, methods, constructors (`__init__`), classes (decorators in the signature), class attributes (fields), module-level `UPPER_CASE` constants | `import`, `from … import`, `__future__` | `_x` private, dunder public; docs from docstrings |
| Go | `.go` | functions, methods (receiver type in the qualified name: `Service.Cancel`), structs (+ fields), interfaces (+ method elements), named types and aliases, top-level `const` / `var` | import specs | capitalisation |
| Java | `.java` | classes, records, interfaces, annotation types, enums, methods, constructors, fields, interface constants | `import` (incl. `static`, wildcards) | keywords; default package-private → internal; interface members public |
| Kotlin | `.kt .kts` | classes, data classes, objects, interfaces, enum classes, functions (block and expression bodies), secondary constructors, type aliases, properties (top-level variables, member fields; companion members qualify under the class) | `import` (alias dropped) | keywords; default public |
| C# | `.cs .csx` | namespaces (block and file-scoped), classes, records, structs, interfaces, enums, delegates (type alias), methods, constructors, properties and fields | `using` (incl. `static`, `global`, aliases) | keywords; defaults: top-level internal, members private, interface members public; XML doc tags stripped |

### Structural tier

| Language | Detection | Symbols | Imports |
|---|---|---|---|
| Dart | `.dart` | classes, mixins (trait), extensions (impl), enums, typedefs, functions, methods (incl. getters, setters, operators), constructors (incl. factory / const / redirecting), fields; `_x` private | `import`, `export`, `part` |
| Swift | `.swift` | classes, actors, structs, enums, extensions (impl), protocols (interface) and their requirements, functions, initialisers, typealiases, properties; visibility keywords, default internal | `import` |
| PHP | `.php` | namespaces (block and file-scoped), classes, interfaces, traits, enums, functions, methods, constructors (`__construct`), class constants, properties; visibility keywords | `use` (incl. `function`, groups), `require` / `include` (`_once`) |
| Ruby | `.rb .rake .gemspec .ru`, `Gemfile`, `Rakefile`, … | modules, classes, methods (`def`, `def self.`), constructors (`initialize`), constants; no visibility (`private` sections are dynamic) | `require`, `require_relative`, `load` |
| C | `.c .h` (`.h` sniffed) | functions (definitions and prototypes, incl. pointer returns), structs, unions, enums, typedefs, `#define` macros, struct fields; `static` functions private | `#include` |
| C++ | `.cc .cpp .cxx .hpp .hh …` (and C++-looking `.h`) | as C, plus namespaces, classes, member declarations and inline definitions, out-of-line definitions (`Service::cancel` → method of `Service`), constructors, templates (template line in the signature), `using` aliases | `#include` |
| Scala | `.scala .sc` | classes, case classes, objects, traits, Scala 3 enums, `def` definitions and declarations, type definitions, `val` / `var` (fields in templates, top-level constants / variables) | `import` (with selectors) |
| Bash* | `.sh .bash .zsh .ksh`, `.bashrc` …, `sh` / `bash` / `zsh` shebang | functions | `source`, `.` |
| CSS* | `.css` | rule sets (named by selectors), `@media` / `@supports` / `@keyframes` and other block at-rules (nested rules qualify under them) | `@import` |

\* Additions beyond the architecture matrix (their grammars were already
declared); they would otherwise be text-only.

### Contract and structure files

| Format | Detection | Symbols | Imports / extra |
|---|---|---|---|
| SQL | `.sql .psql .pgsql .ddl` | tables + columns (`subscriptions.customer_id`), `ALTER TABLE … ADD COLUMN` columns, indexes, views (incl. materialized), functions, types | every top-level statement is a `Block` (with `subject`, e.g. the table) and a chunk unit; grammar: tree-sitter-sequel (generic SQL; unsupported dialect statements remain error regions and fall into leftover chunks) |
| Protocol Buffers | `.proto` | services, RPCs (`Service.Rpc`), messages (nested), enums, fields (incl. `map`, `oneof`), enum values | `import` |
| OpenAPI 3 / Swagger 2 | YAML / JSON with an `openapi` / `swagger` root key | endpoints named `GET /subscriptions/{id}` (signature `… (operationId: getSubscription)`, doc = summary or description), schemas (`components.schemas`, `definitions`), other top-level keys | `dialect = OpenApi` |
| AsyncAPI 2 / 3 | `asyncapi` root key | channels; v2 `publish` / `subscribe` → `PUBLISH channel`; v3 operations → `RECEIVE channel` (operationId in the signature); component schemas and messages | `dialect = AsyncApi` |
| Markdown | `.md .markdown .mdx .mkd` | ATX and setext headings; the symbol range is the section (to the next heading of the same or higher level); qualified `Guide > Setup > Linux` | — |
| YAML, JSON | `.yaml .yml`, `.json .jsonc …` | top-level keys and their direct children (≤ 200 per map), every YAML document | — |
| Compose | `compose*.y(a)ml`, `docker-compose*.y(a)ml` | services (signature includes the image), top-level keys | `dialect = Compose` |
| Kubernetes | `apiVersion` + `kind` root keys | one `Kind/name` resource per document | `dialect = Kubernetes` |
| TOML | `.toml`, `Cargo.lock`, `Pipfile`, … | tables `[a.b]`, array tables `[[bin]]`, keys (top-level and per table) | — |
| Dockerfile | `Dockerfile`, `Dockerfile.*`, `Containerfile`, `*.dockerfile` | build stages (named by `AS` alias or image; doc from comments above `FROM`) | base images and `COPY --from` images (stage references and `scratch` excluded); parser directives ignored |

### Text only

HTML, Vue, Svelte, XML, GraphQL, HCL / Terraform, Lua, Perl, R, Elixir,
Erlang, Haskell, OCaml, Clojure, Zig, Objective-C, PowerShell, Batch, INI,
Makefile, CMake, Groovy / Gradle and anything unrecognised (`text`): no
symbols, text chunker only. `tree-sitter-html` was declared in the workspace
but is not used (HTML is text-only in the matrix), so it is not a dependency
of this crate.

### Generated files

`is_generated` is set for generator banners in the first 40 lines / 8 KiB
(`@generated`, `DO NOT EDIT`, `Code generated … DO NOT EDIT.`,
`<auto-generated>`, protobuf / OpenAPI / Swagger generator banners, …),
lockfiles (`Cargo.lock`, `package-lock.json`, `go.sum`, …) and generated-file
suffixes (`.pb.go`, `_pb2.py`, `.g.dart`, `.min.js`, `.Designer.cs`, …).
Generated files are still analysed so that they can be linked to their source.

## Limits and degradation

Input is untrusted. `ParseLimits` (defaults): `max_bytes` 2 MiB,
`timeout` 5 s (parse and extraction, checked through tree-sitter progress
callbacks), `max_nesting` 1024 (bracket depth or indentation / 8, measured
before tree-sitter runs), `max_symbols` 20 000. When analysis is skipped or
partial, `ParsedFile::degraded` says why — it is never silent:

| Degradation | Effect |
|---|---|
| `TooLarge` | not parsed, no chunks |
| `Minified` (≥ 4 KiB, a ≥ 2000-byte line, ≥ 300 bytes per line on average) | not parsed, text chunks |
| `TooDeep` | not parsed, text chunks |
| `Timeout`, `Cancelled` | symbols found so far are kept; text chunks if none |
| `Truncated` | symbols beyond the limit dropped |
| `GrammarError` | internal bug (a query failed to compile) |

Syntax errors do not degrade: tree-sitter recovers, `has_errors` is set, and
symbols outside the broken region are kept. Unparsable regions still reach
chunks as leftovers. Tree walks are linear (no per-node parent / sibling
lookups) and nothing recurses on input nesting.

## Chunking

Sizes are in bytes of UTF-8 text (≈ 4 bytes per token for code):
`target_chars` 4000 (≈ 1000 tokens, minimum 256), `min_chars` 400,
`overlap_chars` 200 (text chunker only, at most a quarter of the target).

1. **Small file** (trimmed size ≤ `min_chars`): one `File` chunk.
2. **Units** are the top-level symbols (all kinds except fields, columns and
   SQL schema objects); for SQL, the statement blocks. A unit that fits is one
   chunk, members included, with its doc comments and attributes.
3. **Oversized container**: a header chunk — the container's own text with
   its largest members replaced by one-line elisions
   (`async step0(id: string): Promise<number> { … }`, Python `...`,
   Ruby `… end`, `…` in structure files) until it fits; smaller members stay
   inline. Elided members are chunked the same way, with `parent` = the
   header's ordinal. Oversized modules and namespaces are transparent: their
   members become top-level units.
4. **Oversized leaf**: split at line boundaries, preferring blank lines when
   that keeps at least half a target; lines longer than the target are split
   on character boundaries (preferring whitespace). Continuation pieces keep
   the symbol and point at the first piece.
5. **Grouping**: consecutive top-level units smaller than `min_chars`,
   separated only by whitespace, form one `Group` chunk (≤ target).
6. **Leftovers**: text outside every top-level unit (imports, top-level
   statements, unparsable regions) forms `TopLevel` chunks, emitted first.
7. **Text chunker** (text-only, minified, too-deep or unparsed files):
   paragraphs packed up to the target, a new chunk before a heading once the
   current one reaches `min_chars`, up to `overlap_chars` of trailing lines
   repeated at the start of the next chunk.
8. **Kinds**: `Test` for test functions and blocks (`#[test]`, `@Test`,
   `[Fact]`, `cfg(test)` modules, `test*` functions in test files, Go
   `Test*` / `Benchmark*` / `Fuzz*` / `Example*`, JS `describe` / `it`), then
   by symbol kind: `Function`, `Method`, `Class`, `Interface`, `Type`,
   `Module`, `Declaration`, `Endpoint`, `Statement`, `Section`, `Config`.

Every chunk is at most `target_chars`; output is deterministic.

## Embedding input

```text
path: src/billing/service.ts
language: typescript
project: billing-api
symbol: SubscriptionService.cancelSubscription (method)
container: export class SubscriptionService
doc: Subscription service.

<chunk text>
```

Absent fields are omitted (`kind: <kind>` replaces `symbol:` for chunks
without a symbol). `container` / `doc` describe the enclosing symbol — the
class for a method, the symbol itself for continuation pieces of a split
unit — summarised to one line. `title` is `path · symbol`. The hash is
BLAKE3 over `knowell.prepared`, `PARSER_VERSION`, `PREPARED_FORMAT_VERSION`,
the title and the text.

## Versioning policy

- `PARSER_VERSION` covers the analysis output: symbols, imports, blocks,
  chunks and skeletons. Bump it with any change that can alter them for the
  same input — a grammar or query update, an extraction or normalisation
  rule, a chunking rule or default. Stored analysis and embedding-cache
  entries keyed on it are then invalidated.
- `PREPARED_FORMAT_VERSION` covers the layout of `prepared_input` (header
  fields, order, separators). Bump it when that layout changes without the
  chunks changing.
- Both are part of every `PreparedInput::hash`, so a bump re-embeds exactly
  the affected inputs.

## Known limitations

- Visibility is syntactic: Ruby `private` sections, C++ access sections and
  JavaScript CommonJS exports are not resolved.
- tree-sitter-python tracks about 500 indentation levels; deeper code is
  misparsed (bounded, no panic).
- SQL uses one generic grammar; vendor-specific syntax (PL/pgSQL bodies,
  `DO` blocks, …) falls back to leftover chunks without schema symbols.
- Structure files list keys two levels deep; deeper configuration is
  reachable through chunks and lexical search.
