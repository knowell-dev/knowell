//! Registration: making sure the organization, workspace, sources, projects,
//! views and embedding profiles of a resolved workspace exist in the store
//! (idempotently), and the per-view context every stage works from.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use knowell_config::{
    DataPolicy, ProviderConfig, ResolvedEmbedding, ResolvedProject, ResolvedWorkspace,
};
use knowell_core::{ContentHash, Name, RepoPath, TrackTarget};
use knowell_embed::{Embedder, ProviderKind as EmbedProviderKind};
use knowell_secrets::ExclusionPolicy;
use knowell_store::embeddings::{self, EmbeddingProfile, NewEmbeddingProfile};
use knowell_store::hierarchy::{self, Organization, Project, Source, Workspace};
use knowell_store::views::{self, View};
use knowell_store::{
    OrganizationId, PgConnection, ProfileId, ProjectId, SourceId, SourceKind, StoreError, ViewId,
    WorkspaceId,
};
use serde::Serialize;

use crate::config::IndexerConfig;
use crate::error::IndexError;
use crate::status::TierSkip;

/// What the embedding tier of a view will do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "plan", rename_all = "snake_case")]
pub enum EmbeddingPlan {
    /// Embed with this provider into this store profile.
    Embed {
        /// Provider name from the engine configuration.
        provider: Name,
        /// Registered store profile.
        profile: ProfileId,
        /// Its name.
        profile_name: Name,
        /// Vector size.
        dimensions: u32,
    },
    /// Deliberately not embedded.
    Skip {
        /// Why.
        reason: TierSkip,
    },
    /// Configured, but cannot run; every build reports T2 as failed.
    Unavailable {
        /// Why (no secret values).
        reason: String,
    },
}

/// One registered view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegisteredView {
    /// Project name.
    pub project: Name,
    /// Project id.
    pub project_id: ProjectId,
    /// View id.
    pub view: ViewId,
    /// What the view follows.
    pub target: TrackTarget,
    /// Git repository or plain directory.
    pub source_kind: SourceKind,
    /// The embedding plan.
    pub embedding: EmbeddingPlan,
}

/// A project that could not be registered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegistrationIssue {
    /// Project name.
    pub project: Name,
    /// Why (no secret values).
    pub reason: String,
}

/// Result of [`crate::Indexer::register`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Registration {
    /// The organization.
    pub organization: OrganizationId,
    /// The workspace.
    pub workspace: WorkspaceId,
    /// Registered views, in workspace file order.
    pub views: Vec<RegisteredView>,
    /// Projects that could not be registered.
    pub issues: Vec<RegistrationIssue>,
}

/// Everything a stage needs to know about a view.
#[derive(Clone)]
pub(crate) struct ViewContext {
    pub(crate) view: ViewId,
    pub(crate) organization: OrganizationId,
    pub(crate) workspace: WorkspaceId,
    pub(crate) workspace_name: Name,
    pub(crate) project: ProjectId,
    pub(crate) project_name: Name,
    pub(crate) source_kind: SourceKind,
    /// Repository (or directory) root on disk.
    pub(crate) source_path: PathBuf,
    /// Sub-root of the project inside the source.
    pub(crate) root: Option<RepoPath>,
    pub(crate) target: TrackTarget,
    pub(crate) policy: ExclusionPolicy,
    /// Identity of the content policy (patterns, size limit, root).
    pub(crate) policy_key: ContentHash,
    pub(crate) embedding: EmbeddingDecision,
}

/// The embedding decision with the full store profile.
#[derive(Clone)]
pub(crate) enum EmbeddingDecision {
    Embed {
        provider: Name,
        profile: EmbeddingProfile,
    },
    Skip(TierSkip),
    Unavailable(String),
}

impl EmbeddingDecision {
    pub(crate) fn plan(&self) -> EmbeddingPlan {
        match self {
            EmbeddingDecision::Embed { provider, profile } => EmbeddingPlan::Embed {
                provider: provider.clone(),
                profile: profile.id,
                profile_name: profile.name.clone(),
                dimensions: profile.dimensions,
            },
            EmbeddingDecision::Skip(reason) => EmbeddingPlan::Skip { reason: *reason },
            EmbeddingDecision::Unavailable(reason) => EmbeddingPlan::Unavailable {
                reason: reason.clone(),
            },
        }
    }
}

