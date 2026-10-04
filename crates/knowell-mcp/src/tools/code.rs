//! `search`, `fetch` and `inspect_symbol`.

use knowell_core::{LineRange, Name};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    Validate, check_len, check_limit, check_range, check_text, check_unique, invalid, limits,
};
use crate::error::ToolError;
use crate::ids::ResultId;
use crate::model::{
    AnalysisLevel, Evidence, EvidenceType, FileLocator, Gap, MatchReason, RelationKind, Resolution,
    SymbolRef, Target,
};
use crate::text::UntrustedText;
use crate::tools::memory::MemoryRecord;

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

/// Input of `search`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchInput {
    /// Context or workspace to search.
    #[serde(flatten)]
    pub target: Target,
    /// What to look for: a question, words, a symbol, an error message or a path.
    #[schemars(length(min = 1, max = 2000))]
    #[schemars(description = "Question, words, symbol, error or path")]
    pub query: String,
    /// What to search (all kinds when empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub kinds: Vec<SearchKind>,
    /// Only these projects (all when empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub projects: Vec<Name>,
    /// Only paths starting with one of these prefixes, e.g. `src/payments/`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "e.g. src/payments/")]
    pub path_prefixes: Vec<String>,
    /// Only these languages, e.g. `typescript`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub languages: Vec<String>,
    /// Maximum hits (default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 100))]
    #[schemars(description = "default 10")]
    pub limit: Option<u32>,
    /// Whole source-mode response budget, in estimated tokens (default 4000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 256, max = 200000))]
    #[schemars(description = "default 4000; whole source response")]
    pub token_budget: Option<u32>,
    /// Include snippets of the matched lines (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "default true")]
    pub include_snippets: Option<bool>,
    /// Include measured retrieval counters and embedding usage (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "default false")]
    pub include_diagnostics: Option<bool>,
}

impl Validate for SearchInput {
    fn validate(&self) -> Result<(), ToolError> {
        self.target.validate()?;
        check_text("query", &self.query, limits::MAX_QUERY_CHARS)?;
        check_len("kinds", self.kinds.len(), limits::MAX_LIST_ITEMS)?;
        check_len("projects", self.projects.len(), limits::MAX_LIST_ITEMS)?;
        check_unique("projects", &self.projects)?;
        check_len(
            "path_prefixes",
            self.path_prefixes.len(),
            limits::MAX_LIST_ITEMS,
        )?;
        for prefix in &self.path_prefixes {
            check_text("path_prefixes", prefix, 1024)?;
            if prefix.starts_with('/')
                || prefix.contains('\\')
                || prefix.split('/').any(|p| p == "..")
            {
                return Err(invalid(
                    "`path_prefixes` must be relative, '/'-separated and must not contain '..'",
                ));
            }
        }
        check_len("languages", self.languages.len(), limits::MAX_LIST_ITEMS)?;
        for language in &self.languages {
            check_text("languages", language, 64)?;
        }
        check_limit(self.limit, 100)?;
        check_range(
            "token_budget",
            self.token_budget,
            limits::MIN_TOKEN_BUDGET,
            limits::MAX_TOKEN_BUDGET,
        )
    }
}

/// What `search` looks through.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SearchKind {
    /// Source code (chunks and symbols).
    Code,
    /// Documentation, ADRs, READMEs.
    Docs,
    /// Cross-project contracts.
    Contracts,
    /// Memory records.
    Memory,
}

/// Output of `search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchOutput {
    /// How the query was interpreted.
    pub query_class: QueryClass,
    /// Source hits, best first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hits: Vec<SearchHit>,
    /// Memory hits, best first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory_hits: Vec<MemoryHit>,
    /// More hits exist beyond `limit`.
    #[serde(default)]
    pub more_available: bool,
    /// Why the result is empty or incomplete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<Gap>,
    /// Opt-in measured counters; omitted in normal agent output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<SearchDiagnostics>,
    /// Requested and estimated source-response budget, when supplied by the
    /// engine. Source rendering also charges headers, fences, IDs and notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<super::TokenBudget>,
}

