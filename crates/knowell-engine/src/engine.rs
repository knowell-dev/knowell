//! The [`Engine`] object: construction, the workspace registry and the
//! shared state every tool and REST request works from.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};

use knowell_config::{DataPolicy, EngineConfig, ProviderConfig, ResolvedWorkspace};
use knowell_core::{Name, RepoPath, TrackTarget};
use knowell_embed::AnyEmbedder;
use knowell_index::{
    EmbeddingPlan, Indexer, IndexerConfig, ProgressKind, RegistrationIssue, RelationStage,
};
use knowell_query::Glossary;
use knowell_store::hierarchy;
use knowell_store::{
    OrganizationId, ProfileId, ProjectId, SourceKind, Store, StoreError, ViewId, WorkspaceId,
};
use tokio::sync::broadcast::error::RecvError;

use crate::access::{AccessResolver, StaticAccess};
use crate::error::EngineError;
use crate::graph::GraphCache;
use crate::memory::Directory;
use crate::memory::{MemoryRepo, StoreMemory};
use crate::scope::ContextStore;
use crate::settings::EngineSettings;
use crate::snapshot::{SnapshotCache, TextCache};
use crate::usage::UsageRecorder;

/// One registered project of a workspace.
#[derive(Debug, Clone)]
pub(crate) struct ProjectEntry {
    pub(crate) name: Name,
    pub(crate) id: ProjectId,
    /// The view of the tracked ref.
    pub(crate) view: ViewId,
    pub(crate) target: TrackTarget,
    pub(crate) path: PathBuf,
    pub(crate) root: Option<RepoPath>,
    pub(crate) data_policy: DataPolicy,
    pub(crate) embedding: EmbeddingPlan,
    pub(crate) source_kind: SourceKind,
}

/// One registered workspace.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceEntry {
    pub(crate) name: Name,
    pub(crate) id: WorkspaceId,
    /// Projects in workspace-file order.
    pub(crate) projects: Vec<ProjectEntry>,
    /// Projects that could not be registered.
    pub(crate) issues: Vec<RegistrationIssue>,
    /// The configuration it was registered from (profile switches
    /// re-register it with new embedding settings).
    pub(crate) resolved: ResolvedWorkspace,
}

impl WorkspaceEntry {
    pub(crate) fn project(&self, name: &Name) -> Option<&ProjectEntry> {
        self.projects.iter().find(|p| &p.name == name)
    }
}

/// The engine facade. Cheap to clone; every clone shares the same state.
///
/// One `Engine` serves both the MCP tools ([`knowell_mcp::KnowellTools`])
/// and the REST engine operations ([`knowell_server::Engine`]). Build it
/// with [`Engine::builder`].
#[derive(Clone)]
pub struct Engine {
    pub(crate) inner: Arc<Inner>,
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("organization", &self.inner.organization_name)
            .field("workspaces", &self.workspace_names())
            .finish_non_exhaustive()
    }
}

/// Shared state behind an [`Engine`].
pub(crate) struct Inner {
    pub(crate) store: Store,
    pub(crate) indexer: Indexer<AnyEmbedder>,
    pub(crate) embedders: BTreeMap<Name, Arc<AnyEmbedder>>,
    pub(crate) providers: BTreeMap<Name, ProviderConfig>,
    pub(crate) organization: OrganizationId,
    pub(crate) organization_name: Name,
    pub(crate) workspaces: RwLock<BTreeMap<Name, Arc<WorkspaceEntry>>>,
    pub(crate) directory: Arc<RwLock<Directory>>,
    pub(crate) settings: EngineSettings,
    pub(crate) glossary: Glossary,
    pub(crate) access: Arc<dyn AccessResolver>,
    pub(crate) memory: Arc<dyn MemoryRepo>,
    pub(crate) contexts: Mutex<ContextStore>,
    pub(crate) snapshots: SnapshotCache,
    pub(crate) graphs: GraphCache,
    pub(crate) texts: TextCache,
    pub(crate) usage: UsageRecorder,
    pub(crate) overlay_generation: AtomicU64,
    /// Store profile → engine provider name, for every profile a
    /// registration ever named (a switched-away profile keeps serving the
    /// generations its vectors cover).
    pub(crate) profile_providers: RwLock<BTreeMap<ProfileId, Name>>,
    /// Profile switches started by this process.
    pub(crate) switches: Mutex<Vec<crate::rest::SwitchRecord>>,
}

/// Builds an [`Engine`]: the indexer, the embedders, the workspaces and the
/// settings it serves.
pub struct EngineBuilder {
    store: Store,
    indexer_config: IndexerConfig,
    engine_config: Option<EngineConfig>,
    embedders: BTreeMap<Name, Arc<AnyEmbedder>>,
    workspaces: Vec<ResolvedWorkspace>,
    settings: EngineSettings,
    access: Option<Arc<dyn AccessResolver>>,
    memory: Option<Arc<dyn MemoryRepo>>,
    relation_stage: Option<Arc<dyn RelationStage>>,
}

