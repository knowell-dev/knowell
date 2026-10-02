# knowell-scip

Precise code intelligence for Knowell, from [SCIP](https://github.com/scip-code/scip)
indexes produced by each language's own indexer. This crate reads SCIP files (untrusted
input), maps them to Knowell types, answers cross-file queries, and runs the indexers.

## Supported indexers

| Language | Indexer | Detected by (project directory, top level) | Command (cwd = project dir) |
|---|---|---|---|
| Rust | `rust-analyzer` | `Cargo.toml` | `rust-analyzer scip . --output <out>` |
| TypeScript / JavaScript | `scip-typescript` | `tsconfig.json`, `package.json` | `scip-typescript index --output <out>` (+ `--infer-tsconfig` when there is no `tsconfig.json`) |
| Python | `scip-python` | `pyproject.toml`, `setup.cfg`, `setup.py` | `scip-python index . --output <out>` |
| Go | `scip-go` | `go.mod` | `scip-go --output <out>` |
| Java / Kotlin / Scala | `scip-java` | `pom.xml`, `build.gradle`, `build.gradle.kts` | `scip-java index --output <out>` |
| C# | `scip-dotnet` | `*.csproj`, `*.sln`, `*.slnx` | `scip-dotnet index --output <out>` |

A missing tool is never replaced silently: `run_indexer` returns
`IndexerError::NotInstalled { tool, install_hint }`. The caller reports "precise analysis
unavailable for <language>" and keeps its syntax-level results.

Indexers run without a shell, with a cleared environment that gets back only a short
allowlist of non-secret variables (`PATH`, home/temp directories, toolchain locations),
stdin/stdout closed, stderr kept as a truncated tail, and a kill on timeout. Note that
indexers execute project build tooling (for example Gradle, Maven, `cargo metadata`), so
only index projects you trust to build.

## Precise versus syntactic evidence

Knowell labels every relation with its evidence type:

* **precise (SCIP)**: definitions, references, implementations and relationships stated by
  the language's own compiler front end. Everything this crate returns is precise, and
  only for files whose index is current.
* **syntactic (tree-sitter)**: names and structure found by parsing. Used when no indexer is
  installed, when an indexer failed, or for files changed after the index was built.

Callers merge the two per file: a file with `Freshness::Fresh` uses SCIP occurrences; any
other file (`Stale`, `Unknown`, `NotIndexed`) falls back to syntactic evidence and is
reported as such. Semantic similarity is never presented as a SCIP relation.

## Freshness contract

An index is tied to the content it was built from.

* If the indexer embedded document text, the reader keeps its BLAKE3 hash
  (`Document::content_hash`).
* Otherwise the caller supplies a manifest of the hashes of the files at index time with
  `ScipIndex::apply_manifest` (existing embedded hashes are never overridden).
* `ScipIndex::coverage(path, current_hash)` returns `Fresh`, `Stale`, `Unknown` (no hash
  recorded) or `NotIndexed`; `covers(..)` is true only for `Fresh`.
* `ScipIndex::retain_documents` drops documents the caller found stale.
* The commit an index was built from is not part of the SCIP format; record it with
  `set_source_revision`.

## Conventions

* `Span` is exactly what SCIP states: 0-based lines and characters, end exclusive, in the
  document's `PositionEncoding`. `Occurrence::line_range` is the same range as 1-based
  inclusive `LineRange` (Knowell convention). `symbol_at(path, line, character)` takes
  0-based coordinates.
* Document paths are validated as `RepoPath` (relative, `/`-separated, no `..`); a document
  whose path escapes the project root is rejected and listed in `ScipIndex::report()`.
  Invalid ranges and oversize symbols are dropped and counted there too.
* `local N` symbols are scoped to their document; `find_local_symbol(path, moniker)`.
* Reading is bounded by `Limits` (bytes, documents, occurrences, symbols, documentation).