/// Measured work for one search. Counts are diagnostic, not quality scores.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchDiagnostics {
    /// Total tool time in whole milliseconds, including scope and source reads.
    pub elapsed_ms: u64,
    /// Snapshot/index preparation time in whole milliseconds.
    pub preparation_ms: u64,
    /// Pinned project views prepared for this search.
    pub prepared_views: u64,
    /// Exact-source lookup time in whole milliseconds.
    pub exact_ms: u64,
    /// Lexical retrieval and refill time in whole milliseconds.
    pub lexical_ms: u64,
    /// Embedding and ANN retrieval/refill time in whole milliseconds.
    pub semantic_ms: u64,
    /// Fusion and graph expansion time in whole milliseconds.
    pub fusion_expansion_ms: u64,
    /// Text acquisition for shown snippets in whole milliseconds.
    pub snippet_read_ms: u64,
    /// Lexical index probes, including bounded refill attempts.
    pub lexical_queries: u64,
    /// File hits examined across lexical probes, including repeated hits.
    pub lexical_file_hits: u64,
    /// Lexical span candidates generated across probes.
    pub lexical_spans: u64,
    /// ANN probes across profiles and refill attempts.
    pub semantic_queries: u64,
    /// ANN neighbors examined across probes.
    pub semantic_neighbors: u64,
    /// Query embedding operations started, including failed operations.
    pub embedding_calls: u64,
    /// Query embedding operations that failed; their usage is unknown.
    pub embedding_failures: u64,
    /// A found exact path allowed the query embedding to be skipped.
    pub exact_path_embedding_bypassed: bool,
    /// Usage returned by successful query embeddings, absent if none ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<QueryEmbeddingUsage>,
}

/// Query embedding usage; provider reports and estimates remain distinguishable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QueryEmbeddingUsage {
    /// Input tokens reported by the provider or estimated when unavailable.
    pub input_tokens: u64,
    /// Whether any input token count was estimated.
    pub tokens_estimated: bool,
    /// Initial requests, excluding retries.
    pub requests: u32,
    /// Additional attempts.
    pub retries: u32,
    /// Provider operation time in whole milliseconds; includes queue/backoff.
    pub operation_ms: u64,
}

/// How a query was classified before searching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueryClass {
    /// Names a symbol.
    ExactSymbol,
    /// Names a path.
    Path,
    /// Names an endpoint, topic or other contract.
    Contract,
    /// An error message or stack trace.
    ErrorTrace,
    /// Asks how something behaves.
    Behavior,
    /// Asks what a change affects.
    Impact,
    /// Asks why something is the way it is.
    Rationale,
}

/// One source hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchHit {
    /// Stable id; pass to fetch.
    pub id: ResultId,
    /// What was hit.
    pub kind: HitKind,
    /// Symbol name, heading or contract key.
    pub title: String,
    /// Where it is and why it matched.
    pub evidence: Evidence,
    /// The matched lines (untrusted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<UntrustedText>,
    /// Actual source lines displayed in `snippet`, not the full fetch range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet_lines: Option<LineRange>,
    /// Fetchable identity of the actual displayed source range, when it differs
    /// from the original hit's range. The original hit `id` remains unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet_id: Option<ResultId>,
    /// Whether the displayed snippet is shorter than the full evidence range.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub snippet_truncated: bool,
    /// Exact pinned ranges adjacent to the displayed source that can be read
    /// with `fetch`. Fetching the displayed snippet ID alone does not advance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub continuation_ids: Vec<ResultId>,
}

/// Kind of a source hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HitKind {
    /// A code chunk.
    Code,
    /// A symbol definition.
    Symbol,
    /// A test.
    Test,
    /// A documentation section.
    Doc,
    /// A contract definition or use.
    Contract,
    /// A configuration file section.
    Config,
}

/// One memory hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MemoryHit {
    /// The record (read more with read_memory).
    pub record: MemoryRecord,
    /// Why it matched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub why: Vec<MatchReason>,
}

// ---------------------------------------------------------------------------
// fetch
// ---------------------------------------------------------------------------

/// Input of `fetch`. Pass `ids`, `paths`, or both (at most 20 in total).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FetchInput {
    /// Context or workspace; paths are read at its pinned views.
    #[serde(flatten)]
    pub target: Target,
    /// Result ids from other tools.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub ids: Vec<ResultId>,
    /// Files or line ranges to read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub paths: Vec<FileLocator>,
    /// Extra lines before and after each range (default 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(max = 200))]
    #[schemars(description = "Extra lines around each range (default 0)")]
    pub context_lines: Option<u32>,
}

impl Validate for FetchInput {
    fn validate(&self) -> Result<(), ToolError> {
        self.target.validate()?;
        let total = self.ids.len().saturating_add(self.paths.len());
        if total == 0 {
            return Err(invalid("pass at least one of `ids` or `paths`"));
        }
        check_len("ids and paths", total, limits::MAX_FETCH_ITEMS)?;
        check_unique("ids", &self.ids)?;
        check_range(
            "context_lines",
            self.context_lines,
            0,
            limits::MAX_CONTEXT_LINES,
        )
    }
}

