//! `/api/v1/indexes` and `/api/v1/jobs`: views and generations from the
//! store, the job queue, reindex requests and dead-letter retries.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use knowell_auth::{Action, Resource};
use knowell_store::hierarchy;
use knowell_store::hierarchy::Organization;
use knowell_store::jobs::{self, JobFilter, JobScope, JobScopeFilter, MAX_JOBS_LISTED, NewJob};
use knowell_store::views::{self, View};
use knowell_store::{GenerationState, JobId, JobState, PgConnection, ViewId};
use serde::Deserialize;

use super::catalog::{organization, parse_id, visible_project};
use crate::engine::Validate;
use crate::error::ApiError;
use crate::events::{EventScope, ProgressEvent};
use crate::extract::{ApiPath, ApiQuery, Caller, JsonBody};
use crate::state::AppState;
use crate::wire::{
    DeadLetterView, GenerationView, IndexView, IndexesOverview, JobView, RefPolicy, ReindexRequest,
    ReindexResult, ViewState,
};

/// Job kind enqueued by `POST /api/v1/indexes/reindex`. Payload (camelCase
/// JSON): `viewId`, `projectId`, `projectName`, `workspaceName`, `scope`
/// (`changed` | `full`), `requestedBy` (principal text).
pub const JOB_KIND_VIEW_REINDEX: &str = "view.reindex";

const GENERATIONS_PER_VIEW: usize = 20;
const MAX_IDEMPOTENCY_KEY: usize = 128;

impl Validate for ReindexRequest {
    fn validate(&self) -> Result<(), ApiError> {
        if self.view_id.is_empty() || self.view_id.len() > 64 {
            return Err(ApiError::invalid("`viewId` must be a view id"));
        }
        Ok(())
    }
}

/// `GET /api/v1/indexes`: views of the visible projects; jobs and dead
/// letters only for callers that may read organization-wide data (job
/// payloads are not attributed to projects).
pub(super) async fn overview(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<IndexesOverview>, ApiError> {
    caller.require_scope(&state, Action::ReadCode)?;
    let store = state.store()?;
    let mut conn = store.acquire().await?;
    let org = organization(&state, &mut conn).await?;
    let mut views_out = Vec::new();
    for workspace in hierarchy::list_workspaces(&mut conn, org.id).await? {
        if !caller.auth.sees_workspace(&workspace.name) {
            continue;
        }
        for project in hierarchy::list_projects(&mut conn, workspace.id).await? {
            if !caller.auth.visible.allows(&workspace.name, &project.name) {
                continue;
            }
            for view in views::list_views(&mut conn, project.id).await? {
                let generations = views::list_generations(&mut conn, view.id).await?;
                views_out.push(index_view(
                    &view,
                    project.name.as_str(),
                    workspace.name.as_str(),
                    &generations,
                ));
            }
        }
    }
    let org_wide = caller
        .auth
        .decide(Action::ReadCode, &Resource::Organization)
        .allowed;
    let (jobs_out, dead) = if org_wide {
        let limit = max_listed(&state);
        let live = list_org_jobs(&mut conn, &org, NOT_DEAD.to_vec(), limit).await?;
        let dead = list_org_jobs(&mut conn, &org, vec![JobState::Dead], limit).await?;
        (
            Some(live.iter().map(JobView::from_job).collect()),
            Some(dead.iter().map(DeadLetterView::from_job).collect()),
        )
    } else {
        (None, None)
    };
    Ok(Json(IndexesOverview {
        views: views_out,
        jobs: jobs_out,
        dead_letters: dead,
        migrations: None,
    }))
}

/// Jobs of this server's organization plus unscoped jobs (enqueued before
/// jobs carried a tenant, or by producers that do not name one), newest
/// first. Callers must be organization-wide readers.
async fn list_org_jobs(
    conn: &mut PgConnection,
    org: &Organization,
    states: Vec<JobState>,
    limit: u32,
) -> Result<Vec<jobs::Job>, ApiError> {
    let filter = JobFilter {
        states,
        scope: JobScopeFilter::organization(org.id, true),
        ..JobFilter::new(limit)
    };
    Ok(jobs::list_jobs(conn, &filter).await?)
}

/// The configured listing limit, capped by what the store returns at once.
fn max_listed(state: &AppState) -> u32 {
    state.config().limits.max_jobs_listed.min(MAX_JOBS_LISTED)
}

const NOT_DEAD: [JobState; 5] = [
    JobState::Queued,
    JobState::Running,
    JobState::Failed,
    JobState::Succeeded,
    JobState::Cancelled,
];

fn index_view(
    view: &View,
    project: &str,
    workspace: &str,
    generations: &[views::ViewGeneration],
) -> IndexView {
    let building = generations
        .iter()
        .any(|g| g.state == GenerationState::Building);
    let (state, note) = if building {
        (
            ViewState::Building,
            Some("a new generation is being built".to_owned()),
        )
    } else if view.active_generation.is_none() {
        let note = match generations.first() {
            Some(g) if g.state == GenerationState::Failed => {
                "no generation is active; the latest one failed".to_owned()
            }
            _ => "no generation has been activated yet".to_owned(),
        };
        (ViewState::NotIndexed, Some(note))
    } else if view.latest_seen_commit.is_some()
        && view.active_commit.is_some()
        && view.latest_seen_commit != view.active_commit
    {
        (
            ViewState::Stale,
            Some("the newest seen commit is not indexed yet".to_owned()),
        )
    } else {
        (ViewState::Ready, None)
    };
    IndexView {
        id: view.id.to_string(),
        project_id: view.project.to_string(),
        project_name: project.to_owned(),
        workspace_name: workspace.to_owned(),
        track_target: RefPolicy::from(&view.target),
        last_seen_commit: view.latest_seen_commit.clone(),
        active_index_commit: view.active_commit.clone(),
        state,
        state_note: note,
        generations: generations
            .iter()
            .take(GENERATIONS_PER_VIEW)
            .map(GenerationView::from_generation)
            .collect(),
        tiers: None,
        analysis: None,
    }
}

/// `GET /api/v1/jobs?state=&limit=` query.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JobsQuery {
    #[serde(default)]
    state: Option<JobState>,
    #[serde(default)]
    limit: Option<u32>,
}

