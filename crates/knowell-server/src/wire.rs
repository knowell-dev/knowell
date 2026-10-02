//! JSON shapes the server itself produces (the panel's `types.ts` mirrors
//! them). Field names are camelCase; timestamps are RFC 3339 UTC strings;
//! durations are milliseconds. A field the server cannot know yet is `null`,
//! never a made-up value.

use knowell_config::{DataPolicy, Origin, ServerRole};
use knowell_core::TrackTarget;
use knowell_store::jobs::Job;
use knowell_store::views::ViewGeneration;
use knowell_store::{GenerationState, JobState};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// `GET /api/v1/session`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    /// The acting principal (`user:<uuid>`).
    pub user: String,
    /// Role of this server.
    pub role: ServerRole,
    /// Token to echo in `X-Knowell-CSRF` on state-changing requests.
    pub csrf_token: String,
    /// When the session ends at the latest.
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// Health of one component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    /// Working.
    Ok,
    /// Working with limitations.
    Degraded,
    /// Not working.
    Down,
}

/// One component in [`EngineHealth`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComponentHealth {
    /// Component name (`database`, `engine`, `panel`, …).
    pub name: String,
    /// Its status.
    pub status: HealthStatus,
    /// Why, in a short secret-free sentence.
    pub detail: String,
}

/// Queue counts from the durable job queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueStats {
    /// Jobs waiting to run.
    pub queued: u64,
    /// Jobs running under a lease.
    pub running: u64,
    /// Jobs whose last attempt failed and that will be retried.
    pub failed: u64,
    /// Jobs that used up their attempts.
    pub dead_letter: u64,
    /// Age of the oldest queued job in milliseconds; `null` when none is queued.
    pub oldest_queued_ms: Option<u64>,
}

/// `GET /api/v1/health`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineHealth {
    /// Worst status of the components.
    pub status: HealthStatus,
    /// Version of the running binary.
    pub version: String,
    /// Role of this server.
    pub role: ServerRole,
    /// Milliseconds since the server state was built.
    pub uptime_ms: u64,
    /// Configured listen address.
    pub bind_address: String,
    /// Component statuses.
    pub components: Vec<ComponentHealth>,
    /// Queue counts; `null` without a database.
    pub queue: Option<QueueStats>,
    /// Per-tier freshness from the engine; `null` when the engine cannot say.
    pub freshness: Option<serde_json::Value>,
    /// Recent errors from the engine; `null` when the engine cannot say.
    pub recent_errors: Option<serde_json::Value>,
    /// Resource use from the engine; `null` when the engine cannot say.
    pub resources: Option<serde_json::Value>,
}

/// A track target in the panel's shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RefPolicy {
    /// `branch`, `remote`, `tag`, `sha` or `worktree-head`.
    pub kind: &'static str,
    /// Branch, `remote/branch`, tag or commit id; empty for `worktree-head`.
    pub name: String,
    /// The canonical text form (`branch:main`).
    pub text: String,
}

impl From<&TrackTarget> for RefPolicy {
    fn from(target: &TrackTarget) -> Self {
        let (kind, name) = match target {
            TrackTarget::Branch(b) => ("branch", b.clone()),
            TrackTarget::Remote { remote, branch } => ("remote", format!("{remote}/{branch}")),
            TrackTarget::Tag(t) => ("tag", t.clone()),
            TrackTarget::Commit(c) => ("sha", c.clone()),
            TrackTarget::WorktreeHead => ("worktree-head", String::new()),
        };
        Self {
            kind,
            name,
            text: target.to_string(),
        }
    }
}

/// A value together with where it was configured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Setting<T> {
    /// The effective value.
    pub value: T,
    /// `builtin`, `workspace` or `project`.
    pub origin: Origin,
    /// Human readable pointer to the defining layer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_note: Option<String>,
}

/// `GET /api/v1/workspaces` item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSummary {
    /// Store id.
    pub id: String,
    /// Workspace name.
    pub name: String,
    /// From `knowell.toml`; `null` when unknown.
    pub description: Option<String>,
    /// Projects visible to the caller.
    pub project_count: u64,
    /// Not tracked yet: always `null`.
    pub member_count: Option<u64>,
    /// Workspace-level track target from `knowell.toml`; `null` when unknown or unset.
    pub tracked_ref: Option<RefPolicy>,
    /// Not tracked yet: always `null`.
    pub embedding_profile_id: Option<String>,
    /// Workspace-level data policy (built-in default when unset); `null` when the file is unknown.
    pub data_policy: Option<DataPolicy>,
    /// Creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Set when `knowell.toml` is configured but could not be loaded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings_error: Option<String>,
}

/// `GET /api/v1/workspaces/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDetail {
    /// Summary fields.
    #[serde(flatten)]
    pub summary: WorkspaceSummary,
    /// Ids of the visible projects, by name.
    pub project_ids: Vec<String>,
    /// Not tracked yet: always `null`.
    pub members: Option<Vec<serde_json::Value>>,
}