impl ViewContext {
    /// The project directory on disk (source root plus project root).
    pub(crate) fn project_dir(&self) -> PathBuf {
        match &self.root {
            Some(root) => {
                let mut out = self.source_path.clone();
                for component in root.components() {
                    out.push(component);
                }
                out
            }
            None => self.source_path.clone(),
        }
    }

    /// Project-relative path of a repository-relative path, if it lies in
    /// the project.
    pub(crate) fn to_project_path(&self, repo_path: &RepoPath) -> Option<RepoPath> {
        match &self.root {
            None => Some(repo_path.clone()),
            Some(root) => repo_path
                .as_str()
                .strip_prefix(root.as_str())
                .and_then(|rest| rest.strip_prefix('/'))
                .and_then(|rest| RepoPath::new(rest).ok()),
        }
    }

    /// Repository-relative path of a project-relative path.
    pub(crate) fn to_repo_path(&self, path: &RepoPath) -> Option<RepoPath> {
        match &self.root {
            None => Some(path.clone()),
            Some(root) => root.join(path.as_str()).ok(),
        }
    }
}

/// The exclusion policy of a project: built-in rules, the configured
/// vendor directories, then the project's own patterns.
pub(crate) fn exclusion_policy(
    project: &ResolvedProject,
    config: &IndexerConfig,
) -> Result<(ExclusionPolicy, ContentHash), IndexError> {
    let mut patterns: Vec<String> = config.content.excluded_dirs.clone();
    patterns.extend(project.exclude.iter().map(|p| p.value.clone()));
    let policy = ExclusionPolicy::with_patterns(&patterns)?;
    let mut parts: Vec<Vec<u8>> = vec![
        b"knowell.content-policy.v1".to_vec(),
        config.limits.max_file_bytes.to_le_bytes().to_vec(),
        project
            .root
            .as_ref()
            .map(|r| r.as_str().as_bytes().to_vec())
            .unwrap_or_default(),
    ];
    parts.extend(patterns.iter().map(|p| p.as_bytes().to_vec()));
    let key = ContentHash::of_parts(parts.iter().map(Vec::as_slice));
    Ok((policy, key))
}

/// Git repository unless the project is a plain directory followed as
/// `worktree` (a directory has no refs to follow).
pub(crate) fn source_kind(project: &ResolvedProject) -> SourceKind {
    let has_git = project.path.join(".git").exists();
    if !has_git && project.track.value == TrackTarget::WorktreeHead && project.path.is_dir() {
        SourceKind::Directory
    } else {
        SourceKind::Git
    }
}

/// The profile name and settings the store registers for an embedder.
pub(crate) fn store_profile(
    profile: &knowell_embed::EmbeddingProfile,
) -> Result<NewEmbeddingProfile, IndexError> {
    let input_format_version = format!(
        "embed{}-prepared{}-parser{}",
        profile.input_format_version,
        knowell_parse::PREPARED_FORMAT_VERSION,
        knowell_parse::PARSER_VERSION
    );
    let key = ContentHash::of_parts([
        profile.profile_key().as_bytes().as_slice(),
        input_format_version.as_bytes(),
    ]);
    let name = Name::new(format!(
        "{}-{}-{}",
        profile.provider_kind.as_str(),
        profile.dimensions,
        key.short()
    ))
    .map_err(|e| IndexError::Config(format!("embedding profile name: {e}")))?;
    Ok(NewEmbeddingProfile {
        name,
        provider: profile.provider_kind.as_str().to_owned(),
        model: profile.model.clone(),
        dimensions: profile.dimensions,
        input_format_version,
    })
}

