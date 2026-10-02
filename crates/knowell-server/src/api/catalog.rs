//! `/api/v1/workspaces` and `/api/v1/projects`: the store's hierarchy, plus
//! effective settings with provenance from `knowell.toml` when its path is
//! configured for the workspace.

use std::path::Path;

use axum::Json;
use axum::extract::State;
use knowell_auth::Action;
use knowell_config::{Origin, ResolvedProject, ResolvedWorkspace, WorkspaceConfig};
use knowell_core::Name;
use knowell_store::hierarchy::{self, Organization, Project, Workspace};
use knowell_store::views::{self, View};
use knowell_store::{PgConnection, ProjectId, SourceKind, WorkspaceId};
use serde::Deserialize;
use time::OffsetDateTime;

use crate::error::ApiError;
use crate::extract::{ApiPath, ApiQuery, Caller};
use crate::state::AppState;
use crate::wire::{
    EmbeddingSettings, ProjectDetail, ProjectSummary, ProjectViewRef, RefPolicy, Setting,
    SourceView, WorkspaceDetail, WorkspaceSummary,
};

const SETTINGS_ERROR: &str =
    "the workspace configuration could not be loaded; run `know doctor` for details";

/// The configured organization, or 503 `not_initialized`.
pub(super) async fn organization(
    state: &AppState,
    conn: &mut PgConnection,
) -> Result<Organization, ApiError> {
    hierarchy::find_organization(conn, &state.config().organization)
        .await?
        .ok_or_else(|| {
            ApiError::unavailable(
                "not_initialized",
                "the organization does not exist yet; run `know init`",
            )
        })
}

/// Parses a store id from a path segment; malformed ids name nothing (404).
pub(super) fn parse_id(text: &str) -> Result<uuid::Uuid, ApiError> {
    uuid::Uuid::parse_str(text)
        .map_err(|_| ApiError::not_found("the requested resource does not exist"))
}

/// Workspace settings loaded from `knowell.toml`.
enum Settings {
    /// No file is configured for the workspace.
    Unknown,
    /// The file could not be loaded or resolved (details are in the log).
    Error,
    Known(Box<(WorkspaceConfig, ResolvedWorkspace)>),
}

async fn workspace_settings(state: &AppState, name: &Name) -> Settings {
    let Some(path) = state.config().workspace_files.get(name).cloned() else {
        return Settings::Unknown;
    };
    match tokio::task::spawn_blocking(move || load_settings(&path)).await {
        Ok(Ok(loaded)) => Settings::Known(Box::new(loaded)),
        Ok(Err(err)) => {
            tracing::warn!(workspace = %name, error = %err, "workspace configuration not loaded");
            Settings::Error
        }
        Err(_) => Settings::Error,
    }
}

fn load_settings(path: &Path) -> Result<(WorkspaceConfig, ResolvedWorkspace), String> {
    let config = knowell_config::load_workspace(path).map_err(|e| e.to_string())?;
    let base = path.parent().unwrap_or(Path::new("."));
    let resolved = config.resolve(base).map_err(|issues| issues.to_string())?;
    Ok((config, resolved))
}

fn origin_note(origin: Origin, workspace: &Name, project: Option<&Name>) -> Option<String> {
    Some(match (origin, project) {
        (Origin::Builtin, _) => "built-in default".to_owned(),
        (Origin::Workspace, _) => format!("workspace \"{workspace}\" in knowell.toml"),
        (Origin::Project, Some(p)) => format!("project \"{p}\" in knowell.toml"),
        (Origin::Project, None) => "project in knowell.toml".to_owned(),
    })
}

fn summary(workspace: &Workspace, project_count: u64, settings: &Settings) -> WorkspaceSummary {
    let mut out = WorkspaceSummary {
        id: workspace.id.to_string(),
        name: workspace.name.to_string(),
        description: None,
        project_count,
        member_count: None,
        tracked_ref: None,
        embedding_profile_id: None,
        data_policy: None,
        created_at: workspace.created_at,
        settings_error: None,
    };
    match settings {
        Settings::Unknown => {}
        Settings::Error => out.settings_error = Some(SETTINGS_ERROR.to_owned()),
        Settings::Known(loaded) => {
            let (config, resolved) = loaded.as_ref();
            out.description = resolved.description.clone();
            out.tracked_ref = config.workspace.track.as_ref().map(RefPolicy::from);
            out.data_policy = Some(config.workspace.data_policy.unwrap_or_default());
        }
    }
    out
}

