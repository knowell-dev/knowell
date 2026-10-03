//! PostgreSQL storage for Knowell: the single source of truth.
//!
//! - [`Store`] — connection pool, embedded migrations ([`Store::migrate`])
//!   and server checks ([`Store::check_server`]). Errors never contain the
//!   connection URL or password.
//! - [`hierarchy`] — organization → workspace → project, and sources.
//! - [`views`] — views, generations with the activation fence, manifests.
//! - [`content`] — content blobs, chunks, generation-scoped file versions,
//!   per-path prepared embedding inputs of chunks.
//! - [`symbols`] — stable logical symbols and their occurrences.
//! - [`graph`] — evidence-carrying edges, contracts, bounded traversal.
//! - [`embeddings`] — profiles, `halfvec` vectors with one partial HNSW index
//!   per profile, nearest-neighbour search, index generations.
//! - [`jobs`] — durable job queue (`SKIP LOCKED`, leases, retries, dead
//!   letters), tenant-scoped listings, view-scoped claims.
//! - [`knowledge`] — memory records with immutable versions, evidence and
//!   history; optimistic concurrency; full-text search; staleness lookups.
//! - [`tasks`] — tasks and append-only checkpoints.
//! - [`identity`] — principals, grants and API tokens (hashes only).
//! - [`audit`] — the append-only audit log.
//!
//! Repository functions are plain `async fn`s taking `&mut PgConnection`, so
//! they work on a pooled connection ([`Store::acquire`]) and inside a
//! caller's transaction ([`Store::begin`]) alike. Functions that need several
//! statements to be atomic open a transaction (a savepoint when nested).
//!
//! All queries are checked at run time (no compile-time database needed);
//! the integration tests in `tests/` cover every one of them against a real
//! PostgreSQL with pgvector.

pub mod audit;
pub mod content;
pub mod embeddings;
mod error;
pub mod graph;
pub mod hierarchy;
pub mod identity;
mod ids;
pub mod jobs;
pub mod knowledge;
mod migrations;
mod store;
pub mod symbols;
pub mod tasks;
mod types;
pub mod views;

pub use error::StoreError;
pub use ids::{
    ApiTokenId, AuditEntryId, CheckpointReceiptId, ContractId, EdgeId, GrantId, IndexGenerationId,
    JobId, KnowledgeRecordId, ManifestId, OccurrenceId, OrganizationId, PrincipalId, ProfileId,
    ProjectId, SourceId, SymbolId, TaskId, ViewId, WorkspaceId,
};
pub use store::{ServerInfo, ServerIssue, Store, StoreOptions, VectorExtension};
pub use types::{
    ApiTokenScope, ContractKind, ContractRole, EvidenceType, GenerationState, GrantRole, JobState,
    KnowledgeAction, KnowledgeKind, KnowledgeScopeKind, KnowledgeState, NodeKind, OccurrenceRole,
    PrincipalKind, Resolution, SourceKind, TaskStatus, ViewKind,
};
pub use views::GenerationPin;

/// Re-exported so callers can name connection types without depending on
/// sqlx directly.
pub use sqlx::{PgConnection, PgPool, postgres::PgConnectOptions};
