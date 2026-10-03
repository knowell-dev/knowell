//! Pinning what a request may see: the view manifest (one generation and
//! commit per visible project), personal overlays, and `context_id`s.
//!
//! Permission enforcement point 1: only projects the caller may read are
//! ever pinned. A project the caller cannot see is reported exactly like a
//! project that does not exist. A context is bound to the identity that
//! opened it and is re-filtered against the caller's current grants on every
//! use, so revoked access takes effect immediately.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use knowell_auth::{Action, Resource, UserId};
use knowell_core::{Name, TrackTarget};
use knowell_index::{EmbeddingPlan, GitConfigMode, Overlay, TierState, TierStates};
use knowell_mcp::{
    CommitId, ContextId, FreshnessTier, Gap, GapReason, IndexState, ProjectView, Target, ToolError,
    ViewLayer, ViewPin,
};
use knowell_source::git::{GitError, GitRepo};
use knowell_store::views::{self, GenerationPin};
use knowell_store::{ProfileId, SourceKind, ViewId};
use time::OffsetDateTime;

use crate::access::Access;
use crate::engine::{Engine, ProjectEntry, WorkspaceEntry};
use crate::error::store_tool;

/// A personal overlay pinned for one project.
#[derive(Debug, Clone)]
pub(crate) struct PinnedOverlay {
    /// The user whose worktree it is.
    pub(crate) owner: UserId,
    pub(crate) overlay: Arc<Overlay>,
    /// Engine-local generation of this overlay build.
    pub(crate) generation: u64,
}

/// One project's pinned view.
#[derive(Debug, Clone)]
pub(crate) struct PinnedProject {
    pub(crate) entry: ProjectEntry,
    pub(crate) target: TrackTarget,
    pub(crate) view: ViewId,
    pub(crate) generation: i64,
    pub(crate) commit: Option<String>,
    pub(crate) latest_seen: Option<String>,
    pub(crate) building: Option<i64>,
    /// Tier states as the indexer reports them (`None` for views it does
    /// not run).
    pub(crate) tiers: Option<TierStates>,
    pub(crate) activated_at: Option<OffsetDateTime>,
    pub(crate) overlay: Option<PinnedOverlay>,
    /// The embedding profile the view served when the context was pinned
    /// (`None`: none). Fixed for the context's lifetime, so a profile switch
    /// that activates later never changes which vectors an open context
    /// searches.
    pub(crate) serving_profile: Option<ProfileId>,
}

impl PinnedProject {
    /// The store pin of the base view.
    pub(crate) fn pin(&self) -> GenerationPin {
        GenerationPin {
            view: self.view,
            generation: self.generation,
        }
    }

    /// The commit as an MCP commit id (`None` for directory sources).
    pub(crate) fn commit_id(&self) -> Option<CommitId> {
        self.commit.as_deref().and_then(|c| CommitId::new(c).ok())
    }

    /// Whether the pinned generation is the newest commit seen.
    pub(crate) fn index_state(&self) -> IndexState {
        if self.latest_seen.is_none() || self.latest_seen == self.commit {
            IndexState::Current
        } else if self.building.is_some() {
            IndexState::CatchingUp
        } else {
            IndexState::Stale
        }
    }

    /// Highest ready tier: T1 at least (a generation activates only after
    /// text and symbols are stored), T2/T3 when the indexer reports them
    /// done for this generation.
    pub(crate) fn freshness(&self) -> FreshnessTier {
        let Some(tiers) = &self.tiers else {
            return FreshnessTier::T1Symbols;
        };
        let t2 = tiers.t2 == TierState::Done;
        let t3 = tiers.t3 == TierState::Done;
        match (t2, t3) {
            (true, true) => FreshnessTier::T3Relations,
            (true, false) => FreshnessTier::T2Embeddings,
            _ => FreshnessTier::T1Symbols,
        }
    }

