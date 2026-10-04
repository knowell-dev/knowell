//! Query engine: plans a query, gathers candidates from exact, lexical,
//! semantic and graph sources, fuses them, explains every score, and packs
//! budgeted, cited context for agents.
//!
//! This crate is pure logic. Storage, BM25, vectors, graph and snippets are
//! reached through small traits ([`ExactSource`], [`LexicalSource`],
//! [`VectorSource`], [`GraphExpander`], [`Reranker`], [`SnippetSource`],
//! [`IntentClassifier`]) that the storage crates are adapted to.
//!
//! ```text
//! query ──plan()──▶ QueryPlan ──search()──────────────────────────────────▶ SearchResponse ──pack()──▶ ContextPack
//!          │                    │ 1 scope + view manifest pinned           │                         │ skeletons first,
//!          │ intent rules       │ 2 exact / lexical / semantic sources     │ results + why           │ bodies by rank,
//!          │ exact terms        │ 3 drop: malformed, unpinned, filtered,   │ score breakdown         │ budget, citations,
//!          │ glossary           │   shadowed by overlay, over quota        │ expanded items          │ omitted,
//!          │ (classifier hook)  │ 4 weighted RRF + overlap de-duplication  │ degraded / coverage     │ uncertainties
//!          │                    │ 5 rerank short list (optional, off)      │ empty explanation       │
//!          │                    │ 6 result quota per project, limit        │                         │
//!          │                    │ 7 graph expansion (bounded)              │                         │
//! ```
//!
//! [`pack_task`] defaults to query-sensitive source bodies with bounded
//! complementary selection. The skeleton-first [`pack`] API and explicit
//! research comparators remain available for reproducible comparisons.
//!
//! Principles carried into the API:
//!
//! - **Evidence, not invention.** Every result carries project, view,
//!   generation, commit, path, line range and content hash ([`Location`]),
//!   plus why it matched ([`Reason`]) and a per-source score breakdown.
//! - **No silent fallback.** A missing or failing source is reported in
//!   [`SearchResponse::degraded`] (`"semantic: provider not configured"`);
//!   nothing is substituted. An empty answer explains itself
//!   ([`EmptyExplanation`]) and never claims that behaviour does not exist.
//! - **Pinned views.** Evidence outside the [`ViewManifest`] pinned at query
//!   start is dropped and counted, so results are never half old, half new.
//! - **Deterministic.** Same inputs, same output: every sort has an explicit
//!   tie-break ending in [`Location`] order; glossaries are order-independent.
//!
//! Text matching for the planner and glossary uses *folding*: Unicode
//! lowercase with Turkish letters mapped to ASCII (`ödeme` = `odeme`).
//!
//! ```
//! use knowell_core::Name;
//! use knowell_query::{
//!     Glossary, GlossaryEntry, Intent, QueryScope, SearchConfig, Sources, TermRelation,
//!     ViewManifest, plan, search,
//! };
//!
//! let glossary = Glossary::new(vec![GlossaryEntry::approved(
//!     "ödeme",
//!     "payment",
//!     TermRelation::Translation,
//! )])?;
//! let plan = plan("Ödemeyi iki kez işlemeyi nerede engelliyoruz?", &glossary);
//! assert_eq!(plan.intent, Intent::Behavior);
//! assert_eq!(plan.expansions[0].expansion, "payment");
//!
//! // Nothing is pinned and no source is configured: the answer says so
//! // instead of pretending there is nothing to find.
//! let scope = QueryScope::all(ViewManifest::new(Name::new("shop")?));
//! let response = search(&plan, &scope, &Sources::default(), &SearchConfig::default())?;
//! assert!(response.results.is_empty());
//! assert!(response.empty.is_some());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod candidate;
mod config;
mod engine;
mod error;
mod expand;
mod explain;
mod fusion;
mod glob;
mod glossary;
mod ids;
mod pack;
mod plan;
mod rerank;
mod scope;
mod task_pack;
mod text;

pub use candidate::{
    Candidate, ExactSource, ExactTarget, LexicalSource, MatchDetail, SourceError, SourceKind,
    SourceLists, SourceRequest, SourceStatus, VectorSource,
};
pub use config::{
    EdgePresets, ExpansionConfig, FusionConfig, RerankConfig, SearchConfig, SourceWeights,
    WeightPresets,
};
pub use engine::{
    SearchResponse, SearchResult, Sources, collect_candidates, search, search_with_candidates,
};
pub use error::QueryError;
pub use expand::{
    EdgeKind, EvidenceType, ExpandRequest, ExpandedItem, ExpansionStats, GraphExpander, GraphNode,
    GraphStep, Neighbor, Resolution,
};
pub use explain::{
    ABSENCE_NOTE, Component, CoverageGap, Degradation, DropCounts, EmptyExplanation, EmptyReason,
    Reason, RerankScore, ScoreBreakdown, SearchStats, SearchedCoverage, SearchedView, SourceCounts,
    SourceScore,
};
pub use glob::PathGlob;
pub use glossary::{Expansion, Glossary, GlossaryEntry, TermRelation, TermStatus};
pub use ids::{CommitId, Language, Layer, Location, ViewId};
pub use pack::{
    CharsPerToken, Citation, ContextPack, Omission, OmitReason, Origin, PackItem, Snippet,
    SnippetKind, SnippetRequest, SnippetSource, Tokenizer, Uncertainty, pack, pack_with,
};
pub use plan::{
    ExactTerm, HttpMethod, Intent, IntentClassifier, IntentDecision, PlanOptions, QueryPlan,
    Signal, SignalStrength, TermKind, TraceFrame, plan, plan_with,
};
pub use rerank::{RerankItem, Reranker};
pub use scope::{
    OverlayPin, PathFilter, PinnedView, ProjectCoverage, ProjectPin, QueryScope, ViewManifest,
};
pub use task_pack::{
    EvidenceRole, RoleEvidence, TaskContextPack, TaskPackOptions, TaskSelectionReport,
    TaskSelectionStrategy, pack_task, pack_task_with, task_source_locations,
};
