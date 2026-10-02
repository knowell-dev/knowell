//! `read_memory`, `write_memory`, `resume_task` and `save_checkpoint`, and
//! the memory and task records they share.

use knowell_core::{Name, RepoPath};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    Validate, check_len, check_limit, check_opt_text, check_text, check_texts, check_unique,
    invalid, limits,
};
use crate::error::ToolError;
use crate::ids::{CheckpointId, CommitId, MemoryId, ResultId, TaskId, Timestamp};
use crate::model::{Evidence, Gap, ProjectView, Target};
use crate::text::UntrustedText;

// ---------------------------------------------------------------------------
// Shared records
// ---------------------------------------------------------------------------

/// Level of a memory scope (architecture §11.1).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ScopeLevel {
    /// Shared terminology, approved rules, general decisions.
    Organization,
    /// Cross-project flows, architecture, shared contracts.
    Workspace,
    /// One project's architecture, run/test knowledge, decisions.
    Project,
    /// One task's goal, progress and decisions.
    Task,
    /// The caller's private notes and preferences.
    User,
}

/// Where a memory record lives. `project` is required for project scope and
/// `task_id` for task scope; neither is allowed for other levels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MemoryScope {
    /// Scope level.
    #[schemars(description = "")]
    pub level: ScopeLevel,
    /// Project, for project scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Project level only")]
    pub project: Option<Name>,
    /// Task, for task scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Task level only")]
    pub task_id: Option<TaskId>,
}

impl Validate for MemoryScope {
    fn validate(&self) -> Result<(), ToolError> {
        match (self.level, &self.project, &self.task_id) {
            (ScopeLevel::Project, Some(_), None) | (ScopeLevel::Task, None, Some(_)) => Ok(()),
            (ScopeLevel::Project, _, _) => Err(invalid(
                "project scope needs `scope.project` and no `scope.task_id`",
            )),
            (ScopeLevel::Task, _, _) => Err(invalid(
                "task scope needs `scope.task_id` and no `scope.project`",
            )),
            (_, None, None) => Ok(()),
            _ => Err(invalid(
                "`scope.project` and `scope.task_id` only apply to project and task scopes",
            )),
        }
    }
}

/// Kind of memory record.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// A decision and its reasons.
    Decision,
    /// An engineering rule (a team rule only once accepted).
    Rule,
    /// An approved example to copy.
    Example,
    /// A finding from investigating code.
    Finding,
    /// A free-form note.
    Note,
    /// An observation extracted from code automatically.
    Observation,
    /// A description of code (model- or agent-written).
    Description,
}

/// Lifecycle state of a memory record (architecture §11.2).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    /// Draft awaiting acceptance; not a rule.
    Proposed,
    /// Accepted by policy or a person.
    Accepted,
    /// Rejected; kept for history.
    Rejected,
    /// The evidence code changed; needs re-evaluation.
    Stale,
    /// Replaced by a newer record.
    Superseded,
}

/// Who wrote a record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Author {
    /// Kind of author.
    pub kind: AuthorKind,
    /// Display name (person, agent client, or `knowell`).
    pub name: String,
    /// Agent session, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// Kind of author.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuthorKind {
    /// A person.
    Human,
    /// An AI agent.
    Agent,
    /// Knowell itself (observations extracted from code).
    System,
}

/// A memory record: scoped, versioned and sourced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MemoryRecord {
    /// Record id.
    pub id: MemoryId,
    /// Version, starting at 1.
    pub version: u32,
    /// Scope.
    pub scope: MemoryScope,
    /// Kind.
    pub kind: MemoryKind,
    /// Lifecycle state; only `accepted` rules are team rules.
    pub status: MemoryStatus,
    /// Title (untrusted, like the body).
    pub title: String,
    /// Body (untrusted).
    pub body: UntrustedText,
    /// Author.
    pub author: Author,
    /// Creation time.
    pub created_at: Timestamp,
    /// Time of the current version.
    pub updated_at: Timestamp,
    /// Related projects.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_projects: Vec<Name>,
    /// Related symbols.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_symbols: Vec<String>,
    /// Code the record is based on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
    /// Newer record that replaces this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<MemoryId>,
    /// Records this one conflicts with (never merged silently).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts_with: Vec<MemoryId>,
}