impl fmt::Debug for EngineBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineBuilder")
            .field("embedders", &self.embedders.keys().collect::<Vec<_>>())
            .field(
                "workspaces",
                &self.workspaces.iter().map(|w| &w.name).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl EngineBuilder {
    /// Takes the embedding providers (kinds, default models) from the engine
    /// configuration; the same configuration the indexer uses.
    #[must_use]
    pub fn engine_config(mut self, config: &EngineConfig) -> Self {
        self.engine_config = Some(config.clone());
        self
    }

    /// Supplies the embedder of provider `provider`. It is shared by the
    /// indexer (documents) and the engine (queries, with the provider's own
    /// query prefix).
    #[must_use]
    pub fn embedder(mut self, provider: Name, embedder: Arc<AnyEmbedder>) -> Self {
        self.embedders.insert(provider, embedder);
        self
    }

    /// Registers a workspace when the engine is built (idempotent in the
    /// store).
    #[must_use]
    pub fn workspace(mut self, workspace: ResolvedWorkspace) -> Self {
        self.workspaces.push(workspace);
        self
    }

    /// Replaces the default settings.
    #[must_use]
    pub fn settings(mut self, settings: EngineSettings) -> Self {
        self.settings = settings;
        self
    }

    /// How MCP callers map to identities. Default: deny everyone.
    #[must_use]
    pub fn access(mut self, access: Arc<dyn AccessResolver>) -> Self {
        self.access = Some(access);
        self
    }

    /// Where memory and tasks are kept. Default: the store.
    #[must_use]
    pub fn memory(mut self, memory: Arc<dyn MemoryRepo>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// The T3 relation stage handed to the indexer (contract extraction).
    /// Set [`EngineSettings::relation_stage`] too, so contract tools stop
    /// reporting `contracts_not_extracted`.
    #[must_use]
    pub fn relation_stage(mut self, stage: Arc<dyn RelationStage>) -> Self {
        self.relation_stage = Some(stage);
        self
    }

    /// Builds the indexer, registers the workspaces and starts the listener
    /// that retires cached snapshots when views activate new generations.
    /// Must run inside a tokio runtime.
    ///
    /// # Errors
    /// Store and index errors; [`EngineError::Config`] for an unusable
    /// glossary.
    pub async fn build(self) -> Result<Engine, EngineError> {
        let glossary = Glossary::new(self.settings.glossary.clone())
            .map_err(|e| EngineError::Config(format!("glossary: {e}")))?;
        let organization_name = self.indexer_config.organization.clone();
        let mut builder =
            knowell_index::Indexer::<AnyEmbedder>::builder(self.store.clone(), self.indexer_config);
        let providers = self
            .engine_config
            .as_ref()
            .map(|c| c.providers.clone())
            .unwrap_or_default();
        if let Some(config) = &self.engine_config {
            builder = builder.engine(config);
        }
        for (name, embedder) in &self.embedders {
            builder = builder.embedder(name.clone(), Arc::clone(embedder));
        }
        if let Some(stage) = self.relation_stage {
            builder = builder.relation_stage(stage);
        }
        let indexer = builder.build()?;
        let organization = ensure_organization(&self.store, &organization_name).await?;
        let directory = Arc::new(RwLock::new(Directory::default()));
        let memory: Arc<dyn MemoryRepo> = match self.memory {
            Some(memory) => memory,
            None => Arc::new(StoreMemory::new(
                self.store.clone(),
                organization,
                Arc::clone(&directory),
            )),
        };
        let settings = self.settings;
        let inner = Arc::new(Inner {
            store: self.store,
            indexer,
            embedders: self.embedders,
            providers,
            organization,
            organization_name,
            workspaces: RwLock::new(BTreeMap::new()),
            directory,
            glossary,
            access: self
                .access
                .unwrap_or_else(|| Arc::new(StaticAccess::deny_all())),
            memory,
            contexts: Mutex::new(ContextStore::new(
                settings.context_ttl,
                settings.max_contexts,
            )),
            snapshots: SnapshotCache::new(settings.snapshot_cache),
            graphs: GraphCache::default(),
            texts: TextCache::new(settings.text_cache_bytes),
            usage: UsageRecorder::default(),
            overlay_generation: AtomicU64::new(0),
            profile_providers: RwLock::new(BTreeMap::new()),
            switches: Mutex::new(Vec::new()),
            settings,
        });
        let engine = Engine { inner };
        for workspace in &self.workspaces {
            engine.add_workspace(workspace).await?;
        }
        spawn_activation_listener(&engine);
        Ok(engine)
    }
}

async fn ensure_organization(store: &Store, name: &Name) -> Result<OrganizationId, EngineError> {
    let mut conn = store.acquire().await?;
    if let Some(found) = hierarchy::find_organization(&mut conn, name).await? {
        return Ok(found.id);
    }
    match hierarchy::create_organization(&mut conn, name).await {
        Ok(created) => Ok(created.id),
        Err(StoreError::AlreadyExists { .. }) => hierarchy::find_organization(&mut conn, name)
            .await?
            .map(|o| o.id)
            .ok_or_else(|| EngineError::internal("organization vanished while registering")),
        Err(other) => Err(other.into()),
    }
}

/// Retires cached snapshots and code graphs of views that activated a newer
/// generation; a lagging subscriber clears everything (the next query
/// reloads what it needs).
fn spawn_activation_listener(engine: &Engine) {
    let weak: Weak<Inner> = Arc::downgrade(&engine.inner);
    let mut events = engine.inner.indexer.subscribe();
    tokio::spawn(async move {
        loop {
            let event = events.recv().await;
            let Some(inner) = weak.upgrade() else {
                break;
            };
            match event {
                Ok(event) => {
                    if let (ProgressKind::Activated { .. }, Some(generation)) =
                        (&event.kind, event.generation)
                    {
                        inner.snapshots.retire_older(event.view, generation);
                        inner.graphs.retire_older(event.view, generation);
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    inner.snapshots.clear();
                    inner.graphs.clear();
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
}

impl Engine {
    /// Starts building an engine on `store`. The indexer is built from
    /// `indexer_config` (data directory, organization, limits).
    pub fn builder(store: Store, indexer_config: IndexerConfig) -> EngineBuilder {
        EngineBuilder {
            store,
            indexer_config,
            engine_config: None,
            embedders: BTreeMap::new(),
            workspaces: Vec::new(),
            settings: EngineSettings::default(),
            access: None,
            memory: None,
            relation_stage: None,
        }
    }

    /// The indexer (run workers, refresh views, build overlays).
    pub fn indexer(&self) -> &Indexer<AnyEmbedder> {
        &self.inner.indexer
    }

    /// The store.
    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    /// The settings the engine was built with.
    pub fn settings(&self) -> &EngineSettings {
        &self.inner.settings
    }

    /// Names of the registered workspaces, sorted.
    pub fn workspace_names(&self) -> Vec<Name> {
        self.inner
            .workspaces
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    /// Registers (or re-registers) a workspace with the indexer and the
    /// engine. Idempotent.
    ///
    /// # Errors
    /// Store and index errors.
    pub async fn add_workspace(
        &self,
        workspace: &ResolvedWorkspace,
    ) -> Result<knowell_index::Registration, EngineError> {
        let registration = self.inner.indexer.register(workspace).await?;
        let mut projects = Vec::new();
        for view in &registration.views {
            let Some(resolved) = workspace.projects.iter().find(|p| p.name == view.project) else {
                continue;
            };
            projects.push(ProjectEntry {
                name: view.project.clone(),
                id: view.project_id,
                view: view.view,
                target: view.target.clone(),
                path: resolved.path.clone(),
                root: resolved.root.clone(),
                data_policy: resolved.data_policy.value,
                embedding: view.embedding.clone(),
                source_kind: view.source_kind,
            });
        }
        {
            let mut providers = self
                .inner
                .profile_providers
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            for project in &projects {
                if let EmbeddingPlan::Embed {
                    provider, profile, ..
                } = &project.embedding
                {
                    providers.insert(*profile, provider.clone());
                }
            }
        }
        let entry = WorkspaceEntry {
            name: workspace.name.clone(),
            id: registration.workspace,
            projects,
            issues: registration.issues.clone(),
            resolved: workspace.clone(),
        };
        self.inner
            .directory
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .add_workspace(
                &entry.name,
                entry.id,
                entry.projects.iter().map(|p| (p.name.clone(), p.id)),
            );
        self.inner
            .workspaces
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(entry.name.clone(), Arc::new(entry));
        Ok(registration)
    }

    /// The registered workspace `name`.
    pub(crate) fn workspace(&self, name: &Name) -> Option<Arc<WorkspaceEntry>> {
        self.inner
            .workspaces
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()
    }

    /// Every registered workspace, by name.
    pub(crate) fn all_workspaces(&self) -> Vec<Arc<WorkspaceEntry>> {
        self.inner
            .workspaces
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// The next personal-overlay generation number (process-wide, from 1).
    pub(crate) fn next_overlay_generation(&self) -> u64 {
        self.inner
            .overlay_generation
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1)
    }

    /// Whether provider `name` is a cloud service (by its configured kind).
    pub(crate) fn provider_is_cloud(&self, name: &Name) -> bool {
        self.inner
            .providers
            .get(name)
            .is_some_and(|p| p.kind.is_cloud())
            || self.inner.embedders.get(name).is_some_and(|e| {
                knowell_embed::Embedder::profile(e.as_ref()).provider_kind
                    == knowell_embed::ProviderKind::Gemini
            })
    }
}
