//! Indexing engine: keeps every tracked view of every project current —
//! source sync, change planning, durable jobs, the incremental pipeline
//! (T0 text → T1 symbols → T3 relations → activation → T2 embeddings),
//! generation fencing and freshness reporting.
//!
//! # Overview
//!
//! - [`Indexer`] holds the engine state. [`Indexer::register`] makes sure a
//!   resolved workspace (from `knowell-config`) exists in the store;
//!   [`Indexer::refresh_view`] resolves a view's target (never substituting
//!   another ref) and queues a build when it moved; [`Indexer::status`]
//!   reports freshness per tier; [`Indexer::subscribe`] streams progress.
//! - A build is a chain of durable jobs, one per stage (see [`jobs`]), run
//!   by a [`Worker`] or by [`Indexer::run_until_idle`] in standalone mode.
//!   The previous active generation keeps serving until the new one is
//!   activated behind the store's generation fence, which happens right
//!   after text, symbols and relations: lexical, symbol and graph search
//!   are current within seconds, and embeddings follow as enrichment
//!   ([`Indexer::embedding_coverage`] says how far they got).
//! - T3 links contracts across the workspace's projects with
//!   [`LinkRelationStage`] by default; T1 records conservative references
//!   (syntactic or heuristic, never presented as semantic).
//! - [`Indexer::watch`] turns repository events into builds and overlay
//!   updates and reconciles periodically; [`Indexer::build_overlay`] builds
//!   a worktree's in-memory personal layer ([`Overlay`]).
//!
//! The crate README describes the pipeline, tier semantics, on-disk layout
//! and failure modes.

mod analyze;
mod config;
mod context;
mod embeddings_stage;
mod error;
mod indexer;
pub mod jobs;
mod lexical;
mod link;
mod manifest;
mod merkle;
mod overlay;
mod pipeline;
mod plan;
mod references;
mod relate;
mod relations_stage;
mod status;
mod symbols_stage;
mod watch;
mod worker;

pub use analyze::{SYMBOL_PATH_SEPARATOR, parser_version_tag, split_symbol_key, symbol_key};
pub use config::{
    ContentPolicy, EmbeddingSettings, GeneratedPolicy, GitConfigMode, IndexerConfig, JobSettings,
    Limits, Priority, Retention, WorkerConfig,
};
pub use context::{EmbeddingPlan, RegisteredView, Registration, RegistrationIssue};
pub use error::IndexError;
pub use indexer::{IndexStats, Indexer, IndexerBuilder, RunSummary, SyncOutcome, skip_reason};
pub use jobs::BuildTarget;
pub use link::{DEFAULT_MAX_LINK_FILES, LINK_STAGE_NAME, LinkRelationStage};
pub use merkle::tree_hash;
pub use overlay::{Overlay, OverlayFile};
pub use plan::PlanKind;
pub use relate::{
    NoRelations, RelationError, RelationFile, RelationInput, RelationOutput, RelationStage,
    StaleFile, StalenessEvent,
};
pub use status::{
    EmbeddingCoverage, ProgressEvent, ProgressKind, Tier, TierSkip, TierState, TierStates,
    ViewStatus,
};
pub use watch::{ReconcileReport, WatchHandle};
pub use worker::Worker;