/// Status of a task.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Being worked on.
    InProgress,
    /// Waiting on something.
    Blocked,
    /// Finished.
    Done,
    /// Abandoned.
    Abandoned,
}

/// A task in a list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskSummary {
    /// Task id; pass to resume_task or save_checkpoint.
    pub task_id: TaskId,
    /// Title.
    pub title: String,
    /// Goal (untrusted).
    pub goal: UntrustedText,
    /// Status.
    pub status: TaskStatus,
    /// Who opened the task.
    pub owner: Author,
    /// Time of the last change.
    pub updated_at: Timestamp,
    /// Latest checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checkpoint: Option<CheckpointId>,
}

// ---------------------------------------------------------------------------
// read_memory
// ---------------------------------------------------------------------------

/// Input of `read_memory`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReadMemoryInput {
    /// Context or workspace.
    #[serde(flatten)]
    pub target: Target,
    /// Read these records directly (other filters still apply).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub ids: Vec<MemoryId>,
    /// Text to look for in titles and bodies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Text in titles and bodies")]
    pub query: Option<String>,
    /// Only these scope levels (all reachable when empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub scopes: Vec<ScopeLevel>,
    /// Only project-scoped records of this project (plus wider scopes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Project-scope filter")]
    pub project: Option<Name>,
    /// Only records of this task (plus wider scopes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "")]
    pub task_id: Option<TaskId>,
    /// Only these kinds (all when empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub kinds: Vec<MemoryKind>,
    /// Only these states (default: accepted and proposed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "default accepted + proposed")]
    pub statuses: Vec<MemoryStatus>,
    /// Maximum records (default 20).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 200))]
    #[schemars(description = "default 20")]
    pub limit: Option<u32>,
}

impl Validate for ReadMemoryInput {
    fn validate(&self) -> Result<(), ToolError> {
        self.target.validate()?;
        check_len("ids", self.ids.len(), limits::MAX_LIST_ITEMS)?;
        check_unique("ids", &self.ids)?;
        check_opt_text("query", self.query.as_deref(), limits::MAX_QUERY_CHARS)?;
        check_len("scopes", self.scopes.len(), limits::MAX_LIST_ITEMS)?;
        check_unique("scopes", &self.scopes)?;
        check_len("kinds", self.kinds.len(), limits::MAX_LIST_ITEMS)?;
        check_unique("kinds", &self.kinds)?;
        check_len("statuses", self.statuses.len(), limits::MAX_LIST_ITEMS)?;
        check_unique("statuses", &self.statuses)?;
        check_limit(self.limit, limits::MAX_LIMIT)
    }
}

/// Output of `read_memory`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReadMemoryOutput {
    /// Records, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub records: Vec<MemoryRecord>,
    /// More records exist beyond `limit`.
    #[serde(default)]
    pub more_available: bool,
    /// Why the result is empty or incomplete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<Gap>,
}

// ---------------------------------------------------------------------------
// write_memory
// ---------------------------------------------------------------------------

/// Input of `write_memory`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WriteMemoryInput {
    /// Context or workspace.
    #[serde(flatten)]
    pub target: Target,
    /// Where the record belongs.
    #[schemars(description = "project for project level, task_id for task level")]
    pub scope: MemoryScope,
    /// Kind of record.
    #[schemars(description = "")]
    pub kind: MemoryKind,
    /// One-line title.
    #[schemars(length(min = 1, max = 200))]
    #[schemars(description = "")]
    pub title: String,
    /// The record; cite code with `evidence`. Never include secrets.
    #[schemars(length(min = 1, max = 20000))]
    #[schemars(description = "Cite code via evidence; no secrets")]
    pub body: String,
    /// Related symbols.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub related_symbols: Vec<String>,
    /// Result ids of the code this record is based on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "Result ids of cited code")]
    pub evidence: Vec<ResultId>,
    /// Record this one replaces (it becomes superseded once accepted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Record it replaces")]
    pub supersedes: Option<MemoryId>,
    /// Same key, same record: makes retries safe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Makes retries safe")]
    pub idempotency_key: Option<String>,
}

