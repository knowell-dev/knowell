//! `open_workspace` and `index_status`.

use knowell_core::{Name, RepoPath, TrackTarget};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    Validate, check_len, check_opt_text, check_range, check_unique, check_view_pins, limits,
};
use crate::error::ToolError;
use crate::ids::{CommitId, ContextId, JobId, Timestamp};
use crate::model::view_map;
use crate::model::{
    AnalysisLevel, FreshnessTier, Gap, IndexState, JobState, ProjectView, Target, ViewLayer,
    ViewPin,
};
use crate::tools::memory::{MemoryRecord, TaskSummary};

// ---------------------------------------------------------------------------
// open_workspace
// ---------------------------------------------------------------------------

/// Input of `open_workspace`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OpenWorkspaceInput {
    /// Optional when only one workspace is reachable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Optional if only one is reachable")]
    pub workspace: Option<Name>,
    /// Project to ref (branch:x, tag:x, commit:sha or worktree); others use their tracked ref.
    #[schemars(description = "project to ref; others use their tracked ref")]
    #[serde(default, skip_serializing_if = "Vec::is_empty", with = "view_map")]
    #[schemars(schema_with = "view_map::schema")]
    pub views: Vec<ViewPin>,
    /// Your absolute working directory (selects project and worktree layer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Your absolute working directory")]
    pub working_directory: Option<String>,
    /// Token budget of the start-up pack (default 2000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 200, max = 20000))]
    #[schemars(description = "Start-up pack token budget (default 2000)")]
    pub summary_budget_tokens: Option<u32>,
}

impl Validate for OpenWorkspaceInput {
    fn validate(&self) -> Result<(), ToolError> {
        check_view_pins(&self.views)?;
        check_opt_text(
            "working_directory",
            self.working_directory.as_deref(),
            limits::MAX_PATH_HINT_CHARS,
        )?;
        check_range(
            "summary_budget_tokens",
            self.summary_budget_tokens,
            200,
            20_000,
        )
    }
}

/// Output of `open_workspace`: the context id and the start-up pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OpenWorkspaceOutput {
    /// Pass this to every other tool.
    pub context_id: ContextId,
    /// Opened workspace.
    pub workspace: Name,
    /// Project detected from `working_directory`, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_project: Option<Name>,
    /// Pinned view of every project (the view manifest).
    pub manifest: Vec<ProjectView>,
    /// Projects and their roles.
    pub projects: Vec<ProjectInfo>,
    /// Accepted rules in scope (budgeted).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<MemoryRecord>,
    /// Open tasks (resume one with resume_task).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_tasks: Vec<TaskSummary>,
    /// Recent decisions (budgeted, newest first).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_decisions: Vec<MemoryRecord>,
    /// Coverage gaps (unindexed projects, missing refs, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<Gap>,
}

/// A project in a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectInfo {
    /// Project name.
    pub name: Name,
    /// Short description from the workspace configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Roles, e.g. `backend`, `web-client`, `worker`, `sdk`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// Main languages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<String>,
    /// Root inside a monorepo, when the project is not a whole repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<RepoPath>,
    /// Ref the project tracks by policy.
    pub tracks: TrackTarget,
}

// ---------------------------------------------------------------------------
// index_status
// ---------------------------------------------------------------------------

/// Input of `index_status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IndexStatusInput {
    /// Context or workspace to report on.
    #[serde(flatten)]
    pub target: Target,
    /// Only these projects (default all).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub projects: Vec<Name>,
    /// Jobs to report on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub job_ids: Vec<JobId>,
}

impl Validate for IndexStatusInput {
    fn validate(&self) -> Result<(), ToolError> {
        self.target.validate()?;
        check_len("projects", self.projects.len(), limits::MAX_LIST_ITEMS)?;
        check_unique("projects", &self.projects)?;
        check_len("job_ids", self.job_ids.len(), limits::MAX_LIST_ITEMS)?;
        check_unique("job_ids", &self.job_ids)
    }
}

/// Output of `index_status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IndexStatusOutput {
    /// Per-project index status.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<ProjectIndexStatus>,
    /// Requested and currently running jobs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<JobInfo>,
    /// Why something is missing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<Gap>,
}

/// Index status of one project view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectIndexStatus {
    /// Project.
    pub project: Name,
    /// Ref the view follows.
    pub tracking: TrackTarget,
    /// Shared index or personal worktree layer.
    pub layer: ViewLayer,
    /// Latest commit seen on the tracked ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_seen_commit: Option<CommitId>,
    /// Commit of the index currently serving queries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexed_commit: Option<CommitId>,
    /// Overall state.
    pub state: IndexState,
    /// State of each analysis tier.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<TierStatus>,
    /// Coverage per language.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<LanguageCoverage>,
    /// Embedding profile of the view, when embeddings are enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_profile: Option<String>,
    /// When the serving index was activated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_indexed_at: Option<Timestamp>,
    /// Short note, e.g. why indexing is paused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// State of one analysis tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TierStatus {
    /// Tier.
    pub tier: FreshnessTier,
    /// State.
    pub state: TierState,
    /// Files processed for this tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_done: Option<u64>,
    /// Files this tier will process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_total: Option<u64>,
}

/// State of an analysis tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TierState {
    /// Up to date for the serving view.
    Ready,
    /// Being built.
    Building,
    /// Waiting in the queue.
    Queued,
    /// Cannot run (e.g. embedding provider unreachable).
    Unavailable,
    /// Turned off by configuration.
    Disabled,
}

/// Coverage of one language in a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LanguageCoverage {
    /// Language name, e.g. `typescript`.
    pub language: String,
    /// Indexed files.
    pub files: u64,
    /// Analysis depth.
    pub analysis: AnalysisLevel,
}

/// Status of a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JobInfo {
    /// Job id.
    pub job_id: JobId,
    /// What the job does.
    pub kind: JobKind,
    /// Current state.
    pub state: JobState,
    /// Progress from 0 to 100, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(max = 100))]
    pub progress_percent: Option<u8>,
    /// Project the job works on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<Name>,
    /// Start time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Timestamp>,
    /// End time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<Timestamp>,
    /// Short note, e.g. a failure reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// What a job does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// Text, symbol and lexical indexing.
    Index,
    /// Embedding computation.
    Embed,
    /// Relation and cross-project link analysis.
    Relations,
    /// `analyze_impact` computation.
    ImpactAnalysis,
    /// `build_context` computation.
    ContextPack,
    /// `trace_flow` computation.
    Trace,
}