/// Decides what T2 does for a project; registers the store profile when it
/// embeds. `conn` must not be inside a transaction (index creation).
pub(crate) async fn decide_embedding<E: Embedder>(
    conn: &mut PgConnection,
    organization: OrganizationId,
    embedding: &ResolvedEmbedding,
    data_policy: DataPolicy,
    providers: &BTreeMap<Name, ProviderConfig>,
    embedders: &BTreeMap<Name, Arc<E>>,
) -> Result<EmbeddingDecision, IndexError> {
    let Some(provider) = &embedding.provider else {
        return Ok(EmbeddingDecision::Skip(TierSkip::NoProvider));
    };
    let name = &provider.value;
    let Some(config) = providers.get(name) else {
        return Ok(EmbeddingDecision::Unavailable(format!(
            "embedding provider `{name}` (from {}) is not defined under `[providers]` in the engine config",
            provider.origin
        )));
    };
    let embedder = embedders.get(name);
    let cloud = config.kind.is_cloud()
        || embedder.is_some_and(|e| e.profile().provider_kind == EmbedProviderKind::Gemini);
    if data_policy == DataPolicy::LocalOnly && cloud {
        // Nothing is ever sent: the decision is made before any input exists.
        return Ok(EmbeddingDecision::Skip(TierSkip::DataPolicyLocalOnly));
    }
    let Some(embedder) = embedder else {
        return Ok(EmbeddingDecision::Unavailable(format!(
            "embedding provider `{name}` is configured but no embedder for it was given to the indexer"
        )));
    };
    let profile = embedder.profile();
    let expected_model = embedding
        .model
        .as_ref()
        .map(|m| m.value.as_str())
        .or(config.model.as_deref());
    match expected_model {
        None => {
            return Ok(EmbeddingDecision::Unavailable(format!(
                "no embedding model is configured for provider `{name}`; set `model` in the workspace, project or provider"
            )));
        }
        Some(model) if model != profile.model => {
            return Ok(EmbeddingDecision::Unavailable(format!(
                "provider `{name}` embeds with model `{}`, but the configuration asks for `{model}`; no other model is used in its place",
                profile.model
            )));
        }
        Some(_) => {}
    }
    if profile.dimensions != embedding.dimensions.value {
        return Ok(EmbeddingDecision::Unavailable(format!(
            "provider `{name}` produces {} dimensions, but the configuration asks for {}",
            profile.dimensions, embedding.dimensions.value
        )));
    }
    let spec = store_profile(profile)?;
    let stored = match embeddings::register_profile(conn, organization, &spec).await {
        Ok(profile) => profile,
        Err(error @ StoreError::SemanticUnavailable) => {
            return Ok(EmbeddingDecision::Unavailable(error.to_string()));
        }
        Err(error) => return Err(error.into()),
    };
    Ok(EmbeddingDecision::Embed {
        provider: name.clone(),
        profile: stored,
    })
}

pub(crate) async fn ensure_organization(
    conn: &mut PgConnection,
    name: &Name,
) -> Result<Organization, IndexError> {
    if let Some(found) = hierarchy::find_organization(conn, name).await? {
        return Ok(found);
    }
    match hierarchy::create_organization(conn, name).await {
        Ok(created) => Ok(created),
        // Another process created it in between.
        Err(StoreError::AlreadyExists { .. }) => hierarchy::find_organization(conn, name)
            .await?
            .ok_or_else(|| IndexError::Inconsistent(format!("organization `{name}` vanished"))),
        Err(e) => Err(e.into()),
    }
}

pub(crate) async fn ensure_workspace(
    conn: &mut PgConnection,
    organization: OrganizationId,
    name: &Name,
) -> Result<Workspace, IndexError> {
    if let Some(found) = hierarchy::find_workspace(conn, organization, name).await? {
        return Ok(found);
    }
    match hierarchy::create_workspace(conn, organization, name).await {
        Ok(created) => Ok(created),
        Err(StoreError::AlreadyExists { .. }) => {
            hierarchy::find_workspace(conn, organization, name)
                .await?
                .ok_or_else(|| IndexError::Inconsistent(format!("workspace `{name}` vanished")))
        }
        Err(e) => Err(e.into()),
    }
}

pub(crate) async fn ensure_source(
    conn: &mut PgConnection,
    organization: OrganizationId,
    kind: SourceKind,
    location: &str,
) -> Result<Source, IndexError> {
    if let Some(found) = hierarchy::find_source(conn, organization, location).await? {
        return Ok(found);
    }
    match hierarchy::create_source(conn, organization, kind, location).await {
        Ok(created) => Ok(created),
        Err(StoreError::AlreadyExists { .. }) => {
            hierarchy::find_source(conn, organization, location)
                .await?
                .ok_or_else(|| {
                    IndexError::Inconsistent("source vanished while registering".to_owned())
                })
        }
        Err(e) => Err(e.into()),
    }
}