impl Validate for WriteMemoryInput {
    fn validate(&self) -> Result<(), ToolError> {
        self.target.validate()?;
        self.scope.validate()?;
        check_text("title", &self.title, limits::MAX_TITLE_CHARS)?;
        if self.title.contains('\n') {
            return Err(invalid("`title` must be a single line"));
        }
        check_text("body", &self.body, limits::MAX_BODY_CHARS)?;
        check_texts(
            "related_symbols",
            &self.related_symbols,
            limits::MAX_LIST_ITEMS,
            limits::MAX_SYMBOL_CHARS,
        )?;
        check_len("evidence", self.evidence.len(), limits::MAX_LIST_ITEMS)?;
        check_unique("evidence", &self.evidence)?;
        check_idempotency_key(self.idempotency_key.as_deref())
    }
}

fn check_idempotency_key(key: Option<&str>) -> Result<(), ToolError> {
    check_opt_text("idempotency_key", key, limits::MAX_IDEMPOTENCY_KEY_CHARS)
}

/// Output of `write_memory`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WriteMemoryOutput {
    /// The stored record (usually `proposed`).
    pub record: MemoryRecord,
    /// False when the idempotency key matched an earlier write.
    pub created: bool,
}

// ---------------------------------------------------------------------------
// resume_task
// ---------------------------------------------------------------------------

/// Input of `resume_task`. Without `task_id` it lists tasks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResumeTaskInput {
    /// Context or workspace.
    #[serde(flatten)]
    pub target: Target,
    /// Task to resume; omit to list tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Omit to list tasks")]
    pub task_id: Option<TaskId>,
    /// When listing: only these states (default: in_progress and blocked).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "Listing only (default in_progress, blocked)")]
    pub statuses: Vec<TaskStatus>,
    /// When listing: text to look for in titles and goals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Listing only")]
    pub query: Option<String>,
    /// Maximum tasks or checkpoints (default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 100))]
    #[schemars(description = "default 10")]
    pub limit: Option<u32>,
}

impl Validate for ResumeTaskInput {
    fn validate(&self) -> Result<(), ToolError> {
        self.target.validate()?;
        if self.task_id.is_some() && (!self.statuses.is_empty() || self.query.is_some()) {
            return Err(invalid(
                "`statuses` and `query` only apply when listing (without `task_id`)",
            ));
        }
        check_len("statuses", self.statuses.len(), limits::MAX_LIST_ITEMS)?;
        check_unique("statuses", &self.statuses)?;
        check_opt_text("query", self.query.as_deref(), limits::MAX_QUERY_CHARS)?;
        check_limit(self.limit, 100)
    }
}

/// Output of `resume_task`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResumeTaskOutput {
    /// Listed tasks (without `task_id`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<TaskSummary>,
    /// The resumed task (with `task_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskDetail>,
    /// Why the result is empty or incomplete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<Gap>,
}

/// Everything needed to continue a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskDetail {
    /// The task.
    pub summary: TaskSummary,
    /// Checkpoints, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<Checkpoint>,
    /// Decisions recorded for the task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<MemoryRecord>,
    /// Open questions (untrusted).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_questions: Vec<UntrustedText>,
    /// Next steps (untrusted).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_steps: Vec<UntrustedText>,
    /// Related symbols.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_symbols: Vec<String>,
    /// View manifest recorded at the last checkpoint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manifest: Vec<ProjectView>,
    /// Source files that changed between that manifest and the context's views.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_since: Vec<SourceChange>,
    /// Task knowledge whose evidence changed since it was written.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale_knowledge: Vec<MemoryRecord>,
}

