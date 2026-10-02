//! Memory and knowledge: scoped, versioned, evidence-backed records,
//! their state machine, tasks and checkpoints, and session bootstrap packs.
//!
//! This crate is pure domain logic: no database, no I/O, no clock. Callers
//! pass timestamps and identifiers in, and persist what comes out (the store
//! crate does that). Everything is deterministic so results can be compared
//! and tested exactly.
//!
//! * [`KnowledgeRecord`] and its state machine ([`RecordState`]), with
//!   [`AcceptancePolicy`] deciding what auto-accepts.
//! * [`compute_staleness`] / [`apply_staleness`] / [`RevalidationQueue`]:
//!   records whose evidence changed are kept, flagged and queued.
//! * [`detect_conflicts`]: contradictions are reported, never merged.
//! * [`Task`], [`Checkpoint`] and [`resume`].
//! * [`bootstrap_pack`]: the budgeted, sourced start-of-session pack.
//! * [`render_section`], [`replace_section`] and [`doc_drift`]: write-back
//!   into repository documents.
//! * Every write path scans free text with `knowell_secrets` and rejects
//!   secrets by naming only the finding kind and line.

mod bootstrap;
mod conflict;
mod error;
mod guard;
mod ids;
mod model;
mod policy;
mod record;
mod staleness;
mod task;
mod writeback;

#[cfg(test)]
mod testutil;

pub use bootstrap::{
    BootstrapInputs, BootstrapItem, BootstrapPack, ItemSource, OmittedItem, ProjectMap, Tier,
    bootstrap_pack, estimate_tokens,
};
pub use conflict::{Conflict, conflicts_for_candidate, detect_conflicts};
pub use error::KnowledgeError;
pub use guard::check_no_secrets;
pub use ids::{
    ClientId, CommitId, RecordId, SessionId, Subject, SymbolId, TaskId, Timestamp, UserId, ViewId,
};
pub use model::{
    Action, Actor, Evidence, HistoryEntry, KnowledgeRecord, NewRecord, RULE_TAG, RecordKind,
    RecordState, RecordVersion, Scope,
};
pub use policy::{AcceptancePolicy, Decision, Rights};
pub use record::{EditPatch, WriteOutcome};
pub use staleness::{
    ApplyOutcome, ChangeSet, FileChange, FileChangeKind, RevalidationItem, RevalidationQueue,
    StaleFinding, StaleReason, StalenessReport, SymbolChange, SymbolChangeKind, apply_staleness,
    compute_staleness,
};
pub use task::{
    Baseline, Checkpoint, DecisionIssue, DecisionProblem, FileRef, ManifestChange,
    ManifestChangeKind, ManifestPin, OpenQuestion, ProgressNote, ResumeDigest, StaleRecordRef,
    Task, TaskStatus, resume,
};
pub use writeback::{DriftRef, DriftReport, doc_drift, render_section, replace_section};