/// Finds or creates the project. A project registered earlier with another
/// source or root is reported, never silently re-pointed.
pub(crate) async fn ensure_project(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    source: SourceId,
    name: &Name,
    root: Option<&RepoPath>,
) -> Result<Project, IndexError> {
    let existing = match hierarchy::find_project(conn, workspace, name).await? {
        Some(p) => Some(p),
        None => match hierarchy::create_project(conn, workspace, source, name, root).await {
            Ok(created) => return Ok(created),
            Err(StoreError::AlreadyExists { .. }) => {
                hierarchy::find_project(conn, workspace, name).await?
            }
            Err(e) => return Err(e.into()),
        },
    };
    let project =
        existing.ok_or_else(|| IndexError::Inconsistent(format!("project `{name}` vanished")))?;
    if project.source != source || project.root.as_ref() != root {
        return Err(IndexError::Config(format!(
            "project `{name}` is already registered with another location or root; rename it or remove the old registration first"
        )));
    }
    Ok(project)
}

pub(crate) async fn ensure_view(
    conn: &mut PgConnection,
    project: ProjectId,
    target: &TrackTarget,
) -> Result<View, IndexError> {
    if let Some(found) = views::find_view(conn, project, target).await? {
        return Ok(found);
    }
    match views::create_view(conn, project, target).await {
        Ok(created) => Ok(created),
        Err(StoreError::AlreadyExists { .. }) => views::find_view(conn, project, target)
            .await?
            .ok_or_else(|| IndexError::Inconsistent(format!("view `{target}` vanished"))),
        Err(e) => Err(e.into()),
    }
}

/// The text stored as a source location: the absolute project path.
pub(crate) fn location(project: &ResolvedProject) -> String {
    project.path.to_string_lossy().into_owned()
}

/// Registers every project of `workspace`. Projects that fail are listed in
/// [`Registration::issues`]; the others are registered.
pub(crate) async fn register_workspace<E: Embedder>(
    conn: &mut PgConnection,
    config: &IndexerConfig,
    providers: &BTreeMap<Name, ProviderConfig>,
    embedders: &BTreeMap<Name, Arc<E>>,
    workspace: &ResolvedWorkspace,
) -> Result<(Registration, Vec<ViewContext>), IndexError> {
    let org = ensure_organization(conn, &config.organization).await?;
    let ws = ensure_workspace(conn, org.id, &workspace.name).await?;
    let mut registered = Vec::new();
    let mut contexts = Vec::new();
    let mut issues = Vec::new();
    for project in &workspace.projects {
        match register_project(conn, config, providers, embedders, org.id, &ws, project).await {
            Ok((view, context)) => {
                registered.push(view);
                contexts.push(context);
            }
            Err(error) => issues.push(RegistrationIssue {
                project: project.name.clone(),
                reason: error.to_string(),
            }),
        }
    }
    Ok((
        Registration {
            organization: org.id,
            workspace: ws.id,
            views: registered,
            issues,
        },
        contexts,
    ))
}

async fn register_project<E: Embedder>(
    conn: &mut PgConnection,
    config: &IndexerConfig,
    providers: &BTreeMap<Name, ProviderConfig>,
    embedders: &BTreeMap<Name, Arc<E>>,
    organization: OrganizationId,
    workspace: &Workspace,
    project: &ResolvedProject,
) -> Result<(RegisteredView, ViewContext), IndexError> {
    let (policy, policy_key) = exclusion_policy(project, config)?;
    let kind = source_kind(project);
    let source = ensure_source(conn, organization, kind, &location(project)).await?;
    let stored = ensure_project(
        conn,
        workspace.id,
        source.id,
        &project.name,
        project.root.as_ref(),
    )
    .await?;
    let view = ensure_view(conn, stored.id, &project.track.value).await?;
    let embedding = decide_embedding(
        conn,
        organization,
        &project.embedding,
        project.data_policy.value,
        providers,
        embedders,
    )
    .await?;
    let context = ViewContext {
        view: view.id,
        organization,
        workspace: workspace.id,
        workspace_name: workspace.name.clone(),
        project: stored.id,
        project_name: project.name.clone(),
        source_kind: source.kind,
        source_path: project.path.clone(),
        root: project.root.clone(),
        target: project.track.value.clone(),
        policy,
        policy_key,
        embedding,
    };
    let registered = RegisteredView {
        project: project.name.clone(),
        project_id: stored.id,
        view: view.id,
        target: view.target.clone(),
        source_kind: source.kind,
        embedding: context.embedding.plan(),
    };
    Ok((registered, context))
}

