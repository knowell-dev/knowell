//! Persistence seam for memory records and tasks.
//!
//! The domain rules (state machine, acceptance policy, secret guard,
//! bootstrap, resume) live in `knowell-knowledge`; a [`MemoryRepo`] only
//! stores what those rules produce. [`StoreMemory`] persists into the
//! `knowell-store` knowledge and task tables; [`InMemoryMemory`] keeps
//! everything in process (tests, throwaway sessions).

mod store_repo;

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, PoisonError};

use knowell_core::Name;
use knowell_knowledge::{
    Checkpoint, KnowledgeRecord, RecordId, RecordKind, RecordState, Scope, Task, TaskId, TaskStatus,
};
use uuid::Uuid;

pub(crate) use store_repo::Directory;
pub use store_repo::StoreMemory;

/// A boxed, sendable future (the repository trait is object safe).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Failures of a [`MemoryRepo`]. Messages never contain record bodies.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    /// A record or task with this id already exists.
    #[error("already exists: {0}")]
    AlreadyExists(String),
    /// The record or task does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// Someone else changed the item since it was read.
    #[error("conflict: {0}")]
    Conflict(String),
    /// The item cannot be stored as given.
    #[error("invalid: {0}")]
    Invalid(String),
    /// The database failed.
    #[error("store: {0}")]
    Store(#[from] knowell_store::StoreError),
}

/// A stored record and its optimistic-concurrency revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordRow {
    /// The record.
    pub record: KnowledgeRecord,
    /// Revision of the stored row; updates name the revision they read.
    pub revision: u64,
    /// Workspace identity for each evidence entry, in evidence order.
    /// `None` means that the repository cannot determine the namespace.
    pub evidence_workspaces: Vec<Option<Name>>,
}

/// What [`MemoryRepo::find_records`] returns. Empty lists mean "any".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordQuery {
    /// Records in exactly one of these scopes.
    pub scopes: Vec<Scope>,
    /// Records in one of these states.
    pub states: Vec<RecordState>,
    /// Records of one of these kinds.
    pub kinds: Vec<RecordKind>,
    /// Words that must occur in title, subject or body (full-text match).
    pub text: Option<String>,
    /// Most records, at least 1.
    pub limit: u32,
}

/// One checkpoint for [`MemoryRepo::save_checkpoint`]: everything it writes.
#[derive(Debug, Clone, Copy)]
pub struct CheckpointSave<'a> {
    /// Receipt of an idempotent save, derived from the caller and their
    /// idempotency key; `None` always stores.
    pub receipt: Option<Uuid>,
    /// The task as read; the save fails with a conflict when it has moved.
    pub before: &'a TaskRow,
    /// The task after the checkpoint (same id as `before`).
    pub after: &'a Task,
    /// New task decisions, whose evidence was resolved in `workspace`.
    pub decisions: &'a [KnowledgeRecord],
    /// The workspace the decisions' evidence was resolved in.
    pub workspace: &'a Name,
    /// The checkpoint to append (of the same task).
    pub checkpoint: &'a Checkpoint,
}

/// What [`MemoryRepo::save_checkpoint`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointSaved {
    /// The decisions, task update, checkpoint and receipt were stored.
    Saved {
        /// The updated task.
        task: Box<TaskRow>,
        /// The new checkpoint's number within the task.
        seq: u64,
        /// The stored decisions, in input order.
        decisions: Vec<RecordRow>,
    },
    /// The receipt already existed (an earlier attempt, possibly by another
    /// process, or a concurrent save that won); nothing was stored.
    Replayed {
        /// The task of the receipt's checkpoint.
        task: TaskId,
        /// That checkpoint's number.
        seq: u64,
    },
}

impl CheckpointSave<'_> {
    /// Rejects a save whose parts name different tasks.
    pub(crate) fn check(&self) -> Result<(), MemoryError> {
        if self.after.id != self.before.task.id || self.checkpoint.task != self.after.id {
            return Err(MemoryError::Invalid(
                "checkpoint save mixes different tasks".to_owned(),
            ));
        }
        Ok(())
    }
}

/// A stored task with its workspace, owner and revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    /// The task.
    pub task: Task,
    /// The workspace the task is about.
    pub workspace: Option<Name>,
    /// The user the task belongs to (the engine's user key).
    pub owner: Option<String>,
    /// Revision of the stored row.
    pub revision: u64,
}

/// A stored checkpoint with its sequence number (1, 2, … per task).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointRow {
    /// Sequence number.
    pub seq: u64,
    /// The checkpoint.
    pub checkpoint: Checkpoint,
}

