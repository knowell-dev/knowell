//! The engine behind the MCP tools and the delegated REST routes.
//!
//! [`build_engine`] assembles one `knowell_engine::Engine` from the engine
//! configuration, the registered workspaces and the store; [`Tools`] serves
//! it over MCP and [`rest_engine`] over the REST API. Without a database the
//! tools answer `not_ready` ([`EngineUnavailable`]) and delegated REST routes
//! answer `503 engine_unavailable` — never invented data.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use knowell_auth::UserId;
use knowell_config::{EngineConfig, ProviderKind, ResolvedWorkspace};
use knowell_core::Name;
use knowell_embed::{
    AnyEmbedder, GeminiConfig, GeminiEmbedder, OllamaConfig, OllamaEmbedder,
    OpenAiCompatibleConfig, OpenAiCompatibleEmbedder,
};
use knowell_engine::{
    AccessResolver, Engine, EngineSettings, ParseProductCacheSettings, StoreAccess,
};
use knowell_index::{IndexerConfig, Priority, Worker, WorkerConfig};
use knowell_mcp::tools::{
    AnalyzeImpactInput, AnalyzeImpactOutput, BuildContextInput, BuildContextOutput, ContractsInput,
    ContractsOutput, FetchInput, FetchOutput, HistoryInput, HistoryOutput, IndexStatusInput,
    IndexStatusOutput, InspectSymbolInput, InspectSymbolOutput, OpenWorkspaceInput,
    OpenWorkspaceOutput, ReadMemoryInput, ReadMemoryOutput, ResumeTaskInput, ResumeTaskOutput,
    SaveCheckpointInput, SaveCheckpointOutput, SearchInput, SearchOutput, TraceFlowInput,
    TraceFlowOutput, WriteMemoryInput, WriteMemoryOutput,
};
use knowell_mcp::{Caller, KnowellTools, ToolError};
use knowell_store::Store;
use tokio_util::sync::CancellationToken;
use url::Url;

/// Whether a real engine answers the tools. `know context --session-start`
/// tells agents when it does not, so they do not waste calls.
pub(crate) const ENGINE_WIRED: bool = true;

/// Message of every tool answer when no engine could be built (no database).
pub(crate) const NOT_WIRED: &str =
    "the Knowell engine has no database; run `know init`, then `know doctor`";

/// Keeps cold-parse reuse and lexical span counts explicit across local and MCP engines.
pub(crate) fn engine_settings(home: &Path, parse_cache: bool, lexical_spans: u8) -> EngineSettings {
    EngineSettings {
        lexical_spans_per_file: lexical_spans,
        parse_product_cache: parse_cache
            .then(|| ParseProductCacheSettings::new(home.join("cache"))),
        ..EngineSettings::default()
    }
}

/// Everything the engine needs from the binary.
#[derive(Debug, Clone)]
pub(crate) struct EngineDeps {
    /// `$KNOWELL_HOME`.
    pub(crate) home: PathBuf,
    /// Opt-in to persisted exact-source parse products.
    pub(crate) parse_cache: bool,
    /// Most lexical spans per pinned file, from 1 through 3.
    pub(crate) lexical_spans: u8,
    /// The engine configuration in effect (after command-line overrides).
    pub(crate) engine: EngineConfig,
    /// An open, migrated store, or `None` (no database configured/reachable).
    pub(crate) store: Option<Store>,
    /// Registered workspace files by workspace name.
    pub(crate) workspace_files: BTreeMap<Name, PathBuf>,
    /// Organization (tenant) the workspaces belong to.
    pub(crate) organization: Name,
    /// The machine owner on non-hub roles; MCP callers act for this user.
    /// `None` on a hub, where identities come from tokens.
    pub(crate) local_user: Option<UserId>,
}

/// Builds the engine and registers every workspace that resolves. `Ok(None)`
/// when there is no store. Workspaces with configuration problems are
/// reported and skipped; providers whose secrets cannot be resolved are
/// reported and left out (their projects report semantic search as off).
pub(crate) async fn build_engine(
    deps: &EngineDeps,
) -> anyhow::Result<Option<(Engine, Vec<ResolvedWorkspace>)>> {
    let Some(store) = deps.store.clone() else {
        return Ok(None);
    };
    let workspaces = resolve_workspaces(&deps.workspace_files);
    let mut indexer_config = IndexerConfig::new(deps.home.join("data"), deps.organization.clone());
    indexer_config.concurrency = indexer_config.concurrency.max(1);
    let access = StoreAccess::new(store.clone(), deps.organization.clone());
    let access: Arc<dyn AccessResolver> = Arc::new(match deps.local_user {
        Some(user) => access.with_local_user(user),
        None => access,
    });
    let mut builder = Engine::builder(store, indexer_config)
        .engine_config(&deps.engine)
        .settings(engine_settings(
            &deps.home,
            deps.parse_cache,
            deps.lexical_spans,
        ))
        .access(access);
    for (name, embedder) in embedders(&deps.engine, &workspaces) {
        builder = builder.embedder(name, embedder);
    }
    for workspace in &workspaces {
        builder = builder.workspace(workspace.clone());
    }
    let engine = builder
        .build()
        .await
        .context("cannot start the Knowell engine")?;
    Ok(Some((engine, workspaces)))
}