/// One saved checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Checkpoint {
    /// Checkpoint id.
    pub checkpoint_id: CheckpointId,
    /// 1 for the first checkpoint of a task, then increasing.
    pub sequence: u32,
    /// Save time.
    pub saved_at: Timestamp,
    /// Who saved it.
    pub author: Author,
    /// Progress summary (untrusted).
    pub progress: UntrustedText,
}

/// A source file that changed since a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SourceChange {
    /// Project.
    pub project: Name,
    /// Path in the context's view.
    pub path: RepoPath,
    /// Kind of change.
    pub change: ChangeKind,
    /// Previous path, for renames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_path: Option<RepoPath>,
    /// Commit recorded at the checkpoint.
    pub from_commit: CommitId,
    /// Commit of the context's view.
    pub to_commit: CommitId,
}

/// Kind of source change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// New file.
    Added,
    /// Changed content.
    Modified,
    /// Removed file.
    Deleted,
    /// Moved or renamed.
    Renamed,
}

// ---------------------------------------------------------------------------
// save_checkpoint
// ---------------------------------------------------------------------------

/// Input of `save_checkpoint`. Omit `task_id` to start a new task.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SaveCheckpointInput {
    /// Context or workspace; its view manifest is recorded.
    #[serde(flatten)]
    pub target: Target,
    /// Task to add the checkpoint to; omit to start a new task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Omit to start a new task")]
    pub task_id: Option<TaskId>,
    /// Title of a new task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "New task only")]
    pub title: Option<String>,
    /// Goal of a new task (required without `task_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Required for a new task")]
    pub goal: Option<String>,
    /// What was done since the last checkpoint.
    #[schemars(length(min = 1, max = 20000))]
    #[schemars(description = "")]
    pub progress: String,
    /// Decisions made (stored as proposed task-scope decisions).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "Stored as proposed decisions")]
    pub decisions: Vec<DecisionInput>,
    /// Open questions (replaces the task's list).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "Replaces the list")]
    pub open_questions: Vec<String>,
    /// Next steps (replaces the task's list).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "Replaces the list")]
    pub next_steps: Vec<String>,
    /// Symbols the task touches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub related_symbols: Vec<String>,
    /// New task status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "")]
    pub status: Option<TaskStatus>,
    /// Same key, same checkpoint: makes retries safe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Makes retries safe")]
    pub idempotency_key: Option<String>,
}

/// A decision recorded with a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DecisionInput {
    /// One-line title.
    #[schemars(length(min = 1, max = 200))]
    #[schemars(description = "")]
    pub title: String,
    /// The decision and its reasons.
    #[schemars(length(min = 1, max = 20000))]
    #[schemars(description = "")]
    pub body: String,
    /// Result ids of supporting code.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub evidence: Vec<ResultId>,
}

impl Validate for SaveCheckpointInput {
    fn validate(&self) -> Result<(), ToolError> {
        self.target.validate()?;
        match &self.task_id {
            None => {
                let Some(goal) = &self.goal else {
                    return Err(invalid("`goal` is required to start a new task"));
                };
                check_text("goal", goal, limits::MAX_BODY_CHARS)?;
            }
            Some(_) => {
                if self.title.is_some() || self.goal.is_some() {
                    return Err(invalid(
                        "`title` and `goal` only apply when starting a new task (without `task_id`)",
                    ));
                }
            }
        }
        check_opt_text("title", self.title.as_deref(), limits::MAX_TITLE_CHARS)?;
        check_text("progress", &self.progress, limits::MAX_BODY_CHARS)?;
        check_len("decisions", self.decisions.len(), 20)?;
        for decision in &self.decisions {
            check_text("decisions.title", &decision.title, limits::MAX_TITLE_CHARS)?;
            check_text("decisions.body", &decision.body, limits::MAX_BODY_CHARS)?;
            check_len(
                "decisions.evidence",
                decision.evidence.len(),
                limits::MAX_LIST_ITEMS,
            )?;
        }
        check_texts(
            "open_questions",
            &self.open_questions,
            limits::MAX_LIST_ITEMS,
            limits::MAX_NOTE_CHARS,
        )?;
        check_texts(
            "next_steps",
            &self.next_steps,
            limits::MAX_LIST_ITEMS,
            limits::MAX_NOTE_CHARS,
        )?;
        check_texts(
            "related_symbols",
            &self.related_symbols,
            limits::MAX_LIST_ITEMS,
            limits::MAX_SYMBOL_CHARS,
        )?;
        check_idempotency_key(self.idempotency_key.as_deref())
    }
}