/// `GET /api/v1/jobs`: newest jobs first, optionally of one state
/// (`state=dead` is the dead-letter queue). Organization-wide readers only.
pub(super) async fn list_jobs(
    State(state): State<AppState>,
    caller: Caller,
    ApiQuery(query): ApiQuery<JobsQuery>,
) -> Result<Json<Vec<JobView>>, ApiError> {
    caller.require(&state, Action::ReadCode, Resource::Organization)?;
    let max = max_listed(&state);
    let limit = query.limit.unwrap_or(max);
    if limit == 0 || limit > max {
        return Err(ApiError::invalid(format!(
            "`limit` must be between 1 and {max}"
        )));
    }
    let store = state.store()?;
    let mut conn = store.acquire().await?;
    let org = organization(&state, &mut conn).await?;
    let states: Vec<JobState> = query.state.into_iter().collect();
    let found = list_org_jobs(&mut conn, &org, states, limit).await?;
    Ok(Json(found.iter().map(JobView::from_job).collect()))
}

/// `POST /api/v1/jobs/{id}/retry`: puts a dead job back into the queue.
/// 204 on success, 404 for an unknown job, 409 when it is not dead.
pub(super) async fn retry(
    State(state): State<AppState>,
    caller: Caller,
    ApiPath(id): ApiPath<String>,
) -> Result<Response, ApiError> {
    caller.require(&state, Action::ManageIndex, Resource::Organization)?;
    let id = JobId(parse_id(&id)?);
    let store = state.store()?;
    let mut conn = store.acquire().await?;
    if jobs::get_job(&mut conn, id).await?.is_none() {
        return Err(ApiError::not_found("the job does not exist"));
    }
    if !jobs::requeue_dead(&mut conn, id).await? {
        return Err(ApiError::conflict(
            "not_dead",
            "only dead-lettered jobs can be retried",
        ));
    }
    publish_job(&state, &mut conn, id, EventScope::Organization).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /api/v1/indexes/reindex`: enqueues a `view.reindex` job for the
/// view's project. An `Idempotency-Key` header makes retries return the same
/// job.
pub(super) async fn reindex(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    JsonBody(request): JsonBody<ReindexRequest>,
) -> Result<Response, ApiError> {
    caller.require_scope(&state, Action::ManageIndex)?;
    let view_id = ViewId(parse_id(&request.view_id)?);
    let key = idempotency_key(&headers)?;
    let store = state.store()?;
    let mut conn = store.acquire().await?;
    let org = organization(&state, &mut conn).await?;
    let view = views::get_view(&mut conn, view_id)
        .await?
        .ok_or_else(|| ApiError::not_found("the view does not exist"))?;
    let (workspace, project) = visible_project(&caller, &mut conn, &org, view.project)
        .await
        .map_err(|_| ApiError::not_found("the view does not exist"))?;
    caller.require(
        &state,
        Action::ManageIndex,
        Resource::project(workspace.name.clone(), project.name.clone()),
    )?;
    let mut job = NewJob::new(
        JOB_KIND_VIEW_REINDEX,
        serde_json::json!({
            "viewId": view.id.to_string(),
            "projectId": project.id.to_string(),
            "projectName": project.name.as_str(),
            "workspaceName": workspace.name.as_str(),
            "scope": request.scope,
            "requestedBy": caller.auth.principal.to_string(),
        }),
    );
    // Interactive requests go before bulk indexing.
    job.priority = 10;
    job.idempotency_key =
        key.map(|k| format!("{JOB_KIND_VIEW_REINDEX}:{}:{k}", caller.auth.principal));
    let enqueued = jobs::enqueue_scoped(&mut conn, &job, JobScope::View(view.id)).await?;
    if enqueued.created {
        let scope = EventScope::Project {
            workspace: workspace.name.clone(),
            project: project.name.clone(),
        };
        publish_job(&state, &mut conn, enqueued.id, scope).await;
    }
    let body = ReindexResult {
        job_id: enqueued.id.to_string(),
        created: enqueued.created,
    };
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}

/// The optional `Idempotency-Key` header, `[A-Za-z0-9._:-]{1,128}`.
fn idempotency_key(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let mut values = headers.get_all("idempotency-key").iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    let text = value.to_str().ok().filter(|t| {
        values.next().is_none()
            && !t.is_empty()
            && t.len() <= MAX_IDEMPOTENCY_KEY
            && t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".:_-".contains(&b))
    });
    text.map(|t| Some(t.to_owned())).ok_or_else(|| {
        ApiError::invalid("`Idempotency-Key` must be 1-128 characters of [A-Za-z0-9._:-]")
    })
}

/// Publishes the job's current state; a failed lookup only skips the event.
async fn publish_job(state: &AppState, conn: &mut PgConnection, id: JobId, scope: EventScope) {
    match jobs::get_job(conn, id).await {
        Ok(Some(job)) => {
            state.events().publish(
                scope,
                ProgressEvent::Job {
                    job: JobView::from_job(&job),
                },
            );
        }
        Ok(None) => {}
        Err(err) => tracing::warn!(error = %err, "job event not published"),
    }
}