#[cfg(test)]
mod tests {
    use super::*;
    use knowell_config::{EmbeddingPreset, Origin, Sourced};

    fn project(root: Option<&str>, exclude: &[&str]) -> ResolvedProject {
        ResolvedProject {
            name: Name::new("p").unwrap(),
            path: std::env::temp_dir().join("knowell-index-context-test"),
            root: root.map(|r| RepoPath::new(r).unwrap()),
            remote: None,
            track: Sourced {
                value: "branch:main".parse().unwrap(),
                origin: Origin::Workspace,
            },
            data_policy: Sourced {
                value: DataPolicy::LocalOnly,
                origin: Origin::Builtin,
            },
            embedding: ResolvedEmbedding {
                provider: None,
                model: None,
                preset: Sourced {
                    value: EmbeddingPreset::Balanced,
                    origin: Origin::Builtin,
                },
                dimensions: Sourced {
                    value: 1536,
                    origin: Origin::Builtin,
                },
            },
            exclude: exclude
                .iter()
                .map(|e| Sourced {
                    value: (*e).to_owned(),
                    origin: Origin::Project,
                })
                .collect(),
        }
    }

    #[test]
    fn policy_excludes_vendor_and_project_patterns() {
        let config = IndexerConfig::new("/tmp/x", Name::new("acme").unwrap());
        let (policy, key) = exclusion_policy(&project(None, &["fixtures/**"]), &config).unwrap();
        let check = |p: &str| policy.check(&RepoPath::new(p).unwrap()).is_some();
        assert!(check("vendor/lib/a.go"));
        assert!(check("web/node_modules/x/index.js"));
        assert!(check("fixtures/data.json"));
        assert!(check(".env"));
        assert!(!check("src/main.rs"));
        let (_, other) = exclusion_policy(&project(None, &[]), &config).unwrap();
        assert_ne!(key, other);
        let (_, rooted) =
            exclusion_policy(&project(Some("pkg"), &["fixtures/**"]), &config).unwrap();
        assert_ne!(key, rooted);
        assert!(exclusion_policy(&project(None, &["a["]), &config).is_err());
    }

    #[test]
    fn project_paths_map_through_the_root() {
        let config = IndexerConfig::new("/tmp/x", Name::new("acme").unwrap());
        let p = project(Some("packages/contracts"), &[]);
        let (policy, policy_key) = exclusion_policy(&p, &config).unwrap();
        let ctx = ViewContext {
            view: ViewId(uuid::Uuid::nil()),
            organization: OrganizationId(uuid::Uuid::nil()),
            workspace: WorkspaceId(uuid::Uuid::nil()),
            workspace_name: Name::new("w").unwrap(),
            project: ProjectId(uuid::Uuid::nil()),
            project_name: p.name.clone(),
            source_kind: SourceKind::Git,
            source_path: p.path.clone(),
            root: p.root.clone(),
            target: p.track.value.clone(),
            policy,
            policy_key,
            embedding: EmbeddingDecision::Skip(TierSkip::NoProvider),
        };
        let inside = RepoPath::new("packages/contracts/api/openapi.yaml").unwrap();
        let outside = RepoPath::new("packages/contractsx/a").unwrap();
        assert_eq!(
            ctx.to_project_path(&inside).unwrap().as_str(),
            "api/openapi.yaml"
        );
        assert!(ctx.to_project_path(&outside).is_none());
        assert!(
            ctx.to_project_path(&RepoPath::new("packages/contracts").unwrap())
                .is_none()
        );
        assert_eq!(
            ctx.to_repo_path(&RepoPath::new("api/openapi.yaml").unwrap())
                .unwrap(),
            inside
        );
        assert!(ctx.project_dir().ends_with("contracts"));
    }

    #[test]
    fn store_profile_names_are_valid_and_versioned() {
        let profile = knowell_embed::FakeEmbedder::new(64)
            .unwrap()
            .profile()
            .clone();
        let spec = store_profile(&profile).unwrap();
        assert!(spec.name.as_str().starts_with("fake-64-"));
        assert!(spec.input_format_version.contains("prepared"));
        let mut other = profile.clone();
        other.dimensions = 32;
        assert_ne!(store_profile(&other).unwrap().name, spec.name);
    }
}