    /// The manifest entry of this project.
    pub(crate) fn project_view(&self) -> ProjectView {
        ProjectView {
            project: self.entry.name.clone(),
            view: self.target.clone(),
            layer: if self.overlay.is_some() {
                ViewLayer::Personal
            } else {
                ViewLayer::Shared
            },
            commit: self.commit_id(),
            local_generation: self.overlay.as_ref().map_or(0, |o| o.generation),
            freshness: Some(self.freshness()),
            index_state: self.index_state(),
        }
    }
}

/// The view manifest of one request or context.
#[derive(Debug, Clone)]
pub(crate) struct Pinned {
    pub(crate) workspace: Arc<WorkspaceEntry>,
    pub(crate) projects: BTreeMap<Name, PinnedProject>,
    /// Visible projects without an active index.
    pub(crate) not_indexed: BTreeSet<Name>,
    /// Problems found while pinning (missing refs, …).
    pub(crate) gaps: Vec<Gap>,
    pub(crate) current_project: Option<Name>,
}

impl Pinned {
    /// The manifest as the MCP tools report it, in project order.
    pub(crate) fn manifest(&self) -> Vec<ProjectView> {
        let mut out: Vec<ProjectView> = self
            .projects
            .values()
            .map(PinnedProject::project_view)
            .collect();
        for project in &self.not_indexed {
            if let Some(entry) = self.workspace.project(project) {
                out.push(ProjectView {
                    project: project.clone(),
                    view: entry.target.clone(),
                    layer: ViewLayer::Shared,
                    commit: None,
                    local_generation: 0,
                    freshness: None,
                    index_state: IndexState::NotIndexed,
                });
            }
        }
        out.sort_by(|a, b| a.project.cmp(&b.project));
        out
    }

    /// Gaps for unindexed projects among `only` (all when empty).
    pub(crate) fn not_indexed_gaps(&self, only: &[Name]) -> Vec<Gap> {
        self.not_indexed
            .iter()
            .filter(|p| only.is_empty() || only.contains(p))
            .map(|p| {
                Gap::for_project(
                    GapReason::ProjectNotIndexed,
                    p.clone(),
                    format!("{p} has no index yet; its code was not searched"),
                )
            })
            .collect()
    }

    /// A copy narrowed to what `access` may see now.
    fn visible_to(&self, access: &Access) -> Pinned {
        let ws = &self.workspace.name;
        let mut out = self.clone();
        out.projects
            .retain(|name, _| access.reads_project(ws, name));
        out.not_indexed
            .retain(|name| access.reads_project(ws, name));
        for (name, project) in &mut out.projects {
            if let Some(overlay) = &project.overlay
                && !access.allows(
                    Action::ReadUncommittedOverlay(overlay.owner),
                    &Resource::project(ws.clone(), name.clone()),
                )
            {
                project.overlay = None;
            }
        }
        out.gaps.retain(|g| {
            g.project
                .as_ref()
                .is_none_or(|p| access.reads_project(ws, p))
        });
        if out
            .current_project
            .as_ref()
            .is_some_and(|p| !access.reads_project(ws, p))
        {
            out.current_project = None;
        }
        out
    }

    /// A copy restricted to an explicit project selection, without changing
    /// the generations, commits or profiles of an existing context.
    fn selected(&self, only: &[Name]) -> Pinned {
        let mut out = self.clone();
        if only.is_empty() {
            return out;
        }
        out.projects.retain(|name, _| only.contains(name));
        out.not_indexed.retain(|name| only.contains(name));
        out.gaps
            .retain(|gap| gap.project.as_ref().is_none_or(|name| only.contains(name)));
        if out
            .current_project
            .as_ref()
            .is_some_and(|name| !only.contains(name))
        {
            out.current_project = None;
        }
        out
    }
}

fn require_visible_projects(
    workspace: &WorkspaceEntry,
    access: &Access,
    projects: &[Name],
) -> Result<(), ToolError> {
    for project in projects {
        if workspace.project(project).is_none() || !access.reads_project(&workspace.name, project) {
            return Err(ToolError::not_found(format!(
                "project {project} does not exist in workspace {}",
                workspace.name,
            )));
        }
    }
    Ok(())
}