/// Starts background indexing: an initial sync of every workspace, a job
/// worker, and file watching with periodic reconciliation. Everything stops
/// when `shutdown` is cancelled.
pub(crate) struct Background {
    shutdown: CancellationToken,
    initial: tokio::task::JoinHandle<()>,
    worker: tokio::task::JoinHandle<Result<(), knowell_index::IndexError>>,
    watcher: Option<knowell_index::WatchHandle>,
}

impl Background {
    /// Joins every writer before its database and runtime leases are released.
    pub(crate) async fn finish(self) -> anyhow::Result<()> {
        self.shutdown.cancel();
        if let Some(watcher) = self.watcher {
            watcher.stop().await;
        }
        self.initial
            .await
            .context("initial indexing did not stop cleanly")?;
        self.worker
            .await
            .context("index worker did not stop cleanly")??;
        Ok(())
    }
}

pub(crate) fn start_indexing(
    engine: &Engine,
    workspaces: Vec<ResolvedWorkspace>,
    shutdown: &CancellationToken,
) -> Background {
    let indexer = engine.indexer().clone();
    let token = shutdown.clone();
    let initial = tokio::spawn(async move {
        for workspace in &workspaces {
            let outcome = tokio::select! {
                biased;
                () = token.cancelled() => return,
                outcome = indexer.index_workspace(workspace, Priority::Active) => outcome,
            };
            match outcome {
                Ok((_, outcomes)) => {
                    for outcome in outcomes {
                        if let knowell_index::SyncOutcome::Failed { .. } = &outcome {
                            tracing::warn!("workspace {}: {outcome:?}", workspace.name);
                        }
                    }
                }
                Err(err) => tracing::error!("indexing workspace {} failed: {err}", workspace.name),
            }
        }
    });
    let worker = Worker::new(engine.indexer(), WorkerConfig::default());
    let worker = worker.spawn(shutdown.clone());
    let watcher = match engine.indexer().watch(shutdown.clone()) {
        Ok(handle) => {
            tracing::info!("watching {} working tree(s)", handle.watched().len());
            Some(handle)
        }
        Err(err) => {
            tracing::warn!("file watching is off: {err}");
            None
        }
    };
    Background {
        shutdown: shutdown.clone(),
        initial,
        worker,
        watcher,
    }
}

fn resolve_workspaces(files: &BTreeMap<Name, PathBuf>) -> Vec<ResolvedWorkspace> {
    let mut out = Vec::new();
    for (name, file) in files {
        let config = match knowell_config::load_workspace(file) {
            Ok(config) => config,
            Err(err) => {
                tracing::warn!("workspace {name} is skipped: {err}");
                continue;
            }
        };
        let base = match crate::env::parent_dir(file) {
            Ok(base) => base,
            Err(err) => {
                tracing::warn!("workspace {name} is skipped: {err:#}");
                continue;
            }
        };
        match config.resolve(&base) {
            Ok(resolved) => out.push(resolved),
            Err(issues) => tracing::warn!("workspace {name} is skipped: {issues}"),
        }
    }
    out
}

/// One embedder per provider used by any project. The dimension comes from
/// the projects' embedding settings; a provider used with two different
/// dimensions keeps the first and the conflict is reported.
fn embedders(
    engine: &EngineConfig,
    workspaces: &[ResolvedWorkspace],
) -> Vec<(Name, Arc<AnyEmbedder>)> {
    let mut wanted: BTreeMap<Name, (Option<String>, u32)> = BTreeMap::new();
    for workspace in workspaces {
        for project in &workspace.projects {
            let emb = &project.embedding;
            let Some(provider) = &emb.provider else {
                continue;
            };
            let model = emb.model.as_ref().map(|m| m.value.clone());
            let dims = emb.dimensions.value;
            match wanted.get(&provider.value) {
                Some((_, first)) if *first != dims => tracing::warn!(
                    "provider {} is used with {first} and {dims} dimensions; using {first} \
                     (project {} needs its own provider entry)",
                    provider.value,
                    project.name
                ),
                Some(_) => {}
                None => {
                    wanted.insert(provider.value.clone(), (model, dims));
                }
            }
        }
    }
    let mut out = Vec::new();
    for (name, (model, dims)) in wanted {
        match embedder(engine, &name, model, dims) {
            Ok(embedder) => out.push((name, Arc::new(embedder))),
            Err(err) => tracing::warn!(
                "embedding provider {name} is unavailable ({err:#}); its projects report semantic \
                 search as off"
            ),
        }
    }
    out
}