/// Where memory records and tasks are kept.
pub trait MemoryRepo: Send + Sync + 'static {
    /// Stores a new record ([`MemoryError::AlreadyExists`] for a taken id).
    fn insert_record<'a>(
        &'a self,
        record: &'a KnowledgeRecord,
    ) -> BoxFuture<'a, Result<RecordRow, MemoryError>>;

    /// Stores a record whose evidence was resolved in `workspace`, retaining
    /// that origin even for organization or user scopes. Unknown namespaces
    /// must never be inferred from a reader's current source selection.
    /// The caller must resolve and authorize evidence in that workspace before insertion.
    fn insert_record_in_workspace<'a>(
        &'a self,
        record: &'a KnowledgeRecord,
        workspace: &'a Name,
    ) -> BoxFuture<'a, Result<RecordRow, MemoryError>>;

    /// Replaces `before` with `after` (same id): state, content version,
    /// pin, links and the history entries appended since `before`.
    fn update_record<'a>(
        &'a self,
        before: &'a RecordRow,
        after: &'a KnowledgeRecord,
    ) -> BoxFuture<'a, Result<RecordRow, MemoryError>>;

    /// One record.
    fn get_record(&self, id: RecordId) -> BoxFuture<'_, Result<Option<RecordRow>, MemoryError>>;

    /// Records matching `query`, most recently updated first (full-text
    /// matches by relevance first).
    fn find_records<'a>(
        &'a self,
        query: &'a RecordQuery,
    ) -> BoxFuture<'a, Result<Vec<RecordRow>, MemoryError>>;

    /// Stores a new task.
    fn create_task<'a>(&'a self, row: &'a TaskRow) -> BoxFuture<'a, Result<TaskRow, MemoryError>>;

    /// One task.
    fn get_task(&self, id: TaskId) -> BoxFuture<'_, Result<Option<TaskRow>, MemoryError>>;

    /// Tasks of `workspace` (all when `None`) in one of `statuses` (any
    /// when empty), most recently updated first.
    fn list_tasks<'a>(
        &'a self,
        workspace: Option<&'a Name>,
        statuses: &'a [TaskStatus],
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<TaskRow>, MemoryError>>;

    /// Shared tasks and tasks owned by `owner`, filtered before the result
    /// limit. With no owner, only shared tasks are returned.
    fn list_owned_tasks<'a>(
        &'a self,
        workspace: Option<&'a Name>,
        statuses: &'a [TaskStatus],
        owner: Option<&'a str>,
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<TaskRow>, MemoryError>>;

    /// Replaces `before` with `after` (same id).
    fn update_task<'a>(
        &'a self,
        before: &'a TaskRow,
        after: &'a Task,
    ) -> BoxFuture<'a, Result<TaskRow, MemoryError>>;

    /// Appends a checkpoint; returns its sequence number.
    fn append_checkpoint<'a>(
        &'a self,
        checkpoint: &'a Checkpoint,
    ) -> BoxFuture<'a, Result<u64, MemoryError>>;

    /// Checkpoints of a task, oldest first.
    fn checkpoints(&self, task: TaskId) -> BoxFuture<'_, Result<Vec<CheckpointRow>, MemoryError>>;

    /// Stores a checkpoint atomically: the decisions, the task update, the
    /// checkpoint and its receipt all persist, or none of them does. When
    /// the receipt already exists, also after a restart or because a
    /// concurrent save with it won, nothing is stored and that checkpoint is
    /// returned as [`CheckpointSaved::Replayed`].
    fn save_checkpoint<'a>(
        &'a self,
        save: CheckpointSave<'a>,
    ) -> BoxFuture<'a, Result<CheckpointSaved, MemoryError>>;

    /// The task and checkpoint number an idempotent save with `receipt`
    /// produced, if any.
    fn checkpoint_receipt(
        &self,
        receipt: Uuid,
    ) -> BoxFuture<'_, Result<Option<(TaskId, u64)>, MemoryError>>;
}

/// Everything in process memory; lost when the process ends.
#[derive(Debug, Default)]
pub struct InMemoryMemory {
    state: Mutex<InMemoryState>,
}

#[derive(Debug, Default, Clone)]
struct InMemoryState {
    records: BTreeMap<RecordId, RecordRow>,
    tasks: BTreeMap<TaskId, TaskRow>,
    checkpoints: BTreeMap<TaskId, Vec<CheckpointRow>>,
    receipts: BTreeMap<Uuid, (TaskId, u64)>,
}

impl InMemoryMemory {
    /// An empty repository.
    pub fn new() -> Self {
        Self::default()
    }