/// Output of `fetch`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FetchOutput {
    /// Fetched items, in request order (ids first, then paths).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<FetchedItem>,
    /// Requested items that could not be returned, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<Gap>,
}

/// One fetched range of a file version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FetchedItem {
    /// Stable id of exactly this range and version.
    pub id: ResultId,
    /// Exact version and lines.
    pub evidence: Evidence,
    /// Language, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// The lines (untrusted).
    pub content: UntrustedText,
    /// The content was cut to stay within limits.
    #[serde(default)]
    pub truncated: bool,
    /// Whether this version is still what the context's view contains.
    pub status: VersionStatus,
    /// Id of the same range in the context's current view, when it changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_id: Option<ResultId>,
    /// Exact remaining ranges of the requested pinned source, when limited.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub continuation_ids: Vec<ResultId>,
}

/// Whether a fetched version is still current in the context's view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VersionStatus {
    /// Unchanged in the context's view.
    Current,
    /// The file changed in the context's view since this id was issued.
    Changed,
    /// The file no longer exists in the context's view.
    Deleted,
}

// ---------------------------------------------------------------------------
// inspect_symbol
// ---------------------------------------------------------------------------

/// Input of `inspect_symbol`. Pass `id` or `symbol`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InspectSymbolInput {
    /// Context or workspace.
    #[serde(flatten)]
    pub target: Target,
    /// The symbol to inspect.
    #[serde(flatten)]
    pub symbol: SymbolRef,
    /// Facets to include (all when empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub include: Vec<SymbolFacet>,
    /// Maximum references, implementations and tests each (default 20).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 200))]
    #[schemars(description = "Per facet (default 20)")]
    pub limit: Option<u32>,
}

impl Validate for InspectSymbolInput {
    fn validate(&self) -> Result<(), ToolError> {
        self.target.validate()?;
        self.symbol.validate()?;
        check_len("include", self.include.len(), limits::MAX_LIST_ITEMS)?;
        check_limit(self.limit, limits::MAX_LIMIT)
    }
}

/// Parts of a symbol `inspect_symbol` can return.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SymbolFacet {
    /// Signature and doc comment.
    Signature,
    /// References (callers and other uses).
    References,
    /// Implementations and overrides.
    Implementations,
    /// Tests that exercise the symbol.
    Tests,
}

/// Output of `inspect_symbol`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InspectSymbolOutput {
    /// Matching symbols; several when the name is ambiguous.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub symbols: Vec<SymbolInfo>,
    /// Why the result is empty or incomplete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<Gap>,
}

/// One symbol with its facets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SymbolInfo {
    /// Stable id of the definition; pass to fetch.
    pub id: ResultId,
    /// Short name.
    pub name: String,
    /// Qualified name, e.g. `PaymentService.cancelSubscription`.
    pub qualified_name: String,
    /// Kind of symbol.
    pub kind: SymbolKind,
    /// Language.
    pub language: String,
    /// How deeply the language is analysed (decides reference quality).
    pub analysis: AnalysisLevel,
    /// Where it is defined.
    pub definition: Evidence,
    /// Signature (untrusted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<UntrustedText>,
    /// Doc comment (untrusted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<UntrustedText>,
    /// References to the symbol.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<SymbolLink>,
    /// Implementations and overrides.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub implementations: Vec<SymbolLink>,
    /// Tests that exercise it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<SymbolLink>,
    /// Whether `references` lists every known reference (not cut by `limit`, no unresolved ones).
    #[serde(default)]
    pub references_complete: bool,
}

/// Kind of a symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    /// Free function.
    Function,
    /// Method.
    Method,
    /// Class.
    Class,
    /// Interface or protocol.
    Interface,
    /// Trait.
    Trait,
    /// Struct or record.
    Struct,
    /// Enum.
    Enum,
    /// Type alias or type definition.
    Type,
    /// Constant.
    Constant,
    /// Variable or field.
    Variable,
    /// Module, namespace or package.
    Module,
    /// HTTP endpoint handler.
    Endpoint,
    /// Test.
    Test,
    /// Anything else.
    Other,
}

/// A reference, implementation or test of a symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SymbolLink {
    /// Stable id of the referencing code; pass to fetch.
    pub id: ResultId,
    /// Relation to the inspected symbol.
    pub relation: RelationKind,
    /// How the relation is known.
    pub evidence_type: EvidenceType,
    /// Whether it resolved to exactly this symbol.
    pub resolution: Resolution,
    /// Where the reference is.
    pub evidence: Evidence,
}