fn embedder(
    engine: &EngineConfig,
    name: &Name,
    model: Option<String>,
    dims: u32,
) -> anyhow::Result<AnyEmbedder> {
    let provider = engine
        .providers
        .get(name)
        .with_context(|| format!("no `[providers.{name}]` in the engine configuration"))?;
    let model = model
        .or_else(|| provider.model.clone())
        .context("no model configured")?;
    let secret = |required: bool| -> anyhow::Result<Option<secrecy::SecretString>> {
        match &provider.api_key {
            // The error names the reference only, never a value.
            Some(reference) => Ok(Some(knowell_secrets::resolve(reference)?)),
            None if required => anyhow::bail!("`api_key` is required"),
            None => Ok(None),
        }
    };
    let base_url = |default: Option<&str>| -> anyhow::Result<Option<Url>> {
        provider
            .effective_base_url()
            .or(default)
            .map(|u| Url::parse(u).context("`base_url` is not a valid URL"))
            .transpose()
    };
    Ok(match provider.kind {
        ProviderKind::Gemini => {
            let config = GeminiConfig {
                model,
                dimensions: dims,
                base_url: base_url(None)?,
                ..GeminiConfig::default()
            };
            let key = secret(true)?.context("`api_key` is required")?;
            AnyEmbedder::Gemini(GeminiEmbedder::new(key, config)?)
        }
        ProviderKind::Ollama => {
            let url = base_url(None)?.context("no base URL")?;
            AnyEmbedder::Ollama(OllamaEmbedder::new(OllamaConfig::new(url, model, dims))?)
        }
        ProviderKind::OpenaiCompatible => {
            let url = base_url(None)?.context("`base_url` is required")?;
            AnyEmbedder::OpenAiCompatible(OpenAiCompatibleEmbedder::new(
                secret(false)?,
                OpenAiCompatibleConfig::new(url, model, dims),
            )?)
        }
        ProviderKind::Voyage => anyhow::bail!("Voyage embeddings are not supported yet"),
    })
}

/// The engine behind the server's delegated REST routes; `None` makes them
/// answer `503 engine_unavailable`.
pub(crate) fn rest_engine(engine: Option<&Engine>) -> Option<Arc<dyn knowell_server::Engine>> {
    engine.map(|e| Arc::new(e.clone()) as Arc<dyn knowell_server::Engine>)
}

/// The [`KnowellTools`] implementation served over MCP (stdio and HTTP).
pub(crate) enum Tools {
    /// The real engine.
    Engine(Engine),
    /// No database: every tool answers `not_ready`.
    Unavailable(EngineUnavailable),
}

impl Tools {
    pub(crate) fn new(engine: Option<&Engine>) -> Self {
        match engine {
            Some(engine) => Tools::Engine(engine.clone()),
            None => Tools::Unavailable(EngineUnavailable),
        }
    }
}

/// Answers every tool with `not_ready` when there is no engine.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct EngineUnavailable;

fn not_wired() -> ToolError {
    ToolError::NotReady {
        message: NOT_WIRED.to_owned(),
        retry_after_ms: None,
    }
}

macro_rules! tools_impls {
    ($($method:ident($input:ty) -> $output:ty;)*) => {
        impl KnowellTools for EngineUnavailable {
            $(
                async fn $method(&self, _caller: &Caller, _input: $input) -> Result<$output, ToolError> {
                    Err(not_wired())
                }
            )*
        }

        impl KnowellTools for Tools {
            $(
                async fn $method(&self, caller: &Caller, input: $input) -> Result<$output, ToolError> {
                    match self {
                        Tools::Engine(engine) => engine.$method(caller, input).await,
                        Tools::Unavailable(none) => none.$method(caller, input).await,
                    }
                }
            )*
        }
    };
}

tools_impls! {
    open_workspace(OpenWorkspaceInput) -> OpenWorkspaceOutput;
    search(SearchInput) -> SearchOutput;
    fetch(FetchInput) -> FetchOutput;
    inspect_symbol(InspectSymbolInput) -> InspectSymbolOutput;
    trace_flow(TraceFlowInput) -> TraceFlowOutput;
    analyze_impact(AnalyzeImpactInput) -> AnalyzeImpactOutput;
    contracts(ContractsInput) -> ContractsOutput;
    build_context(BuildContextInput) -> BuildContextOutput;
    history(HistoryInput) -> HistoryOutput;
    read_memory(ReadMemoryInput) -> ReadMemoryOutput;
    write_memory(WriteMemoryInput) -> WriteMemoryOutput;
    resume_task(ResumeTaskInput) -> ResumeTaskOutput;
    save_checkpoint(SaveCheckpointInput) -> SaveCheckpointOutput;
    index_status(IndexStatusInput) -> IndexStatusOutput;
}