/// Output of `save_checkpoint`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SaveCheckpointOutput {
    /// Task the checkpoint belongs to.
    pub task_id: TaskId,
    /// The saved checkpoint.
    pub checkpoint_id: CheckpointId,
    /// Sequence number within the task.
    pub sequence: u32,
    /// Save time.
    pub saved_at: Timestamp,
    /// True when this call started the task.
    pub created_task: bool,
    /// False when the idempotency key matched an earlier save.
    pub created: bool,
    /// View manifest recorded with the checkpoint.
    pub manifest: Vec<ProjectView>,
    /// Decision records created (proposed, task scope).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<MemoryRecord>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ContextId;

    fn target() -> Target {
        Target::context(ContextId::new("ctx-1").unwrap())
    }

    #[test]
    fn scopes_need_the_right_ids() {
        let project = Some(Name::new("api").unwrap());
        let task = Some(TaskId::new("task-1").unwrap());
        let scope = |level, project: &Option<Name>, task_id: &Option<TaskId>| MemoryScope {
            level,
            project: project.clone(),
            task_id: task_id.clone(),
        };
        assert!(
            scope(ScopeLevel::Project, &project, &None)
                .validate()
                .is_ok()
        );
        assert!(scope(ScopeLevel::Project, &None, &None).validate().is_err());
        assert!(scope(ScopeLevel::Task, &None, &task).validate().is_ok());
        assert!(scope(ScopeLevel::Task, &project, &task).validate().is_err());
        assert!(
            scope(ScopeLevel::Workspace, &None, &None)
                .validate()
                .is_ok()
        );
        assert!(
            scope(ScopeLevel::Workspace, &project, &None)
                .validate()
                .is_err()
        );
        assert!(scope(ScopeLevel::User, &None, &task).validate().is_err());
    }

    #[test]
    fn write_memory_rejects_bad_text() {
        let base = WriteMemoryInput {
            target: target(),
            scope: MemoryScope {
                level: ScopeLevel::Workspace,
                project: None,
                task_id: None,
            },
            kind: MemoryKind::Finding,
            title: "Retries are capped".into(),
            body: "Capture retries stop after 3 attempts.".into(),
            related_symbols: vec![],
            evidence: vec![],
            supersedes: None,
            idempotency_key: None,
        };
        assert!(base.validate().is_ok());
        let two_lines = WriteMemoryInput {
            title: "a\nb".into(),
            ..base.clone()
        };
        assert!(two_lines.validate().is_err());
        let huge = WriteMemoryInput {
            body: "x".repeat(limits::MAX_BODY_CHARS + 1),
            ..base.clone()
        };
        assert!(huge.validate().is_err());
        let long_key = WriteMemoryInput {
            idempotency_key: Some("k".repeat(limits::MAX_IDEMPOTENCY_KEY_CHARS + 1)),
            ..base
        };
        assert!(long_key.validate().is_err());
    }

    #[test]
    fn checkpoint_new_task_needs_goal() {
        let mut input = SaveCheckpointInput {
            target: target(),
            progress: "Mapped the capture flow.".into(),
            ..SaveCheckpointInput::default()
        };
        assert!(input.validate().is_err());
        input.goal = Some("Cap capture retries".into());
        assert!(input.validate().is_ok());
        input.task_id = Some(TaskId::new("task-1").unwrap());
        assert!(input.validate().is_err(), "goal with an existing task");
        input.goal = None;
        assert!(input.validate().is_ok());
    }
}