/// `GET /api/v1/workspaces`: workspaces with at least one visible project
/// (or visible as a whole), by name.
pub(super) async fn list_workspaces(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Vec<WorkspaceSummary>>, ApiError> {
    caller.require_scope(&state, Action::ReadCode)?;
    let store = state.store()?;
    let mut conn = store.acquire().await?;
    let org = organization(&state, &mut conn).await?;
    let mut out = Vec::new();
    for workspace in hierarchy::list_workspaces(&mut conn, org.id).await? {
        if !caller.auth.sees_workspace(&workspace.name) {
            continue;
        }
        let projects = hierarchy::list_projects(&mut conn, workspace.id).await?;
        let count = visible_count(&caller, &workspace, &projects);
        let settings = workspace_settings(&state, &workspace.name).await;
        out.push(summary(&workspace, count, &settings));
    }
    Ok(Json(out))
}

fn visible_count(caller: &Caller, workspace: &Workspace, projects: &[Project]) -> u64 {
    let n = projects
        .iter()
        .filter(|p| caller.auth.visible.allows(&workspace.name, &p.name))
        .count();
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// A visible workspace of this organization by id, or 404.
async fn visible_workspace(
    caller: &Caller,
    conn: &mut PgConnection,
    org: &Organization,
    id: WorkspaceId,
) -> Result<Workspace, ApiError> {
    match hierarchy::get_workspace(conn, id).await? {
        Some(w) if w.organization == org.id && caller.auth.sees_workspace(&w.name) => Ok(w),
        _ => Err(ApiError::not_found("the workspace does not exist")),
    }
}

/// `GET /api/v1/workspaces/{id}`.
pub(super) async fn get_workspace(
    State(state): State<AppState>,
    caller: Caller,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<WorkspaceDetail>, ApiError> {
    caller.require_scope(&state, Action::ReadCode)?;
    let id = WorkspaceId(parse_id(&id)?);
    let store = state.store()?;
    let mut conn = store.acquire().await?;
    let org = organization(&state, &mut conn).await?;
    let workspace = visible_workspace(&caller, &mut conn, &org, id).await?;
    let projects = hierarchy::list_projects(&mut conn, workspace.id).await?;
    let project_ids: Vec<String> = projects
        .iter()
        .filter(|p| caller.auth.visible.allows(&workspace.name, &p.name))
        .map(|p| p.id.to_string())
        .collect();
    let settings = workspace_settings(&state, &workspace.name).await;
    let count = u64::try_from(project_ids.len()).unwrap_or(u64::MAX);
    Ok(Json(WorkspaceDetail {
        summary: summary(&workspace, count, &settings),
        project_ids,
        members: None,
    }))
}

/// `GET /api/v1/projects?workspace=<id>` query.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProjectsQuery {
    #[serde(default)]
    workspace: Option<String>,
}

/// Index facts of a project derived from its views.
struct IndexFacts {
    views: Vec<View>,
    indexed: bool,
    last_indexed_at: Option<OffsetDateTime>,
}

async fn index_facts(conn: &mut PgConnection, project: ProjectId) -> Result<IndexFacts, ApiError> {
    let views = views::list_views(conn, project).await?;
    let mut indexed = false;
    let mut last_indexed_at: Option<OffsetDateTime> = None;
    for view in &views {
        let Some(active) = view.active_generation else {
            continue;
        };
        indexed = true;
        if let Some(generation) = views::get_generation(conn, view.id, active).await? {
            last_indexed_at = match (last_indexed_at, generation.activated_at) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
        }
    }
    Ok(IndexFacts {
        views,
        indexed,
        last_indexed_at,
    })
}

fn project_summary(workspace: &Workspace, project: &Project, facts: &IndexFacts) -> ProjectSummary {
    ProjectSummary {
        id: project.id.to_string(),
        workspace_id: workspace.id.to_string(),
        workspace_name: workspace.name.to_string(),
        name: project.name.to_string(),
        kind: None,
        languages: None,
        indexed: facts.indexed,
        file_count: None,
        last_indexed_at: facts.last_indexed_at,
    }
}

/// `GET /api/v1/projects`: visible projects, by workspace then name.
pub(super) async fn list_projects(
    State(state): State<AppState>,
    caller: Caller,
    ApiQuery(query): ApiQuery<ProjectsQuery>,
) -> Result<Json<Vec<ProjectSummary>>, ApiError> {
    caller.require_scope(&state, Action::ReadCode)?;
    let store = state.store()?;
    let mut conn = store.acquire().await?;
    let org = organization(&state, &mut conn).await?;
    let workspaces = match query.workspace {
        Some(id) => {
            let id = WorkspaceId(
                uuid::Uuid::parse_str(&id)
                    .map_err(|_| ApiError::invalid("`workspace` must be a workspace id"))?,
            );
            vec![visible_workspace(&caller, &mut conn, &org, id).await?]
        }
        None => hierarchy::list_workspaces(&mut conn, org.id).await?,
    };
    let mut out = Vec::new();
    for workspace in workspaces {
        if !caller.auth.sees_workspace(&workspace.name) {
            continue;
        }
        for project in hierarchy::list_projects(&mut conn, workspace.id).await? {
            if !caller.auth.visible.allows(&workspace.name, &project.name) {
                continue;
            }
            let facts = index_facts(&mut conn, project.id).await?;
            out.push(project_summary(&workspace, &project, &facts));
        }
    }
    Ok(Json(out))
}

/// A visible project of this organization by id, with its workspace, or 404.
pub(super) async fn visible_project(
    caller: &Caller,
    conn: &mut PgConnection,
    org: &Organization,
    id: ProjectId,
) -> Result<(Workspace, Project), ApiError> {
    let not_found = || ApiError::not_found("the project does not exist");
    let project = hierarchy::get_project(conn, id)
        .await?
        .filter(|p| p.organization == org.id)
        .ok_or_else(not_found)?;
    let workspace = hierarchy::get_workspace(conn, project.workspace)
        .await?
        .ok_or_else(not_found)?;
    if !caller.auth.visible.allows(&workspace.name, &project.name) {
        return Err(not_found());
    }
    Ok((workspace, project))
}

/// `GET /api/v1/projects/{id}`.
pub(super) async fn get_project(
    State(state): State<AppState>,
    caller: Caller,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<ProjectDetail>, ApiError> {
    caller.require_scope(&state, Action::ReadCode)?;
    let id = ProjectId(parse_id(&id)?);
    let store = state.store()?;
    let mut conn = store.acquire().await?;
    let org = organization(&state, &mut conn).await?;
    let (workspace, project) = visible_project(&caller, &mut conn, &org, id).await?;
    let source = hierarchy::get_source(&mut conn, project.source)
        .await?
        .ok_or_else(|| ApiError::not_found("the project's source does not exist"))?;
    let facts = index_facts(&mut conn, project.id).await?;
    let settings = workspace_settings(&state, &workspace.name).await;

    let root = match &project.root {
        Some(root) => Setting {
            value: root.as_str().to_owned(),
            origin: Origin::Project,
            origin_note: origin_note(Origin::Project, &workspace.name, Some(&project.name)),
        },
        None => Setting {
            value: String::new(),
            origin: Origin::Builtin,
            origin_note: Some("the source root".to_owned()),
        },
    };
    let views = facts
        .views
        .iter()
        .map(|v| ProjectViewRef {
            id: v.id.to_string(),
            track_target: RefPolicy::from(&v.target),
            active_index_commit: v.active_commit.clone(),
            last_seen_commit: v.latest_seen_commit.clone(),
        })
        .collect();
    let mut detail = ProjectDetail {
        summary: project_summary(&workspace, &project, &facts),
        source: match source.kind {
            SourceKind::Git => SourceView::Git {
                remote: source.location.clone(),
            },
            SourceKind::Directory => SourceView::Local {
                path: source.location.clone(),
            },
        },
        root,
        tracked_ref: None,
        excludes: None,
        embedding: None,
        embedding_profile_id: None,
        data_policy: None,
        analysis: None,
        worktrees: None,
        sensitive_excluded_count: None,
        views,
        settings_error: None,
    };
    match &settings {
        Settings::Unknown => {}
        Settings::Error => detail.settings_error = Some(SETTINGS_ERROR.to_owned()),
        Settings::Known(loaded) => {
            let (_, resolved) = loaded.as_ref();
            match resolved.projects.iter().find(|p| p.name == project.name) {
                Some(rp) => apply_resolved(&mut detail, rp, &workspace.name),
                None => {
                    detail.settings_error = Some(
                        "the project is not listed in the workspace's knowell.toml".to_owned(),
                    );
                }
            }
        }
    }
    Ok(Json(detail))
}

fn apply_resolved(detail: &mut ProjectDetail, rp: &ResolvedProject, workspace: &Name) {
    let note = |origin| origin_note(origin, workspace, Some(&rp.name));
    detail.tracked_ref = Some(Setting {
        value: RefPolicy::from(&rp.track.value),
        origin: rp.track.origin,
        origin_note: note(rp.track.origin),
    });
    detail.excludes = Some(
        rp.exclude
            .iter()
            .map(|e| Setting {
                value: e.value.clone(),
                origin: e.origin,
                origin_note: note(e.origin),
            })
            .collect(),
    );
    detail.data_policy = Some(Setting {
        value: rp.data_policy.value,
        origin: rp.data_policy.origin,
        origin_note: note(rp.data_policy.origin),
    });
    let e = &rp.embedding;
    detail.embedding = Some(EmbeddingSettings {
        provider: e.provider.as_ref().map(|p| Setting {
            value: p.value.to_string(),
            origin: p.origin,
            origin_note: note(p.origin),
        }),
        model: e.model.as_ref().map(|m| Setting {
            value: m.value.clone(),
            origin: m.origin,
            origin_note: note(m.origin),
        }),
        preset: Setting {
            value: e.preset.value.as_str().to_owned(),
            origin: e.preset.origin,
            origin_note: note(e.preset.origin),
        },
        dimensions: Setting {
            value: e.dimensions.value,
            origin: e.dimensions.origin,
            origin_note: note(e.dimensions.origin),
        },
    });
}