/// Live contexts: `context_id` → pinned manifest, bound to the identity that
/// opened it, expiring after an idle period.
#[derive(Debug)]
pub(crate) struct ContextStore {
    ttl: Duration,
    max: usize,
    entries: BTreeMap<String, ContextEntry>,
}

#[derive(Debug)]
struct ContextEntry {
    owner: String,
    pinned: Arc<Pinned>,
    last_used: Instant,
}

impl ContextStore {
    pub(crate) fn new(ttl: Duration, max: usize) -> Self {
        Self {
            ttl,
            max: max.max(1),
            entries: BTreeMap::new(),
        }
    }

    fn insert(&mut self, owner: &str, pinned: Pinned) -> Result<ContextId, ToolError> {
        let now = Instant::now();
        let ttl = self.ttl;
        self.entries
            .retain(|_, e| now.saturating_duration_since(e.last_used) <= ttl);
        while self.entries.len() >= self.max {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(key) => {
                    self.entries.remove(&key);
                }
                None => break,
            }
        }
        let id = format!("ctx-{}", uuid::Uuid::now_v7().simple());
        let context = ContextId::new(id.clone()).map_err(|e| ToolError::internal(e.to_string()))?;
        self.entries.insert(
            id,
            ContextEntry {
                owner: owner.to_owned(),
                pinned: Arc::new(pinned),
                last_used: now,
            },
        );
        Ok(context)
    }

    fn get(&mut self, owner: &str, id: &ContextId) -> Result<Arc<Pinned>, ToolError> {
        let now = Instant::now();
        let Some(entry) = self.entries.get_mut(id.as_str()) else {
            return Err(ToolError::not_found(
                "context_id is unknown; call open_workspace",
            ));
        };
        if entry.owner != owner {
            // Another identity's context is reported like an unknown one.
            return Err(ToolError::not_found(
                "context_id is unknown; call open_workspace",
            ));
        }
        if now.saturating_duration_since(entry.last_used) > self.ttl {
            self.entries.remove(id.as_str());
            return Err(ToolError::stale("the context expired"));
        }
        entry.last_used = now;
        Ok(Arc::clone(&entry.pinned))
    }
}

/// Lexically normalised absolute form of `path` (symlinks resolved when the
/// path exists).
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The git worktree root containing `dir` (walking up at most 64 levels).
fn worktree_root(dir: &Path) -> Option<GitRepo> {
    let mut current = Some(dir);
    let mut steps = 0;
    while let Some(path) = current {
        if let Ok(repo) = GitRepo::open(path)
            && repo.workdir().is_some()
        {
            return Some(repo);
        }
        steps += 1;
        if steps > 64 {
            break;
        }
        current = path.parent();
    }
    None
}

impl Engine {
    /// Resolves a tool's target to a pinned manifest the caller may see.
    pub(crate) async fn resolve_target(
        &self,
        access: &Access,
        target: &Target,
    ) -> Result<Arc<Pinned>, ToolError> {
        self.resolve_target_for_projects(access, target, &[]).await
    }

    /// Resolves only selected projects, so unrelated source failures cannot
    /// prevent a filtered search. Contexts keep their originally pinned data.
    pub(crate) async fn resolve_target_for_projects(
        &self,
        access: &Access,
        target: &Target,
        projects: &[Name],
    ) -> Result<Arc<Pinned>, ToolError> {
        if let Some(context) = &target.context_id {
            let pinned = {
                let mut contexts = self
                    .inner
                    .contexts
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                contexts.get(access.label(), context)?
            };
            require_visible_projects(&pinned.workspace, access, projects)?;
            return Ok(Arc::new(pinned.visible_to(access).selected(projects)));
        }
        let pinned = self
            .pin_projects(access, target.workspace.as_ref(), &target.views, projects)
            .await?;
        Ok(Arc::new(pinned))
    }

