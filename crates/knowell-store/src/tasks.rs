//! Tasks and their append-only checkpoints.
//!
//! The domain model (`knowell_knowledge::Task`, `Checkpoint`, `resume`)
//! lives in `knowell-knowledge`; this module persists rows. The list-valued
//! parts of a task (notes, open questions, related files, the view manifest)
//! and a checkpoint's manifest pins are JSON arrays in the engine's own
//! serialisation.
//!
//! - [`create_task`], [`get_task`], [`list_tasks`], [`delete_task`].
//! - [`update_task`] under optimistic concurrency: the caller names the
//!   [`StoredTask::revision`] it read and gets [`StoreError::Conflict`] when
//!   it moved.
//! - [`append_checkpoint`] numbers checkpoints 1, 2, ... per task;
//!   [`list_checkpoints`] and [`latest_checkpoint`] read them back.
//!   Checkpoints are never changed.

use sqlx::{Connection, PgConnection};
use time::OffsetDateTime;

use crate::error::{StoreError, Violation, violation};
use crate::ids::{KnowledgeRecordId, OrganizationId, TaskId, WorkspaceId};
use crate::types::{
    TaskStatus, check_json_array, check_label, check_text, from_i64, from_revision, to_revision,
};

/// Longest task title, in bytes.
pub const MAX_TASK_TITLE_BYTES: usize = 1000;
/// Longest goal or checkpoint summary, in bytes.
pub const MAX_TASK_TEXT_BYTES: usize = 64 * 1024;
/// Longest owner key, next step or symbol id, in bytes.
pub const MAX_TASK_LABEL_BYTES: usize = 256;
/// Longest next step of a checkpoint, in bytes.
pub const MAX_NEXT_STEP_BYTES: usize = 8 * 1024;
/// Most tasks one [`list_tasks`] call returns.
pub const MAX_TASKS_LISTED: u32 = 1000;

/// The list-valued parts of a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDetails {
    /// Progress notes (JSON array).
    pub notes: serde_json::Value,
    /// Decisions taken during the task, as record ids.
    pub decisions: Vec<KnowledgeRecordId>,
    /// Questions, open and resolved (JSON array).
    pub open_questions: serde_json::Value,
    /// Symbol ids the task touches (each 1-256 bytes).
    pub related_symbols: Vec<String>,
    /// Files the task touches (JSON array).
    pub related_files: serde_json::Value,
    /// Source state the task works against (JSON array of manifest pins).
    pub view_manifest: serde_json::Value,
}

impl Default for TaskDetails {
    fn default() -> Self {
        Self {
            notes: serde_json::Value::Array(Vec::new()),
            decisions: Vec::new(),
            open_questions: serde_json::Value::Array(Vec::new()),
            related_symbols: Vec::new(),
            related_files: serde_json::Value::Array(Vec::new()),
            view_manifest: serde_json::Value::Array(Vec::new()),
        }
    }
}

impl TaskDetails {
    fn validate(&self) -> Result<(), StoreError> {
        check_json_array("task notes", &self.notes)?;
        check_json_array("task open questions", &self.open_questions)?;
        check_json_array("task related files", &self.related_files)?;
        check_json_array("task view manifest", &self.view_manifest)?;
        self.related_symbols
            .iter()
            .try_for_each(|s| check_label("related symbol", s, MAX_TASK_LABEL_BYTES))
    }
}

/// A task to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTask {
    /// Identifier chosen by the caller.
    pub id: TaskId,
    /// Owning organization.
    pub organization: OrganizationId,
    /// The workspace the task is about, if one.
    pub workspace: Option<WorkspaceId>,
    /// The user the task belongs to (the engine's user key, 1-256 bytes).
    pub owner: Option<String>,
    /// Title (1-1000 bytes).
    pub title: String,
    /// What done looks like (1 byte to 64 KiB).
    pub goal: String,
    /// Status.
    pub status: TaskStatus,
    /// List-valued parts.
    pub details: TaskDetails,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

/// A stored task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTask {
    /// Identifier.
    pub id: TaskId,
    /// Owning organization.
    pub organization: OrganizationId,
    /// The workspace the task is about, if one.
    pub workspace: Option<WorkspaceId>,
    /// The user the task belongs to.
    pub owner: Option<String>,
    /// Title.
    pub title: String,
    /// Goal.
    pub goal: String,
    /// Status.
    pub status: TaskStatus,
    /// List-valued parts.
    pub details: TaskDetails,
    /// Optimistic-concurrency token, bumped by every update.
    pub revision: u64,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