    fn with_state<T>(&self, f: impl FnOnce(&mut InMemoryState) -> T) -> T {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut state)
    }

    fn insert_bound_record(
        &self,
        record: &KnowledgeRecord,
        workspace: Option<&Name>,
    ) -> Result<RecordRow, MemoryError> {
        self.with_state(|state| Self::insert_bound(state, record, workspace))
    }

    fn insert_bound(
        state: &mut InMemoryState,
        record: &KnowledgeRecord,
        workspace: Option<&Name>,
    ) -> Result<RecordRow, MemoryError> {
        {
            if state.records.contains_key(&record.id) {
                return Err(MemoryError::AlreadyExists(format!("record {}", record.id)));
            }
            let scoped_workspace = match &record.scope {
                Scope::Workspace(workspace) | Scope::Project { workspace, .. } => {
                    Some(workspace.clone())
                }
                Scope::Task(task) => state.tasks.get(task).and_then(|row| row.workspace.clone()),
                _ => None,
            };
            if let (Some(scoped), Some(origin)) = (&scoped_workspace, workspace)
                && scoped != origin
            {
                return Err(MemoryError::Invalid(
                    "record scope does not match its evidence workspace".to_owned(),
                ));
            }
            let origin = workspace.cloned().or(scoped_workspace);
            let row = RecordRow {
                record: record.clone(),
                revision: 1,
                evidence_workspaces: vec![origin; record.evidence.len()],
            };
            state.records.insert(record.id, row.clone());
            Ok(row)
        }
    }

    fn update_task_in(
        state: &mut InMemoryState,
        before: &TaskRow,
        after: &Task,
    ) -> Result<TaskRow, MemoryError> {
        let Some(current) = state.tasks.get_mut(&before.task.id) else {
            return Err(MemoryError::NotFound(format!("task {}", before.task.id)));
        };
        if current.revision != before.revision {
            return Err(MemoryError::Conflict(format!(
                "task {} changed since it was read",
                before.task.id
            )));
        }
        current.task = after.clone();
        current.revision = current.revision.saturating_add(1);
        Ok(current.clone())
    }

    fn append_checkpoint_in(
        state: &mut InMemoryState,
        checkpoint: &Checkpoint,
    ) -> Result<u64, MemoryError> {
        if !state.tasks.contains_key(&checkpoint.task) {
            return Err(MemoryError::NotFound(format!("task {}", checkpoint.task)));
        }
        let list = state.checkpoints.entry(checkpoint.task).or_default();
        let seq = u64::try_from(list.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        list.push(CheckpointRow {
            seq,
            checkpoint: checkpoint.clone(),
        });
        Ok(seq)
    }

    fn updated_evidence_workspaces(
        current: &RecordRow,
        after: &KnowledgeRecord,
    ) -> Result<Vec<Option<Name>>, MemoryError> {
        if current.record.evidence.len() != current.evidence_workspaces.len() {
            return Err(MemoryError::Invalid(
                "saved memory evidence identity is incomplete".to_owned(),
            ));
        }
        // Identical public pointers can have distinct namespaces, so an
        // unchanged evidence vector retains the original ordinal identities.
        if current.record.evidence == after.evidence {
            return Ok(current.evidence_workspaces.clone());
        }
        after
            .evidence
            .iter()
            .map(|evidence| {
                // The outer option distinguishes an unmatched pointer from a
                // retained pointer whose canonical namespace is unknown.
                let mut retained: Option<&Option<Name>> = None;
                for (previous, workspace) in current
                    .record
                    .evidence
                    .iter()
                    .zip(&current.evidence_workspaces)
                {
                    if previous == evidence {
                        if retained.is_some_and(|known| known != workspace) {
                            return Err(MemoryError::Invalid(
                                "updated memory evidence has ambiguous saved identity".to_owned(),
                            ));
                        }
                        retained = Some(workspace);
                    }
                }
                match retained {
                    Some(workspace) => Ok(workspace.clone()),
                    None => match &after.scope {
                        Scope::Workspace(workspace) | Scope::Project { workspace, .. } => {
                            Ok(Some(workspace.clone()))
                        }
                        _ => Err(MemoryError::Invalid(
                            "updated memory evidence requires a verified workspace".to_owned(),
                        )),
                    },
                }
            })
            .collect()
    }
}

/// Lowercase text with every non-alphanumeric character turned into a space.
fn fold_words(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                ' '
            }
        })
        .collect()
}

/// Whether every word of `query` occurs in the record (substring match).
fn record_matches(record: &KnowledgeRecord, query: &str) -> bool {
    let haystack = fold_words(&format!(
        "{} {} {}",
        record.title, record.subject, record.body
    ));
    fold_words(query)
        .split_whitespace()
        .all(|word| haystack.contains(word))
}