/// `GET /api/v1/projects` item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSummary {
    /// Store id.
    pub id: String,
    /// Workspace store id.
    pub workspace_id: String,
    /// Workspace name.
    pub workspace_name: String,
    /// Project name.
    pub name: String,
    /// Not tracked yet: always `null`.
    pub kind: Option<String>,
    /// Not tracked yet: always `null`.
    pub languages: Option<Vec<String>>,
    /// Whether any view of the project has an active generation.
    pub indexed: bool,
    /// Not tracked yet: always `null`.
    pub file_count: Option<u64>,
    /// When the newest active generation was activated.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_indexed_at: Option<OffsetDateTime>,
}

/// Where a project's files come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SourceView {
    /// A git repository: `remote` is its registered location (clone URL or path).
    Git {
        /// Registered location.
        remote: String,
    },
    /// A plain directory.
    Local {
        /// Directory path.
        path: String,
    },
}

/// Effective embedding settings of a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingSettings {
    /// Provider name; `null` when no layer names one (lexical and graph search only).
    pub provider: Option<Setting<String>>,
    /// Model; `null` means the provider's default model.
    pub model: Option<Setting<String>>,
    /// Size preset.
    pub preset: Setting<String>,
    /// Vector dimensions.
    pub dimensions: Setting<u32>,
}

/// A view of a project, as listed in [`ProjectDetail`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectViewRef {
    /// View store id.
    pub id: String,
    /// Track target.
    pub track_target: RefPolicy,
    /// Commit of the active generation.
    pub active_index_commit: Option<String>,
    /// Newest commit seen on the target.
    pub last_seen_commit: Option<String>,
}

/// `GET /api/v1/projects/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectDetail {
    /// Summary fields.
    #[serde(flatten)]
    pub summary: ProjectSummary,
    /// Registered source.
    pub source: SourceView,
    /// Sub-root inside the source (empty for the source root).
    pub root: Setting<String>,
    /// From `knowell.toml`; `null` when the workspace file is unknown.
    pub tracked_ref: Option<Setting<RefPolicy>>,
    /// Effective exclude globs, each with its own origin; `null` when unknown.
    pub excludes: Option<Vec<Setting<String>>>,
    /// Effective embedding settings; `null` when unknown.
    pub embedding: Option<EmbeddingSettings>,
    /// Not tracked yet: always `null`.
    pub embedding_profile_id: Option<Setting<String>>,
    /// Effective data policy; `null` when unknown.
    pub data_policy: Option<Setting<DataPolicy>>,
    /// Not tracked yet: always `null`.
    pub analysis: Option<Setting<String>>,
    /// Not tracked yet: always `null`.
    pub worktrees: Option<Vec<serde_json::Value>>,
    /// Not tracked yet: always `null`.
    pub sensitive_excluded_count: Option<u64>,
    /// The project's views.
    pub views: Vec<ProjectViewRef>,
    /// Set when `knowell.toml` is configured but could not be loaded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings_error: Option<String>,
}

/// One view generation.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationView {
    /// Generation number within its view.
    pub id: i64,
    /// `building`, `active`, `retired` or `failed`.
    pub state: GenerationState,
    /// Commit being indexed; `null` for directory sources.
    pub commit: Option<String>,
    /// Not tracked per view generation: always `null`.
    pub profile_id: Option<String>,
    /// Creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Activation time.
    #[serde(with = "time::serde::rfc3339::option")]
    pub activated_at: Option<OffsetDateTime>,
    /// When it reached a final state.
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    /// Not tracked per view generation: `null` unless the indexer reports it.
    pub chunk_count: Option<u64>,
    /// 0..1 while building, when the indexer reports it.
    pub progress: Option<f64>,
    /// Why a failed generation failed.
    pub error: Option<String>,
}

impl GenerationView {
    /// The wire form of a stored generation.
    pub fn from_generation(generation: &ViewGeneration) -> Self {
        Self {
            id: generation.generation,
            state: generation.state,
            commit: generation.resolved_commit.clone(),
            profile_id: None,
            created_at: generation.created_at,
            activated_at: generation.activated_at,
            finished_at: generation.finished_at,
            chunk_count: None,
            progress: None,
            error: generation.error.clone(),
        }
    }
}

/// Summary state of a view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewState {
    /// The active generation covers the newest seen commit.
    Ready,
    /// A generation is being built.
    Building,
    /// The newest seen commit is not indexed yet.
    Stale,
    /// No generation was ever activated.
    NotIndexed,
}

/// `GET /api/v1/indexes` view entry.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexView {
    /// View store id.
    pub id: String,
    /// Project store id.
    pub project_id: String,
    /// Project name.
    pub project_name: String,
    /// Workspace name.
    pub workspace_name: String,
    /// What the view follows.
    pub track_target: RefPolicy,
    /// Newest commit seen on the target.
    pub last_seen_commit: Option<String>,
    /// Commit of the active generation.
    pub active_index_commit: Option<String>,
    /// Summary state.
    pub state: ViewState,
    /// Why, when not ready.
    pub state_note: Option<String>,
    /// Newest generations first (at most 20).
    pub generations: Vec<GenerationView>,
    /// Not tracked yet: always `null`.
    pub tiers: Option<Vec<serde_json::Value>>,
    /// Not tracked yet: always `null`.
    pub analysis: Option<Vec<serde_json::Value>>,
}