/// A change to a task, applied by [`update_task`]. Organization, workspace
/// and owner never change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskUpdate {
    /// The task.
    pub id: TaskId,
    /// The revision the change is based on.
    pub expected_revision: u64,
    /// New title.
    pub title: String,
    /// New goal.
    pub goal: String,
    /// New status.
    pub status: TaskStatus,
    /// New list-valued parts.
    pub details: TaskDetails,
    /// Time of this change.
    pub updated_at: OffsetDateTime,
}

/// Keyset position for paging through [`list_tasks`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskCursor {
    /// `updated_at` of the last task seen.
    pub updated_at: OffsetDateTime,
    /// Id of the last task seen (tie-break).
    pub id: TaskId,
}

impl TaskCursor {
    /// The cursor after `task` (pass the last task of a page).
    pub fn after(task: &StoredTask) -> Self {
        Self {
            updated_at: task.updated_at,
            id: task.id,
        }
    }
}

/// What [`list_tasks`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFilter {
    /// The organization (required).
    pub organization: OrganizationId,
    /// Only tasks of this workspace.
    pub workspace: Option<WorkspaceId>,
    /// Only tasks of this owner.
    pub owner: Option<String>,
    /// Tasks in one of these statuses; empty = every status.
    pub statuses: Vec<TaskStatus>,
    /// Most tasks to return, 1..=[`MAX_TASKS_LISTED`].
    pub limit: u32,
    /// Continue after this position.
    pub before: Option<TaskCursor>,
}

impl TaskFilter {
    /// Every task of `organization`, at most `limit`.
    pub fn new(organization: OrganizationId, limit: u32) -> Self {
        Self {
            organization,
            workspace: None,
            owner: None,
            statuses: Vec::new(),
            limit,
            before: None,
        }
    }
}

/// A checkpoint to append.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCheckpoint {
    /// The task.
    pub task: TaskId,
    /// When it was saved.
    pub at: OffsetDateTime,
    /// What has been done and learned (1 byte to 64 KiB).
    pub summary: String,
    /// Decisions taken so far.
    pub decisions: Vec<KnowledgeRecordId>,
    /// What to do next (each 1 byte to 8 KiB).
    pub next_steps: Vec<String>,
    /// The view manifest pins at the time (JSON array).
    pub manifest: serde_json::Value,
}