impl MemoryRepo for InMemoryMemory {
    fn insert_record<'a>(
        &'a self,
        record: &'a KnowledgeRecord,
    ) -> BoxFuture<'a, Result<RecordRow, MemoryError>> {
        let result = self.insert_bound_record(record, None);
        Box::pin(std::future::ready(result))
    }

    fn insert_record_in_workspace<'a>(
        &'a self,
        record: &'a KnowledgeRecord,
        workspace: &'a Name,
    ) -> BoxFuture<'a, Result<RecordRow, MemoryError>> {
        let result = self.insert_bound_record(record, Some(workspace));
        Box::pin(std::future::ready(result))
    }

    fn update_record<'a>(
        &'a self,
        before: &'a RecordRow,
        after: &'a KnowledgeRecord,
    ) -> BoxFuture<'a, Result<RecordRow, MemoryError>> {
        let result = self.with_state(|state| {
            if after.id != before.record.id || after.scope != before.record.scope {
                return Err(MemoryError::Invalid(
                    "memory identity and scope cannot change".to_owned(),
                ));
            }
            let Some(current) = state.records.get_mut(&before.record.id) else {
                return Err(MemoryError::NotFound(format!(
                    "record {}",
                    before.record.id
                )));
            };
            if current.revision != before.revision
                || current.record.version != before.record.version
                || current.record.scope != before.record.scope
                || current.record.evidence != before.record.evidence
                || current.evidence_workspaces != before.evidence_workspaces
            {
                return Err(MemoryError::Conflict(
                    "saved memory identity changed since it was read".to_owned(),
                ));
            }
            if after.version == current.record.version && after.evidence != current.record.evidence
            {
                return Err(MemoryError::Invalid(
                    "changed memory evidence requires a new content version".to_owned(),
                ));
            }
            let workspaces = Self::updated_evidence_workspaces(current, after)?;
            let revision = current.revision.checked_add(1).ok_or_else(|| {
                MemoryError::Invalid("memory revision cannot increase".to_owned())
            })?;
            current.evidence_workspaces = workspaces;
            current.record = after.clone();
            current.revision = revision;
            Ok(current.clone())
        });
        Box::pin(std::future::ready(result))
    }

    fn get_record(&self, id: RecordId) -> BoxFuture<'_, Result<Option<RecordRow>, MemoryError>> {
        let found = self.with_state(|state| state.records.get(&id).cloned());
        Box::pin(std::future::ready(Ok(found)))
    }

    fn find_records<'a>(
        &'a self,
        query: &'a RecordQuery,
    ) -> BoxFuture<'a, Result<Vec<RecordRow>, MemoryError>> {
        let mut rows: Vec<RecordRow> = self.with_state(|state| {
            state
                .records
                .values()
                .filter(|row| query.scopes.is_empty() || query.scopes.contains(&row.record.scope))
                .filter(|row| query.states.is_empty() || query.states.contains(&row.record.state))
                .filter(|row| query.kinds.is_empty() || query.kinds.contains(&row.record.kind))
                .filter(|row| {
                    query
                        .text
                        .as_deref()
                        .is_none_or(|text| record_matches(&row.record, text))
                })
                .cloned()
                .collect()
        });
        rows.sort_by(|a, b| {
            b.record
                .updated_at
                .cmp(&a.record.updated_at)
                .then_with(|| b.record.id.cmp(&a.record.id))
        });
        rows.truncate(usize::try_from(query.limit.max(1)).unwrap_or(usize::MAX));
        Box::pin(std::future::ready(Ok(rows)))
    }

    fn create_task<'a>(&'a self, row: &'a TaskRow) -> BoxFuture<'a, Result<TaskRow, MemoryError>> {
        let result = self.with_state(|state| {
            if state.tasks.contains_key(&row.task.id) {
                return Err(MemoryError::AlreadyExists(format!("task {}", row.task.id)));
            }
            let stored = TaskRow {
                revision: 1,
                ..row.clone()
            };
            state.tasks.insert(row.task.id, stored.clone());
            Ok(stored)
        });
        Box::pin(std::future::ready(result))
    }

    fn get_task(&self, id: TaskId) -> BoxFuture<'_, Result<Option<TaskRow>, MemoryError>> {
        let found = self.with_state(|state| state.tasks.get(&id).cloned());
        Box::pin(std::future::ready(Ok(found)))
    }

    fn list_tasks<'a>(
        &'a self,
        workspace: Option<&'a Name>,
        statuses: &'a [TaskStatus],
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<TaskRow>, MemoryError>> {
        let mut rows: Vec<TaskRow> = self.with_state(|state| {
            state
                .tasks
                .values()
                .filter(|row| workspace.is_none_or(|w| row.workspace.as_ref() == Some(w)))
                .filter(|row| statuses.is_empty() || statuses.contains(&row.task.status))
                .cloned()
                .collect()
        });
        rows.sort_by(|a, b| {
            b.task
                .updated_at
                .cmp(&a.task.updated_at)
                .then_with(|| b.task.id.cmp(&a.task.id))
        });
        rows.truncate(usize::try_from(limit.max(1)).unwrap_or(usize::MAX));
        Box::pin(std::future::ready(Ok(rows)))
    }

    fn list_owned_tasks<'a>(
        &'a self,
        workspace: Option<&'a Name>,
        statuses: &'a [TaskStatus],
        owner: Option<&'a str>,
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<TaskRow>, MemoryError>> {
        let mut rows: Vec<TaskRow> = self.with_state(|state| {
            state
                .tasks
                .values()
                .filter(|row| workspace.is_none_or(|w| row.workspace.as_ref() == Some(w)))
                .filter(|row| statuses.is_empty() || statuses.contains(&row.task.status))
                .filter(|row| row.owner.is_none() || row.owner.as_deref() == owner)
                .cloned()
                .collect()
        });
        rows.sort_by(|a, b| {
            b.task
                .updated_at
                .cmp(&a.task.updated_at)
                .then_with(|| b.task.id.cmp(&a.task.id))
        });
        rows.truncate(usize::try_from(limit.max(1)).unwrap_or(usize::MAX));
        Box::pin(std::future::ready(Ok(rows)))
    }

    fn update_task<'a>(
        &'a self,
        before: &'a TaskRow,
        after: &'a Task,
    ) -> BoxFuture<'a, Result<TaskRow, MemoryError>> {
        let result = self.with_state(|state| Self::update_task_in(state, before, after));
        Box::pin(std::future::ready(result))
    }

    fn append_checkpoint<'a>(
        &'a self,
        checkpoint: &'a Checkpoint,
    ) -> BoxFuture<'a, Result<u64, MemoryError>> {
        let result = self.with_state(|state| Self::append_checkpoint_in(state, checkpoint));
        Box::pin(std::future::ready(result))
    }

    fn checkpoints(&self, task: TaskId) -> BoxFuture<'_, Result<Vec<CheckpointRow>, MemoryError>> {
        let rows =
            self.with_state(|state| state.checkpoints.get(&task).cloned().unwrap_or_default());
        Box::pin(std::future::ready(Ok(rows)))
    }

    fn save_checkpoint<'a>(
        &'a self,
        save: CheckpointSave<'a>,
    ) -> BoxFuture<'a, Result<CheckpointSaved, MemoryError>> {
        let result = save.check().and_then(|()| {
            self.with_state(|state| {
                if let Some(&(task, seq)) = save.receipt.and_then(|id| state.receipts.get(&id)) {
                    return Ok(CheckpointSaved::Replayed { task, seq });
                }
                // Work on a copy so that a failure leaves nothing behind.
                let mut next = state.clone();
                let decisions = save
                    .decisions
                    .iter()
                    .map(|record| Self::insert_bound(&mut next, record, Some(save.workspace)))
                    .collect::<Result<Vec<_>, _>>()?;
                let task = Self::update_task_in(&mut next, save.before, save.after)?;
                let seq = Self::append_checkpoint_in(&mut next, save.checkpoint)?;
                if let Some(id) = save.receipt {
                    next.receipts.insert(id, (task.task.id, seq));
                }
                *state = next;
                Ok(CheckpointSaved::Saved {
                    task: Box::new(task),
                    seq,
                    decisions,
                })
            })
        });
        Box::pin(std::future::ready(result))
    }

    fn checkpoint_receipt(
        &self,
        receipt: Uuid,
    ) -> BoxFuture<'_, Result<Option<(TaskId, u64)>, MemoryError>> {
        let found = self.with_state(|state| state.receipts.get(&receipt).copied());
        Box::pin(std::future::ready(Ok(found)))
    }
}

