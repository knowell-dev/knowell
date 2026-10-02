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
}

/// Everything in process memory; lost when the process ends.
#[derive(Debug, Default)]
pub struct InMemoryMemory {
    state: Mutex<InMemoryState>,
}

#[derive(Debug, Default)]
struct InMemoryState {
    records: BTreeMap<RecordId, RecordRow>,
    tasks: BTreeMap<TaskId, TaskRow>,
    checkpoints: BTreeMap<TaskId, Vec<CheckpointRow>>,
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
        let result = self.with_state(|state| {
            if state.records.contains_key(&record.id) {
                return Err(MemoryError::AlreadyExists(format!("record {}", record.id)));
            }
            let row = RecordRow {
                record: record.clone(),
                revision: 1,
            };
            state.records.insert(record.id, row.clone());
            Ok(row)
        });
        Box::pin(std::future::ready(result))
    }

    fn update_record<'a>(
        &'a self,
        before: &'a RecordRow,
        after: &'a KnowledgeRecord,
    ) -> BoxFuture<'a, Result<RecordRow, MemoryError>> {
        let result = self.with_state(|state| {
            let Some(current) = state.records.get_mut(&before.record.id) else {
                return Err(MemoryError::NotFound(format!(
                    "record {}",
                    before.record.id
                )));
            };
            if current.revision != before.revision {
                return Err(MemoryError::Conflict(format!(
                    "record {} changed since it was read",
                    before.record.id
                )));
            }
            current.record = after.clone();
            current.revision = current.revision.saturating_add(1);
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

    fn update_task<'a>(
        &'a self,
        before: &'a TaskRow,
        after: &'a Task,
    ) -> BoxFuture<'a, Result<TaskRow, MemoryError>> {
        let result = self.with_state(|state| {
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
        });
        Box::pin(std::future::ready(result))
    }

    fn append_checkpoint<'a>(
        &'a self,
        checkpoint: &'a Checkpoint,
    ) -> BoxFuture<'a, Result<u64, MemoryError>> {
        let result = self.with_state(|state| {
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
        });
        Box::pin(std::future::ready(result))
    }

    fn checkpoints(&self, task: TaskId) -> BoxFuture<'_, Result<Vec<CheckpointRow>, MemoryError>> {
        let rows =
            self.with_state(|state| state.checkpoints.get(&task).cloned().unwrap_or_default());
        Box::pin(std::future::ready(Ok(rows)))
    }
}

#[cfg(test)]
mod tests {
    use knowell_knowledge::{Actor, NewRecord, Subject, Timestamp};

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
}