    /// Stores a pinned manifest as a new context owned by `access`.
    pub(crate) fn create_context(
        &self,
        access: &Access,
        pinned: Pinned,
    ) -> Result<ContextId, ToolError> {
        let mut contexts = self
            .inner
            .contexts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        contexts.insert(access.label(), pinned)
    }

    /// The workspace a request names, or the only one the caller can see.
    pub(crate) fn choose_workspace(
        &self,
        access: &Access,
        workspace: Option<&Name>,
    ) -> Result<Arc<WorkspaceEntry>, ToolError> {
        let visible: Vec<Arc<WorkspaceEntry>> = self
            .all_workspaces()
            .into_iter()
            .filter(|w| {
                w.projects
                    .iter()
                    .any(|p| access.reads_project(&w.name, &p.name))
            })
            .collect();
        match workspace {
            Some(name) => visible
                .into_iter()
                .find(|w| &w.name == name)
                .ok_or_else(|| ToolError::not_found(format!("workspace {name} does not exist"))),
            None => match visible.as_slice() {
                [only] => Ok(Arc::clone(only)),
                [] => Err(ToolError::not_found("no workspace is reachable")),
                several => Err(ToolError::invalid_input(format!(
                    "several workspaces are reachable; pass `workspace` ({})",
                    several
                        .iter()
                        .map(|w| w.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))),
            },
        }
    }

    /// Pins the active generation of every visible project of a workspace,
    /// at the tracked ref or at the ref `views` names. A Git target must
    /// still resolve to a commit before its stored view can be used. A
    /// missing ref or indexed view is reported (`ref_not_found`), never
    /// replaced. Existing contexts keep their original version manifest.
    pub(crate) async fn pin(
        &self,
        access: &Access,
        workspace: Option<&Name>,
        views: &[ViewPin],
    ) -> Result<Pinned, ToolError> {
        self.pin_projects(access, workspace, views, &[]).await
    }

    /// Pins only the selected visible projects; an empty selection means all.
    async fn pin_projects(
        &self,
        access: &Access,
        workspace: Option<&Name>,
        views: &[ViewPin],
        only: &[Name],
    ) -> Result<Pinned, ToolError> {
        let ws = self.choose_workspace(access, workspace)?;
        require_visible_projects(&ws, access, only)?;
        for pin in views {
            if ws.project(&pin.project).is_none() || !access.reads_project(&ws.name, &pin.project) {
                return Err(ToolError::not_found(format!(
                    "project {} does not exist in workspace {}",
                    pin.project, ws.name
                )));
            }
        }
        let mut pinned = Pinned {
            workspace: Arc::clone(&ws),
            projects: BTreeMap::new(),
            not_indexed: BTreeSet::new(),
            gaps: Vec::new(),
            current_project: None,
        };
        let mut conn = self.inner.store.acquire().await.map_err(store_tool)?;
        for entry in &ws.projects {
            if !access.reads_project(&ws.name, &entry.name)
                || (!only.is_empty() && !only.contains(&entry.name))
            {
                continue;
            }
            let requested = views.iter().find(|p| p.project == entry.name);
            let target = requested.map_or_else(|| entry.target.clone(), |p| p.view.clone());
            let mut observed_commit = None;
            if entry.source_kind == SourceKind::Git {
                let path = entry.path.clone();
                let requested = target.clone();
                let mode = self.inner.indexer.config().git_config;
                // Reading refs is blocking local I/O. It must follow the
                // authorization check and must not refresh or enqueue work.
                let resolved = tokio::task::spawn_blocking(move || {
                    let repo = match mode {
                        GitConfigMode::User => GitRepo::open(&path),
                        GitConfigMode::Isolated => GitRepo::open_isolated(&path),
                    }?;
                    repo.resolve(&requested).map(|resolved| resolved.commit)
                })
                .await
                .map_err(|error| ToolError::internal(format!("source target task: {error}")))?;
                match resolved {
                    Ok(commit) => observed_commit = Some(commit),
                    Err(
                        error @ (GitError::RefNotFound { .. }
                        | GitError::CommitNotFound { .. }
                        | GitError::UnbornHead { .. }
                        | GitError::NotACommit { .. }
                        | GitError::InvalidObjectId { .. }),
                    ) => {
                        pinned.gaps.push(Gap::for_project(
                            GapReason::RefNotFound,
                            entry.name.clone(),
                            format!(
                                "{} cannot resolve {target}: {error}; no indexed generation was used in its place",
                                entry.name,
                            ),
                        ));
                        continue;
                    }
                    Err(error) => {
                        return Err(ToolError::internal(format!(
                            "cannot resolve source target for project {}: {error}",
                            entry.name,
                        )));
                    }
                }
            }
            let view_row = if target == entry.target {
                views::get_view(&mut conn, entry.view)
                    .await
                    .map_err(store_tool)?
            } else {
                views::find_view(&mut conn, entry.id, &target)
                    .await
                    .map_err(store_tool)?
            };
            let Some(row) = view_row else {
                pinned.gaps.push(Gap::for_project(
                    GapReason::RefNotFound,
                    entry.name.clone(),
                    format!(
                        "{} has no index for {target}; only {} is indexed, and no other ref was used in its place",
                        entry.name, entry.target
                    ),
                ));
                continue;
            };
            let Some(generation) = row.active_generation else {
                pinned.not_indexed.insert(entry.name.clone());
                continue;
            };
            let status = if row.id == entry.view {
                self.inner.indexer.status(row.id).await.ok()
            } else {
                None
            };
            let activated_at = views::get_generation(&mut conn, row.id, generation)
                .await
                .map_err(store_tool)?
                .and_then(|g| g.activated_at);
            pinned.projects.insert(
                entry.name.clone(),
                PinnedProject {
                    entry: entry.clone(),
                    target,
                    view: row.id,
                    generation,
                    commit: row.active_commit.clone(),
                    // Fresh requests report what the ref points to now;
                    // the store's active generation remains the version pin.
                    latest_seen: observed_commit.or_else(|| row.latest_seen_commit.clone()),
                    building: status.as_ref().and_then(|s| s.building_generation),
                    tiers: status.map(|s| s.tiers),
                    activated_at,
                    overlay: None,
                    serving_profile: match &entry.embedding {
                        EmbeddingPlan::Embed { profile, .. } => Some(*profile),
                        _ => None,
                    },
                },
            );
        }
        // One statement for every pinned view, so a switch activating
        // meanwhile is seen for all of them or for none.
        let views: Vec<ViewId> = pinned.projects.values().map(|p| p.view).collect();
        let serving = knowell_store::switches::view_embeddings(&mut conn, &views)
            .await
            .map_err(store_tool)?;
        for project in pinned.projects.values_mut() {
            // Without a stored record the configured profile serves.
            if let Some(row) = serving.get(&project.view) {
                project.serving_profile = row.serving;
            }
        }
        Ok(pinned)
    }

    /// Detects the project and worktree of `working_directory` and pins the
    /// caller's personal overlay for it (when the worktree differs from the
    /// pinned view). Returns gaps for problems (never an error: the
    /// directory is a hint).
    pub(crate) async fn attach_worktree(
        &self,
        access: &Access,
        pinned: &mut Pinned,
        working_directory: &str,
    ) -> Vec<Gap> {
        let mut gaps = Vec::new();
        let dir = PathBuf::from(working_directory);
        if !dir.is_absolute() {
            gaps.push(Gap::new(
                GapReason::NotFound,
                "working_directory is not an absolute path; no personal layer was attached",
            ));
            return gaps;
        }
        let dir_c = canonical(&dir);
        let probe = dir.clone();
        let repo = tokio::task::spawn_blocking(move || worktree_root(&probe))
            .await
            .ok()
            .flatten();
        let repo_common = repo.as_ref().map(|r| canonical(r.common_dir()));
        let mut found: Option<(Name, Option<PathBuf>)> = None;
        for (name, project) in &pinned.projects {
            let project_dir = canonical(&project.entry.path);
            if dir_c.starts_with(&project_dir) {
                found = Some((
                    name.clone(),
                    repo.as_ref()
                        .and_then(|r| r.workdir().map(Path::to_path_buf)),
                ));
                break;
            }
            if project.entry.source_kind != SourceKind::Git {
                continue;
            }
            let Some(common) = &repo_common else { continue };
            let project_path = project.entry.path.clone();
            let project_repo =
                tokio::task::spawn_blocking(move || GitRepo::open(&project_path).ok())
                    .await
                    .ok()
                    .flatten();
            if project_repo.is_some_and(|r| canonical(r.common_dir()) == *common) {
                found = Some((
                    name.clone(),
                    repo.as_ref()
                        .and_then(|r| r.workdir().map(Path::to_path_buf)),
                ));
                break;
            }
        }
        let Some((project, worktree)) = found else {
            return gaps;
        };
        pinned.current_project = Some(project.clone());
        let (Some(worktree), Some(owner)) = (worktree, access.acting_user()) else {
            return gaps;
        };
        let Some(entry) = pinned.projects.get_mut(&project) else {
            return gaps;
        };
        if entry.entry.source_kind != SourceKind::Git || entry.view != entry.entry.view {
            return gaps;
        }
        match self
            .inner
            .indexer
            .build_overlay(entry.view, &worktree)
            .await
        {
            Ok(overlay) => {
                if overlay.base_generation() != Some(entry.generation) {
                    gaps.push(Gap::for_project(
                        GapReason::NotFound,
                        project.clone(),
                        "the view moved while the personal layer was built; call open_workspace again to include your worktree changes",
                    ));
                    return gaps;
                }
                if overlay.is_empty() {
                    return gaps;
                }
                let pinned_overlay = PinnedOverlay {
                    owner,
                    overlay,
                    generation: self.next_overlay_generation(),
                };
                entry.overlay = Some(pinned_overlay);
            }
            Err(error) => {
                tracing::warn!(error = %error, "building a personal overlay failed");
                gaps.push(Gap::for_project(
                    GapReason::NotFound,
                    project.clone(),
                    "the worktree could not be read; results come from the shared view only",
                ));
            }
        }
        gaps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts_expire_and_stay_with_their_owner() {
        let mut store = ContextStore::new(Duration::from_millis(30), 2);
        let pinned = Pinned {
            workspace: Arc::new(WorkspaceEntry {
                name: Name::new("shop").unwrap(),
                id: knowell_store::WorkspaceId(uuid::Uuid::nil()),
                projects: Vec::new(),
                issues: Vec::new(),
                resolved: knowell_config::ResolvedWorkspace {
                    name: Name::new("shop").unwrap(),
                    description: None,
                    projects: Vec::new(),
                },
            }),
            projects: BTreeMap::new(),
            not_indexed: BTreeSet::new(),
            gaps: Vec::new(),
            current_project: None,
        };
        let id = store.insert("alice", pinned.clone()).unwrap();
        assert!(store.get("alice", &id).is_ok());
        assert_eq!(store.get("bob", &id).unwrap_err().kind(), "not_found");
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(store.get("alice", &id).unwrap_err().kind(), "stale");
        // Capacity: the least recently used context is dropped.
        let a = store.insert("alice", pinned.clone()).unwrap();
        let b = store.insert("alice", pinned.clone()).unwrap();
        let c = store.insert("alice", pinned).unwrap();
        assert!(store.get("alice", &a).is_err());
        assert!(store.get("alice", &b).is_ok());
        assert!(store.get("alice", &c).is_ok());
    }
}