#[cfg(test)]
mod tests {
    use knowell_knowledge::{
        AcceptancePolicy, Actor, EditPatch, Evidence, NewRecord, Rights, Subject, Timestamp, UserId,
    };

    use super::*;

    fn record(title: &str, body: &str) -> KnowledgeRecord {
        KnowledgeRecord::propose(
            NewRecord {
                id: RecordId::generate(),
                scope: Scope::Organization,
                kind: RecordKind::ModelSuggestion,
                subject: Subject::new("payments.idempotency").unwrap(),
                title: title.into(),
                body: body.into(),
                evidence: Vec::new(),
                related_symbols: Vec::new(),
                tags: Vec::new(),
                pinned: false,
            },
            Actor::System,
            Timestamp::from_unix_seconds(10),
        )
        .unwrap()
    }

    fn evidence(path: &str) -> Evidence {
        Evidence {
            project: Name::new("synthetic-project").unwrap(),
            view: knowell_knowledge::ViewId::new("branch:main").unwrap(),
            commit: knowell_knowledge::CommitId::new("a".repeat(40)).unwrap(),
            path: knowell_core::RepoPath::new(path).unwrap(),
            range: knowell_core::LineRange::new(1, 1).unwrap(),
            content_hash: knowell_core::ContentHash::of(path.as_bytes()),
        }
    }

