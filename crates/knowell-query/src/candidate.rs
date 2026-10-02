use std::fmt;

use knowell_core::{ContentHash, LineRange, Name, RepoPath};
use serde::{Deserialize, Serialize};

use crate::{Language, Location, QueryPlan, QueryScope, ViewId};

/// Which retrieval method produced a candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// Exact lookup: symbol table, paths, contracts, error codes.
    Exact,
    /// Lexical BM25 search.
    Lexical,
    /// Semantic vector search.
    Semantic,
}

impl SourceKind {
    /// Every source kind, in fusion order.
    pub const ALL: [SourceKind; 3] = [SourceKind::Exact, SourceKind::Lexical, SourceKind::Semantic];

    /// Stable lowercase label.
    pub fn label(self) -> &'static str {
        match self {
            SourceKind::Exact => "exact",
            SourceKind::Lexical => "lexical",
            SourceKind::Semantic => "semantic",
        }
    }
}

impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// What an exact match matched against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExactTarget {
    /// A symbol definition or occurrence.
    Symbol,
    /// A file path.
    Path,
    /// A contract node (endpoint, topic, RPC, table, env name, i18n key).
    Contract,
    /// An error code or error type.
    ErrorCode,
    /// A verbatim text occurrence (quoted phrase).
    Text,
}

/// Source-specific evidence for why a candidate matched.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum MatchDetail {
    /// An exact term matched.
    Exact {
        /// The planned term that matched.
        term: String,
        /// What it matched against.
        target: ExactTarget,
    },
    /// Query terms found by BM25.
    Lexical {
        /// The query terms present in the candidate.
        terms: Vec<String>,
    },
    /// Vector similarity. The similarity itself is the candidate's `raw_score`.
    Semantic {
        /// Embedding profile the vectors belong to; vectors from different
        /// profiles are never compared.
        profile: String,
    },
}

impl MatchDetail {
    /// The source kind this detail belongs to.
    pub fn kind(&self) -> SourceKind {
        match self {
            MatchDetail::Exact { .. } => SourceKind::Exact,
            MatchDetail::Lexical { .. } => SourceKind::Lexical,
            MatchDetail::Semantic { .. } => SourceKind::Semantic,
        }
    }
}

/// One ranked hit from one candidate source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    /// Source-stable id (e.g. a chunk id); used only as a final tie-break.
    pub id: String,
    /// Project of the file.
    pub project: Name,
    /// View the hit was read from.
    pub view: ViewId,
    /// Index generation of that view; must equal the pinned generation.
    pub generation: u64,
    /// Path relative to the project's source root.
    pub path: RepoPath,
    /// Line span; `None` for a file-level hit (e.g. a path match).
    pub range: Option<LineRange>,
    /// Hash of the file version (blob) the range refers to.
    pub content_hash: ContentHash,
    /// Enclosing or matched symbol, if known.
    pub symbol: Option<String>,
    /// Language of the file, if known.
    pub language: Option<Language>,
    /// Which source produced the hit; must match the list it is returned in.
    pub source: SourceKind,
    /// 1-based rank within the source's list (1 = best). Rank 0 is malformed.
    pub source_rank: u32,
    /// Source-native score (BM25, cosine similarity, …): finite, and only
    /// comparable within one source's list. Reported, never fused directly.
    pub raw_score: f64,
    /// Why the source matched it.
    pub detail: MatchDetail,
}

impl Candidate {
    /// The candidate's location.
    pub fn location(&self) -> Location {
        Location {
            project: self.project.clone(),
            path: self.path.clone(),
            range: self.range,
            view: self.view.clone(),
            generation: self.generation,
            content_hash: self.content_hash,
        }
    }

    /// Whether the candidate is internally consistent for a list of `kind`.
    pub(crate) fn is_well_formed(&self, kind: SourceKind) -> bool {
        self.source == kind
            && self.detail.kind() == kind
            && self.source_rank >= 1
            && self.raw_score.is_finite()
    }
}

/// Why a source could not answer. Messages must not contain secret values.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    /// The source exists but cannot serve this query (e.g. "provider not
    /// configured", "embeddings for generation 42 are still building").
    #[error("{0}")]
    Unavailable(String),
    /// The source failed while answering.
    #[error("failed: {0}")]
    Failed(String),
}

/// What every candidate source receives.
#[derive(Clone, Copy, Debug)]
pub struct SourceRequest<'a> {
    /// The query plan.
    pub plan: &'a QueryPlan,
    /// Scope and pinned manifest. Sources should search only pinned views;
    /// the engine re-checks every candidate anyway.
    pub scope: &'a QueryScope,
    /// Maximum candidates to return.
    pub limit: usize,
    /// Maximum candidates per project, so a large project cannot fill the
    /// list. The engine enforces it too.
    pub per_project_limit: Option<usize>,
}

/// Exact lookups: symbol table, paths, contracts, error codes.
pub trait ExactSource {
    /// Ranked candidates for the plan's exact terms.
    fn search_exact(&self, request: &SourceRequest<'_>) -> Result<Vec<Candidate>, SourceError>;
}

/// Lexical BM25 search (e.g. over Tantivy).
pub trait LexicalSource {
    /// Ranked candidates for [`QueryPlan::lexical_terms`].
    fn search_lexical(&self, request: &SourceRequest<'_>) -> Result<Vec<Candidate>, SourceError>;
}

/// Semantic vector search (e.g. over pgvector).
pub trait VectorSource {
    /// Ranked candidates for [`QueryPlan::semantic_text`].
    fn search_semantic(&self, request: &SourceRequest<'_>) -> Result<Vec<Candidate>, SourceError>;
}

/// The outcome of asking (or not asking) one source.
#[derive(Clone, Debug, PartialEq)]
pub enum SourceStatus {
    /// Not asked: its weight for this intent is zero, or the query is empty.
    NotConsulted,
    /// No source of this kind is configured.
    NotConfigured,
    /// The source answered (possibly with no candidates).
    Answered(Vec<Candidate>),
    /// The source was asked and could not answer.
    Failed(SourceError),
}

/// Candidate lists from every source, ready for fusion.
///
/// [`collect_candidates`](crate::collect_candidates) fills this from
/// synchronous sources; an async integration can fill it itself and call
/// [`search_with_candidates`](crate::search_with_candidates).
#[derive(Clone, Debug, PartialEq)]
pub struct SourceLists {
    /// Exact source outcome.
    pub exact: SourceStatus,
    /// Lexical source outcome.
    pub lexical: SourceStatus,
    /// Semantic source outcome.
    pub semantic: SourceStatus,
}

impl SourceLists {
    /// Lists where no source was consulted.
    pub fn not_consulted() -> Self {
        Self {
            exact: SourceStatus::NotConsulted,
            lexical: SourceStatus::NotConsulted,
            semantic: SourceStatus::NotConsulted,
        }
    }

    /// The outcome for `kind`.
    pub fn get(&self, kind: SourceKind) -> &SourceStatus {
        match kind {
            SourceKind::Exact => &self.exact,
            SourceKind::Lexical => &self.lexical,
            SourceKind::Semantic => &self.semantic,
        }
    }
}