/// A stored checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// The task.
    pub task: TaskId,
    /// Position in the task's checkpoints, starting at 1.
    pub seq: u64,
    /// When it was saved.
    pub at: OffsetDateTime,
    /// Summary.
    pub summary: String,
    /// Decisions taken so far.
    pub decisions: Vec<KnowledgeRecordId>,
    /// Next steps.
    pub next_steps: Vec<String>,
    /// View manifest pins.
    pub manifest: serde_json::Value,
    /// When the row was written (server clock).
    pub created_at: OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct TaskRow {
    id: TaskId,
    organization_id: OrganizationId,
    workspace_id: Option<WorkspaceId>,
    owner: Option<String>,
    title: String,
    goal: String,
    status: TaskStatus,
    notes: serde_json::Value,
    decisions: Vec<KnowledgeRecordId>,
    open_questions: serde_json::Value,
    related_symbols: Vec<String>,
    related_files: serde_json::Value,
    view_manifest: serde_json::Value,
    revision: i64,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl TryFrom<TaskRow> for StoredTask {
    type Error = StoreError;

    fn try_from(row: TaskRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.id,
            organization: row.organization_id,
            workspace: row.workspace_id,
            owner: row.owner,
            title: row.title,
            goal: row.goal,
            status: row.status,
            details: TaskDetails {
                notes: row.notes,
                decisions: row.decisions,
                open_questions: row.open_questions,
                related_symbols: row.related_symbols,
                related_files: row.related_files,
                view_manifest: row.view_manifest,
            },
            revision: from_revision(row.revision)?,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct CheckpointRow {
    task_id: TaskId,
    seq: i64,
    at: OffsetDateTime,
    summary: String,
    decisions: Vec<KnowledgeRecordId>,
    next_steps: Vec<String>,
    manifest: serde_json::Value,
    created_at: OffsetDateTime,
}

impl TryFrom<CheckpointRow> for Checkpoint {
    type Error = StoreError;

    fn try_from(row: CheckpointRow) -> Result<Self, StoreError> {
        Ok(Self {
            task: row.task_id,
            seq: from_i64(row.seq, "checkpoint number")?,
            at: row.at,
            summary: row.summary,
            decisions: row.decisions,
            next_steps: row.next_steps,
            manifest: row.manifest,
            created_at: row.created_at,
        })
    }
}

macro_rules! task_columns {
    () => {
        "id, organization_id, workspace_id, owner, title, goal, status, notes, decisions,
         open_questions, related_symbols, related_files, view_manifest, revision, created_at,
         updated_at"
    };
}

macro_rules! checkpoint_columns {
    () => {
        "task_id, seq, at, summary, decisions, next_steps, manifest, created_at"
    };
}

fn check_task_text(title: &str, goal: &str) -> Result<(), StoreError> {
    check_text("task title", title, MAX_TASK_TITLE_BYTES, false)?;
    check_text("task goal", goal, MAX_TASK_TEXT_BYTES, false)
}

/// Creates a task. Fails with [`StoreError::AlreadyExists`] for a taken id
/// and [`StoreError::NotFound`] when the organization or workspace (in that
/// organization) does not exist.
pub async fn create_task(
    conn: &mut PgConnection,
    task: &NewTask,
) -> Result<StoredTask, StoreError> {
    check_task_text(&task.title, &task.goal)?;
    task.details.validate()?;
    if let Some(owner) = &task.owner {
        check_label("task owner", owner, MAX_TASK_LABEL_BYTES)?;
    }
    let d = &task.details;
    let row = sqlx::query_as::<_, TaskRow>(concat!(
        "INSERT INTO task (id, organization_id, workspace_id, owner, title, goal, status, notes,
                           decisions, open_questions, related_symbols, related_files,
                           view_manifest, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
         RETURNING ",
        task_columns!()
    ))
    .bind(task.id)
    .bind(task.organization)
    .bind(task.workspace)
    .bind(task.owner.as_deref())
    .bind(&task.title)
    .bind(&task.goal)
    .bind(task.status)
    .bind(&d.notes)
    .bind(&d.decisions)
    .bind(&d.open_questions)
    .bind(&d.related_symbols)
    .bind(&d.related_files)
    .bind(&d.view_manifest)
    .bind(task.created_at)
    .bind(task.updated_at)
    .fetch_one(conn)
    .await
    .map_err(|e| match violation(&e) {
        Some(Violation::Unique(_)) => StoreError::already_exists("task", task.id),
        Some(Violation::ForeignKey(c)) if c.as_deref() == Some("task_workspace_fk") => {
            StoreError::not_found(
                "workspace in this organization",
                task.workspace.map(|w| w.to_string()).unwrap_or_default(),
            )
        }
        Some(Violation::ForeignKey(_)) => StoreError::not_found("organization", task.organization),
        _ => StoreError::Database(e),
    })?;
    row.try_into()
}

/// Looks a task up.
pub async fn get_task(
    conn: &mut PgConnection,
    id: TaskId,
) -> Result<Option<StoredTask>, StoreError> {
    let row = sqlx::query_as::<_, TaskRow>(concat!(
        "SELECT ",
        task_columns!(),
        " FROM task WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(conn)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// Tasks matching `filter`, most recently updated first (ties by id,
/// descending).
pub async fn list_tasks(
    conn: &mut PgConnection,
    filter: &TaskFilter,
) -> Result<Vec<StoredTask>, StoreError> {
    if filter.limit == 0 || filter.limit > MAX_TASKS_LISTED {
        return Err(StoreError::invalid(format!(
            "task listing limit must be between 1 and {MAX_TASKS_LISTED}"
        )));
    }
    let rows = sqlx::query_as::<_, TaskRow>(concat!(
        "SELECT ",
        task_columns!(),
        " FROM task
         WHERE organization_id = $1
           AND ($2::uuid IS NULL OR workspace_id = $2)
           AND ($3::text IS NULL OR owner = $3)
           AND (cardinality($4::task_status[]) = 0 OR status = ANY($4))
           AND ($5::timestamptz IS NULL OR (updated_at, id) < ($5, $6::uuid))
         ORDER BY updated_at DESC, id DESC
         LIMIT $7"
    ))
    .bind(filter.organization)
    .bind(filter.workspace)
    .bind(filter.owner.as_deref())
    .bind(&filter.statuses)
    .bind(filter.before.map(|c| c.updated_at))
    .bind(filter.before.map(|c| c.id))
    .bind(i64::from(filter.limit))
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(TryInto::try_into).collect()
}

/// Applies `update` if the task still has `expected_revision`; otherwise
/// fails with [`StoreError::Conflict`] (or [`StoreError::NotFound`]) and
/// changes nothing. Returns the updated task (revision + 1).
pub async fn update_task(
    conn: &mut PgConnection,
    update: &TaskUpdate,
) -> Result<StoredTask, StoreError> {
    check_task_text(&update.title, &update.goal)?;
    update.details.validate()?;
    let expected = to_revision(update.expected_revision)?;
    let d = &update.details;
    let row = sqlx::query_as::<_, TaskRow>(concat!(
        "UPDATE task
         SET title = $3, goal = $4, status = $5, notes = $6, decisions = $7,
             open_questions = $8, related_symbols = $9, related_files = $10,
             view_manifest = $11, updated_at = $12, revision = revision + 1
         WHERE id = $1 AND revision = $2
         RETURNING ",
        task_columns!()
    ))
    .bind(update.id)
    .bind(expected)
    .bind(&update.title)
    .bind(&update.goal)
    .bind(update.status)
    .bind(&d.notes)
    .bind(&d.decisions)
    .bind(&d.open_questions)
    .bind(&d.related_symbols)
    .bind(&d.related_files)
    .bind(&d.view_manifest)
    .bind(update.updated_at)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(row) = row {
        return row.try_into();
    }
    let found: Option<i64> = sqlx::query_scalar("SELECT revision FROM task WHERE id = $1")
        .bind(update.id)
        .fetch_optional(conn)
        .await?;
    match found {
        None => Err(StoreError::not_found("task", update.id)),
        Some(actual) => Err(StoreError::Conflict {
            entity: "task",
            key: update.id.to_string(),
            detail: format!("expected revision {expected}, found revision {actual}"),
        }),
    }
}

/// Deletes a task with its checkpoints and task-scoped records. Returns
/// whether it existed.
pub async fn delete_task(conn: &mut PgConnection, id: TaskId) -> Result<bool, StoreError> {
    let done = sqlx::query("DELETE FROM task WHERE id = $1")
        .bind(id)
        .execute(conn)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Appends a checkpoint as the task's next number (1 for the first).
/// Concurrent appends to one task are serialised. Fails with
/// [`StoreError::NotFound`] for an unknown task.
pub async fn append_checkpoint(
    conn: &mut PgConnection,
    checkpoint: &NewCheckpoint,
) -> Result<Checkpoint, StoreError> {
    check_text(
        "checkpoint summary",
        &checkpoint.summary,
        MAX_TASK_TEXT_BYTES,
        false,
    )?;
    checkpoint
        .next_steps
        .iter()
        .try_for_each(|s| check_text("checkpoint next step", s, MAX_NEXT_STEP_BYTES, false))?;
    check_json_array("checkpoint manifest", &checkpoint.manifest)?;

    let mut tx = conn.begin().await?;
    // The task row lock orders concurrent appends, so numbers never collide.
    let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM task WHERE id = $1 FOR UPDATE")
        .bind(checkpoint.task)
        .fetch_optional(&mut *tx)
        .await?;
    if exists.is_none() {
        return Err(StoreError::not_found("task", checkpoint.task));
    }
    let row = sqlx::query_as::<_, CheckpointRow>(concat!(
        "INSERT INTO task_checkpoint (task_id, seq, at, summary, decisions, next_steps, manifest)
         SELECT $1, coalesce(max(seq), 0) + 1, $2, $3, $4, $5, $6
         FROM task_checkpoint WHERE task_id = $1
         RETURNING ",
        checkpoint_columns!()
    ))
    .bind(checkpoint.task)
    .bind(checkpoint.at)
    .bind(&checkpoint.summary)
    .bind(&checkpoint.decisions)
    .bind(&checkpoint.next_steps)
    .bind(&checkpoint.manifest)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    row.try_into()
}

/// A task's checkpoints, oldest first. Empty for an unknown task.
pub async fn list_checkpoints(
    conn: &mut PgConnection,
    task: TaskId,
) -> Result<Vec<Checkpoint>, StoreError> {
    let rows = sqlx::query_as::<_, CheckpointRow>(concat!(
        "SELECT ",
        checkpoint_columns!(),
        " FROM task_checkpoint WHERE task_id = $1 ORDER BY seq"
    ))
    .bind(task)
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(TryInto::try_into).collect()
}

/// A task's newest checkpoint, if any.
pub async fn latest_checkpoint(
    conn: &mut PgConnection,
    task: TaskId,
) -> Result<Option<Checkpoint>, StoreError> {
    let row = sqlx::query_as::<_, CheckpointRow>(concat!(
        "SELECT ",
        checkpoint_columns!(),
        " FROM task_checkpoint WHERE task_id = $1 ORDER BY seq DESC LIMIT 1"
    ))
    .bind(task)
    .fetch_optional(conn)
    .await?;
    row.map(TryInto::try_into).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn details_must_be_arrays() {
        assert!(TaskDetails::default().validate().is_ok());
        let bad = TaskDetails {
            notes: serde_json::json!({"not": "a list"}),
            ..TaskDetails::default()
        };
        assert!(bad.validate().is_err());
        let bad = TaskDetails {
            related_symbols: vec![String::new()],
            ..TaskDetails::default()
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn task_text_is_bounded() {
        assert!(check_task_text("t", "g").is_ok());
        assert!(check_task_text("", "g").is_err());
        assert!(check_task_text("t", "").is_err());
        assert!(check_task_text(&"t".repeat(MAX_TASK_TITLE_BYTES + 1), "g").is_err());
    }
}