    fn edited(before: &KnowledgeRecord, patch: EditPatch) -> KnowledgeRecord {
        let mut after = before.clone();
        after
            .edit(
                before.version,
                patch,
                &Actor::Human(UserId::new("synthetic-reviewer").unwrap()),
                Rights::REVIEWER,
                &AcceptancePolicy::default(),
                "update synthetic source evidence",
                Timestamp::from_unix_seconds(20),
            )
            .unwrap();
        assert_eq!(after.version, before.version + 1);
        after
    }

    #[tokio::test]
    async fn records_round_trip_with_revisions() {
        let repo = InMemoryMemory::new();
        let first = record("Idempotency keys", "captures carry a key");
        let row = repo.insert_record(&first).await.unwrap();
        assert_eq!(row.revision, 1);
        assert!(matches!(
            repo.insert_record(&first).await,
            Err(MemoryError::AlreadyExists(_))
        ));
        let mut changed = first.clone();
        changed.pinned = true;
        let updated = repo.update_record(&row, &changed).await.unwrap();
        assert_eq!(updated.revision, 2);
        assert!(matches!(
            repo.update_record(&row, &changed).await,
            Err(MemoryError::Conflict(_))
        ));
        let query = RecordQuery {
            text: Some("CAPTURES key".into()),
            limit: 10,
            ..RecordQuery::default()
        };
        assert_eq!(repo.find_records(&query).await.unwrap().len(), 1);
        let query = RecordQuery {
            text: Some("refund".into()),
            limit: 10,
            ..RecordQuery::default()
        };
        assert!(repo.find_records(&query).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn contextual_evidence_origin_survives_updates_and_rejects_scope_mismatch() {
        let repo = InMemoryMemory::new();
        let origin = Name::new("synthetic-origin").unwrap();
        let other = Name::new("synthetic-other").unwrap();
        let mut first = record("Synthetic source identity", "synthetic record body");
        first.evidence.push(knowell_knowledge::Evidence {
            project: Name::new("synthetic-project").unwrap(),
            view: knowell_knowledge::ViewId::new("branch:main").unwrap(),
            commit: knowell_knowledge::CommitId::new("a".repeat(40)).unwrap(),
            path: knowell_core::RepoPath::new("src/probe.rs").unwrap(),
            range: knowell_core::LineRange::new(1, 1).unwrap(),
            content_hash: knowell_core::ContentHash::of(b"synthetic source"),
        });
        let bound = repo
            .insert_record_in_workspace(&first, &origin)
            .await
            .unwrap();
        assert_eq!(bound.evidence_workspaces, vec![Some(origin.clone())]);
        let changed = edited(
            &first,
            EditPatch {
                body: Some("changed synthetic record body".to_owned()),
                ..EditPatch::default()
            },
        );
        let updated = repo.update_record(&bound, &changed).await.unwrap();
        assert_eq!(updated.evidence_workspaces, bound.evidence_workspaces);
        assert_eq!(updated.record.evidence, first.evidence);
        assert_eq!(updated.record.version, first.version + 1);
        assert_eq!(updated.record.body, "changed synthetic record body");

        let mut legacy = first.clone();
        legacy.id = RecordId::generate();
        let unknown = repo.insert_record(&legacy).await.unwrap();
        assert_eq!(unknown.evidence_workspaces, vec![None]);

        let mut mismatched = first;
        mismatched.id = RecordId::generate();
        mismatched.scope = Scope::Workspace(origin);
        assert!(matches!(
            repo.insert_record_in_workspace(&mismatched, &other).await,
            Err(MemoryError::Invalid(_))
        ));
        assert!(repo.get_record(mismatched.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn record_updates_reject_forged_before_identity_without_mutating_canonical_row() {
        let repo = InMemoryMemory::new();
        let origin = Name::new("synthetic-private").unwrap();
        let mut first = record("Synthetic canonical source", "synthetic record body");
        first.evidence.push(evidence("src/original.rs"));
        let canonical = repo
            .insert_record_in_workspace(&first, &origin)
            .await
            .unwrap();
        let mut changed = canonical.record.clone();
        changed.pinned = true;

        let mut forged_origin = canonical.clone();
        forged_origin.evidence_workspaces = vec![Some(Name::new("synthetic-public").unwrap())];
        let mut forged_version = canonical.clone();
        forged_version.record.version += 1;
        let mut forged_evidence = canonical.clone();
        forged_evidence.record.evidence = vec![evidence("src/forged.rs")];
        let mut forged_metadata = canonical.clone();
        forged_metadata.evidence_workspaces.clear();
        let mut forged_scope = canonical.clone();
        forged_scope.record.scope = Scope::Workspace(origin);
        let mut changed_scope = changed.clone();
        changed_scope.scope = forged_scope.record.scope.clone();

        for (before, after) in [
            (forged_origin, changed.clone()),
            (forged_version, changed.clone()),
            (forged_evidence, changed.clone()),
            (forged_metadata, changed.clone()),
            (forged_scope, changed_scope),
        ] {
            let result = repo.update_record(&before, &after).await;
            assert!(matches!(result, Err(MemoryError::Conflict(_))));
            assert_eq!(
                repo.get_record(canonical.record.id).await.unwrap(),
                Some(canonical.clone())
            );
        }
    }

    #[tokio::test]
    async fn record_updates_reject_changed_identity_and_unversioned_evidence() {
        let repo = InMemoryMemory::new();
        let origin = Name::new("synthetic-origin").unwrap();
        let mut first = record("Synthetic fixed identity", "synthetic record body");
        first.evidence.push(evidence("src/original.rs"));
        let canonical = repo
            .insert_record_in_workspace(&first, &origin)
            .await
            .unwrap();
        let mut changed_id = first.clone();
        changed_id.id = RecordId::generate();
        let mut changed_scope = first.clone();
        changed_scope.scope = Scope::Workspace(origin);
        let mut changed_evidence = first.clone();
        changed_evidence.evidence = vec![evidence("src/replaced.rs")];

        for after in [changed_id, changed_scope, changed_evidence] {
            let result = repo.update_record(&canonical, &after).await;
            assert!(matches!(result, Err(MemoryError::Invalid(_))));
            assert_eq!(
                repo.get_record(canonical.record.id).await.unwrap(),
                Some(canonical.clone())
            );
        }
    }

    #[tokio::test]
    async fn versioned_evidence_reorder_retains_origins_and_rejects_unknown_new_source() {
        let repo = InMemoryMemory::new();
        let private = Name::new("synthetic-private").unwrap();
        let public = Name::new("synthetic-public").unwrap();
        let mut first = record("Synthetic mixed sources", "synthetic record body");
        first.evidence = vec![evidence("src/first.rs"), evidence("src/second.rs")];
        repo.insert_record_in_workspace(&first, &private)
            .await
            .unwrap();
        // A wide-scope persisted record can retain evidence from two namespaces.
        repo.with_state(|state| {
            state
                .records
                .get_mut(&first.id)
                .unwrap()
                .evidence_workspaces = vec![Some(private.clone()), Some(public.clone())];
        });
        let canonical = repo.get_record(first.id).await.unwrap().unwrap();
        let after = edited(
            &canonical.record,
            EditPatch {
                body: Some("changed synthetic record body".to_owned()),
                evidence: Some(canonical.record.evidence.iter().rev().cloned().collect()),
                ..EditPatch::default()
            },
        );
        let updated = repo.update_record(&canonical, &after).await.unwrap();
        assert_eq!(updated.record, after);
        assert_eq!(updated.revision, canonical.revision + 1);
        assert_eq!(
            updated.evidence_workspaces,
            vec![Some(public), Some(private)]
        );

        let mut new_evidence = updated.record.evidence.clone();
        new_evidence.push(evidence("src/new-unscoped.rs"));
        let unknown = edited(
            &updated.record,
            EditPatch {
                evidence: Some(new_evidence),
                ..EditPatch::default()
            },
        );
        assert!(matches!(
            repo.update_record(&updated, &unknown).await,
            Err(MemoryError::Invalid(_))
        ));
        assert_eq!(repo.get_record(first.id).await.unwrap(), Some(updated));
    }

    #[tokio::test]
    async fn equal_evidence_with_distinct_origins_preserves_order_and_rejects_ambiguous_edit() {
        let repo = InMemoryMemory::new();
        let private = Name::new("synthetic-private").unwrap();
        let public = Name::new("synthetic-public").unwrap();
        let mut first = record("Synthetic duplicate sources", "synthetic record body");
        let pointer = evidence("src/shared.rs");
        first.evidence = vec![pointer.clone(), pointer.clone()];
        repo.insert_record_in_workspace(&first, &private)
            .await
            .unwrap();
        // Equal public pointers can still refer to different canonical namespaces.
        repo.with_state(|state| {
            state
                .records
                .get_mut(&first.id)
                .unwrap()
                .evidence_workspaces = vec![Some(private.clone()), Some(public.clone())];
        });
        let canonical = repo.get_record(first.id).await.unwrap().unwrap();
        let body_edit = edited(
            &canonical.record,
            EditPatch {
                body: Some("changed synthetic record body".to_owned()),
                ..EditPatch::default()
            },
        );
        let updated = repo.update_record(&canonical, &body_edit).await.unwrap();
        assert_eq!(updated.record, body_edit);
        assert_eq!(updated.evidence_workspaces, canonical.evidence_workspaces);

        let ambiguous = edited(
            &updated.record,
            EditPatch {
                evidence: Some(vec![pointer]),
                ..EditPatch::default()
            },
        );
        assert!(matches!(
            repo.update_record(&updated, &ambiguous).await,
            Err(MemoryError::Invalid(_))
        ));
        assert_eq!(repo.get_record(first.id).await.unwrap(), Some(updated));
    }

    #[tokio::test]
    async fn versioned_scoped_evidence_edit_retains_unknown_origin_and_binds_only_new_pointer() {
        let repo = InMemoryMemory::new();
        let workspace = Name::new("synthetic-workspace").unwrap();
        let mut first = record("Synthetic scoped sources", "synthetic record body");
        first.scope = Scope::Workspace(workspace.clone());
        first.evidence.push(evidence("src/original.rs"));
        repo.insert_record(&first).await.unwrap();
        // Legacy unknown origins must remain unknown even in an explicit scope.
        repo.with_state(|state| {
            state
                .records
                .get_mut(&first.id)
                .unwrap()
                .evidence_workspaces = vec![None];
        });
        let canonical = repo.get_record(first.id).await.unwrap().unwrap();
        let mut new_evidence = first.evidence.clone();
        new_evidence.push(evidence("src/new-scoped.rs"));
        let after = edited(
            &first,
            EditPatch {
                evidence: Some(new_evidence),
                ..EditPatch::default()
            },
        );
        let updated = repo.update_record(&canonical, &after).await.unwrap();
        assert_eq!(updated.record, after);
        assert_eq!(updated.evidence_workspaces, vec![None, Some(workspace)]);
    }

    #[tokio::test]
    async fn record_revision_overflow_rejects_update_without_mutation() {
        let repo = InMemoryMemory::new();
        let first = record("Synthetic maximum revision", "synthetic record body");
        repo.insert_record(&first).await.unwrap();
        repo.with_state(|state| {
            state.records.get_mut(&first.id).unwrap().revision = u64::MAX;
        });
        let canonical = repo.get_record(first.id).await.unwrap().unwrap();
        let mut after = first.clone();
        after.pinned = true;
        assert!(matches!(
            repo.update_record(&canonical, &after).await,
            Err(MemoryError::Invalid(_))
        ));
        assert_eq!(repo.get_record(first.id).await.unwrap(), Some(canonical));
    }

    #[tokio::test]
    async fn checkpoints_are_numbered_per_task() {
        let repo = InMemoryMemory::new();
        let task = Task::new(
            TaskId::generate(),
            "Cancel flow",
            "make cancel idempotent",
            Timestamp::from_unix_seconds(1),
        )
        .unwrap();
        let checkpoint = task
            .checkpoint("did a thing", vec![], Timestamp::from_unix_seconds(2))
            .unwrap();
        assert!(repo.append_checkpoint(&checkpoint).await.is_err());
        repo.create_task(&TaskRow {
            task: task.clone(),
            workspace: None,
            owner: None,
            revision: 0,
        })
        .await
        .unwrap();
        assert_eq!(repo.append_checkpoint(&checkpoint).await.unwrap(), 1);
        assert_eq!(repo.append_checkpoint(&checkpoint).await.unwrap(), 2);
        assert_eq!(repo.checkpoints(task.id).await.unwrap().len(), 2);
        assert_eq!(repo.list_tasks(None, &[], 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn task_ownership_is_filtered_before_limits_in_memory() {
        let repo = InMemoryMemory::new();
        let workspace = Name::new("synthetic-workspace").unwrap();
        let mut visible = Vec::new();
        for index in 0..52_u128 {
            let task = Task::new(
                TaskId::from_uuid(uuid::Uuid::from_u128(index + 1)),
                "Synthetic task",
                "verify readable task ordering",
                Timestamp::from_unix_seconds(i64::try_from(index + 1).unwrap()),
            )
            .unwrap();
            let owner = match index {
                0 => Some("synthetic-owner".to_owned()),
                1 => None,
                _ => Some("synthetic-other".to_owned()),
            };
            if index < 2 {
                visible.push(task.id);
            }
            repo.create_task(&TaskRow {
                task,
                workspace: Some(workspace.clone()),
                owner,
                revision: 0,
            })
            .await
            .unwrap();
        }
        let first = repo
            .list_owned_tasks(Some(&workspace), &[], Some("synthetic-owner"), 1)
            .await
            .unwrap();
        assert_eq!(
            first.iter().map(|row| row.task.id).collect::<Vec<_>>(),
            vec![visible[1]]
        );
        let all = repo
            .list_owned_tasks(Some(&workspace), &[], Some("synthetic-owner"), 2)
            .await
            .unwrap();
        assert_eq!(
            all.iter().map(|row| row.task.id).collect::<Vec<_>>(),
            vec![visible[1], visible[0]]
        );
        let shared = repo
            .list_owned_tasks(Some(&workspace), &[], None, 3)
            .await
            .unwrap();
        assert_eq!(
            shared.iter().map(|row| row.task.id).collect::<Vec<_>>(),
            vec![visible[1]]
        );
    }
}