/// A job, in lists and progress events.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobView {
    /// Job id.
    pub id: String,
    /// Job kind (`source.refresh`, `view.reindex`, …).
    pub kind: String,
    /// Queue state.
    pub state: JobState,
    /// `projectName` from the job payload, when present.
    pub project_name: Option<String>,
    /// `workspaceName` from the job payload, when present.
    pub workspace_name: Option<String>,
    /// Attempts started.
    pub attempts: u32,
    /// Attempts allowed.
    pub max_attempts: u32,
    /// Enqueue time.
    #[serde(with = "time::serde::rfc3339")]
    pub enqueued_at: OffsetDateTime,
    /// Last state change.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// 0..1 when the worker reports progress.
    pub progress: Option<f64>,
    /// Error of the last failed attempt.
    pub error: Option<String>,
}

impl JobView {
    /// The wire form of a stored job (the payload itself is not exposed).
    pub fn from_job(job: &Job) -> Self {
        Self {
            id: job.id.to_string(),
            kind: job.kind.clone(),
            state: job.state,
            project_name: payload_text(&job.payload, "projectName"),
            workspace_name: payload_text(&job.payload, "workspaceName"),
            attempts: job.attempts,
            max_attempts: job.max_attempts,
            enqueued_at: job.created_at,
            updated_at: job.updated_at,
            progress: None,
            error: job.last_error.clone(),
        }
    }
}

/// A dead-lettered job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeadLetterView {
    /// Job id (retry with `POST /api/v1/jobs/{id}/retry`).
    pub job_id: String,
    /// Job kind.
    pub kind: String,
    /// `projectName` from the job payload, when present.
    pub project_name: Option<String>,
    /// `workspaceName` from the job payload, when present.
    pub workspace_name: Option<String>,
    /// When it was dead-lettered.
    #[serde(with = "time::serde::rfc3339")]
    pub failed_at: OffsetDateTime,
    /// Attempts used.
    pub attempts: u32,
    /// Error of the last attempt.
    pub error: Option<String>,
}

impl DeadLetterView {
    /// The wire form of a dead job.
    pub fn from_job(job: &Job) -> Self {
        Self {
            job_id: job.id.to_string(),
            kind: job.kind.clone(),
            project_name: payload_text(&job.payload, "projectName"),
            workspace_name: payload_text(&job.payload, "workspaceName"),
            failed_at: job.finished_at.unwrap_or(job.updated_at),
            attempts: job.attempts,
            error: job.last_error.clone(),
        }
    }
}

fn payload_text(payload: &serde_json::Value, key: &str) -> Option<String> {
    payload.get(key).and_then(|v| v.as_str()).map(str::to_owned)
}

/// `GET /api/v1/indexes`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexesOverview {
    /// Views of the visible projects.
    pub views: Vec<IndexView>,
    /// Recent jobs (newest first); `null` when the caller cannot read
    /// organization-wide data.
    pub jobs: Option<Vec<JobView>>,
    /// Dead-lettered jobs (newest first); `null` like `jobs`.
    pub dead_letters: Option<Vec<DeadLetterView>>,
    /// Profile migrations are reported by the engine: always `null` here.
    pub migrations: Option<Vec<serde_json::Value>>,
}

/// Scope of a reindex request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReindexScope {
    /// Only what changed since the active generation.
    Changed,
    /// Everything.
    Full,
}

/// `POST /api/v1/indexes/reindex` body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReindexRequest {
    /// View store id.
    pub view_id: String,
    /// What to rebuild.
    pub scope: ReindexScope,
}

/// `POST /api/v1/indexes/reindex` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReindexResult {
    /// The queued job (an existing one for a repeated `Idempotency-Key`).
    pub job_id: String,
    /// Whether this request created the job.
    pub created: bool,
}

/// Answer of a webhook delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookAck {
    /// Whether a refresh job is queued for this delivery.
    pub accepted: bool,
    /// The job (new, or the existing one for a redelivery).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// Whether this delivery created the job.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<bool>,
    /// Why nothing was queued.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

/// `GET /api/v1/health/live`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Liveness {
    /// Always `ok` when the process answers.
    pub status: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_policy_kinds() {
        let cases = [
            ("branch:main", "branch", "main"),
            ("remote:origin/dev", "remote", "origin/dev"),
            ("tag:v1.0.0", "tag", "v1.0.0"),
            ("worktree", "worktree-head", ""),
        ];
        for (text, kind, name) in cases {
            let target: TrackTarget = text.parse().unwrap();
            let policy = RefPolicy::from(&target);
            assert_eq!((policy.kind, policy.name.as_str()), (kind, name));
            assert_eq!(policy.text, text);
        }
        let sha = "a".repeat(40);
        let target: TrackTarget = format!("commit:{sha}").parse().unwrap();
        assert_eq!(RefPolicy::from(&target).kind, "sha");
    }

    #[test]
    fn view_state_serialises_kebab_case() {
        assert_eq!(
            serde_json::to_value(ViewState::NotIndexed).unwrap(),
            serde_json::json!("not-indexed")
        );
    }
}
