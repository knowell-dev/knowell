//! Strict, one-workspace engine construction for local terminal commands.
//!
//! These commands act as the standalone installation's local caller. They
//! never start watchers or substitute workspaces. Index and search prepare every
//! selected provider; explicit record reads retain profiles without transports.
//! Profile catalogue reads use the engine configuration without selecting sources.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::Args;
use knowell_config::{EngineConfig, ProviderConfig, ProviderKind, ResolvedWorkspace, ServerRole};
use knowell_core::{ContentHash, Name};
use knowell_embed::{
    AnyEmbedder, GeminiConfig, GeminiEmbedder, OllamaConfig, OllamaEmbedder,
    OpenAiCompatibleConfig, OpenAiCompatibleEmbedder,
};
use knowell_engine::{Engine, StoreAccess};
use knowell_index::{
    EmbeddingCoverage, EmbeddingPlan, IndexerConfig, RegisteredView, Registration, Tier, TierSkip,
    TierState, ViewStatus,
};
use knowell_mcp::{Caller, ClientIdentity, TransportKind};
use knowell_store::content;
use knowell_store::views::GenerationPin;
use knowell_store::{SourceKind, Store};
use serde::Serialize;
use url::Url;

use crate::db;
use crate::env::{self, Env};

/// Tenant selected explicitly by the local user; defaults to the local install.
#[derive(Debug, Args)]
pub(crate) struct LocalArgs {
    /// Organization whose local index to use.
    #[arg(long, default_value = "local")]
    pub(crate) organization: Name,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderSpec {
    model: String,
    dimensions: u32,
}

enum ProviderPreparation {
    Required(BTreeMap<Name, ProviderSpec>),
    RecordsOnly,
}

/// Validated selection, before database connections or provider credentials.
pub(crate) struct Prepared {
    config: EngineConfig,
    pub(crate) workspace: ResolvedWorkspace,
    organization: Name,
    providers: ProviderPreparation,
}

/// Validated catalogue selection, with no workspace or provider preparation.
pub(crate) struct CataloguePrepared {
    config: EngineConfig,
    organization: Name,
}

impl CataloguePrepared {
    /// Requires only a standalone engine configuration; credential references stay unresolved.
    pub(crate) fn load(env: &Env, organization: Name) -> anyhow::Result<Self> {
        Ok(Self {
            config: standalone_config(env)?,
            organization,
        })
    }

    /// Opens the real engine with zero provider clients and no source registration.
    /// Existing migrations and organization setup are the only database initialization writes.
    pub(crate) async fn open(self, env: &Env) -> anyhow::Result<CatalogueEngine> {
        let store = db::connect(env, &self.config, Duration::from_secs(15), 10)
            .await
            .context("cannot open the local catalogue database; run `know doctor` for details")?;
        let result: anyhow::Result<CatalogueEngine> = async {
            store
                .validate_schema()
                .await
                .context("database upgrade required; run an explicit maintenance migration")?;
            let user =
                knowell_auth::UserId::new(uuid::Uuid::from_u128(crate::serve_cmd::LOCAL_USER));
            let access =
                StoreAccess::new(store.clone(), self.organization.clone()).with_local_user(user);
            let engine = Engine::builder(
                store.clone(),
                IndexerConfig::new(env.home.join("data"), self.organization.clone()),
            )
            .engine_config(&self.config)
            .access(Arc::new(access))
            .build()
            .await
            .context("cannot start the local catalogue engine")?;
            Ok(CatalogueEngine {
                engine,
                organization: self.organization,
            })
        }
        .await;
        if result.is_err() {
            store.close().await;
        }
        result
    }
}

/// A source-free engine owned by one profile catalogue command.
pub(crate) struct CatalogueEngine {
    pub(crate) engine: Engine,
    pub(crate) organization: Name,
}

impl Prepared {
    /// Requires one selected workspace and a standalone engine configuration.
    pub(crate) fn load(env: &Env, organization: Name) -> anyhow::Result<Self> {
        let (config, workspace) = selected_config(env)?;
        let providers = provider_specs(&config, &workspace)?;
        Ok(Self {
            config,
            workspace,
            organization,
            providers: ProviderPreparation::Required(providers),
        })
    }

    /// Validates record-read configuration without preparing provider transports.
    /// Configured profiles and policy are retained; credential references stay unresolved.
    pub(crate) fn load_records(env: &Env, organization: Name) -> anyhow::Result<Self> {
        let (config, workspace) = selected_config(env)?;
        let issues = workspace.check_against(&config);
        if !issues.is_empty() {
            bail!("{}", knowell_config::ConfigIssues(issues));
        }
        Ok(Self {
            config,
            workspace,
            organization,
            providers: ProviderPreparation::RecordsOnly,
        })
    }

    /// Rejects unknown, duplicate or excessive project filters before opening a store.
    pub(crate) fn validate_projects(&self, projects: &[Name]) -> anyhow::Result<()> {
        validate_projects(&self.workspace, projects)
    }

    /// Opens the configured store and registers the selected workspace once.
    /// No indexing or provider request is started here.
    pub(crate) async fn open(self, env: &Env) -> anyhow::Result<LocalEngine> {
        let ProviderPreparation::Required(providers) = &self.providers else {
            bail!("record-read preparation requires the record-read opener");
        };
        let embedders = providers
            .iter()
            .map(|(name, spec)| {
                let provider = self
                    .config
                    .providers
                    .get(name)
                    .with_context(|| format!("embedding provider `{name}` is missing"))?;
                Ok((name.clone(), Arc::new(embedder(provider, spec)?)))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        self.open_with(env, embedders).await
    }

    /// Opens the real engine for memory and task reads without provider clients.
    /// This does not change configured profiles or schedule indexing work.
    pub(crate) async fn open_records(self, env: &Env) -> anyhow::Result<LocalEngine> {
        self.open_with(env, Vec::new()).await
    }

    async fn open_with(
        self,
        env: &Env,
        embedders: Vec<(Name, Arc<AnyEmbedder>)>,
    ) -> anyhow::Result<LocalEngine> {
        let store = db::connect(env, &self.config, Duration::from_secs(15), 10)
            .await
            .context("cannot open the local index database; run `know doctor` for details")?;
        let result: anyhow::Result<LocalEngine> = async {
            store
                .validate_schema()
                .await
                .context("database upgrade required; run an explicit maintenance migration")?;
            let user =
                knowell_auth::UserId::new(uuid::Uuid::from_u128(crate::serve_cmd::LOCAL_USER));
            let access =
                StoreAccess::new(store.clone(), self.organization.clone()).with_local_user(user);
            let mut builder = Engine::builder(
                store.clone(),
                IndexerConfig::new(env.home.join("data"), self.organization),
            )
            .engine_config(&self.config)
            .settings(crate::tools::engine_settings(
                &env.home,
                env.parse_cache,
                env.lexical_spans,
            ))
            .access(Arc::new(access));
            for (name, embedder) in embedders {
                builder = builder.embedder(name, embedder);
            }
            let engine = builder
                .build()
                .await
                .context("cannot start the local engine")?;
            let registration = engine.add_workspace(&self.workspace).await?;
            Ok(LocalEngine {
                engine,
                workspace: self.workspace,
                registration,
            })
        }
        .await;
        if result.is_err() {
            store.close().await;
        }
        result
    }
}

fn selected_config(env: &Env) -> anyhow::Result<(EngineConfig, ResolvedWorkspace)> {
    let config = standalone_config(env)?;
    let file = env.require_workspace()?;
    let workspace = knowell_config::load_workspace(&file)?.resolve(&env::parent_dir(&file)?)?;
    Ok((config, workspace))
}

fn standalone_config(env: &Env) -> anyhow::Result<EngineConfig> {
    let config = env.require_engine()?;
    if config.server.role != ServerRole::Standalone {
        bail!(
            "local commands require role `standalone`; the configured role is `{}`",
            config.server.role.as_str()
        );
    }
    Ok(config)
}

fn validate_projects(workspace: &ResolvedWorkspace, projects: &[Name]) -> anyhow::Result<()> {
    if projects.len() > knowell_mcp::tools::limits::MAX_LIST_ITEMS {
        bail!("too many project filters");
    }
    let mut seen = BTreeSet::new();
    for project in projects {
        if !seen.insert(project) {
            bail!("project `{project}` was selected more than once");
        }
        if !workspace.projects.iter().any(|p| &p.name == project) {
            bail!(
                "project `{project}` does not exist in workspace `{}`",
                workspace.name
            );
        }
    }
    Ok(())
}

fn provider_specs(
    config: &EngineConfig,
    workspace: &ResolvedWorkspace,
) -> anyhow::Result<BTreeMap<Name, ProviderSpec>> {
    let issues = workspace.check_against(config);
    if !issues.is_empty() {
        bail!("{}", knowell_config::ConfigIssues(issues));
    }
    let mut specs = BTreeMap::new();
    for project in &workspace.projects {
        let Some(name) = project.embedding.provider.as_ref().map(|p| &p.value) else {
            continue;
        };
        let provider = config
            .providers
            .get(name)
            .with_context(|| format!("embedding provider `{name}` is missing"))?;
        if provider.kind == ProviderKind::Voyage {
            bail!("voyage embeddings are not supported yet");
        }
        let model = project
            .embedding
            .model
            .as_ref()
            .map(|m| &m.value)
            .or(provider.model.as_ref())
            .context("an embedding model is required")?;
        let spec = ProviderSpec {
            model: model.clone(),
            dimensions: project.embedding.dimensions.value,
        };
        match specs.get(name) {
            Some(previous) if previous != &spec => bail!(
                "embedding provider `{name}` is used with different models or dimensions; use separate provider entries"
            ),
            Some(_) => {}
            None => {
                specs.insert(name.clone(), spec);
            }
        }
    }
    Ok(specs)
}

fn embedder(provider: &ProviderConfig, spec: &ProviderSpec) -> anyhow::Result<AnyEmbedder> {
    let base = provider
        .effective_base_url()
        .map(|value| {
            let url = Url::parse(value).context("the provider base URL is invalid")?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                bail!(
                    "the provider base URL must be http(s) without credentials, query or fragment"
                );
            }
            Ok(url)
        })
        .transpose()?;
    let secret = |required: bool| -> anyhow::Result<Option<secrecy::SecretString>> {
        match &provider.api_key {
            Some(reference) => Ok(Some(knowell_secrets::resolve(reference)?)),
            None if required => bail!("the embedding provider requires an API key reference"),
            None => Ok(None),
        }
    };
    Ok(match provider.kind {
        ProviderKind::Gemini => AnyEmbedder::Gemini(GeminiEmbedder::new(
            secret(true)?.context("the embedding provider requires an API key reference")?,
            GeminiConfig {
                model: spec.model.clone(),
                dimensions: spec.dimensions,
                base_url: base,
                ..GeminiConfig::default()
            },
        )?),
        ProviderKind::Ollama => AnyEmbedder::Ollama(OllamaEmbedder::new(OllamaConfig::new(
            base.context("the Ollama provider requires a base URL")?,
            spec.model.clone(),
            spec.dimensions,
        ))?),
        ProviderKind::OpenaiCompatible => {
            AnyEmbedder::OpenAiCompatible(OpenAiCompatibleEmbedder::new(
                secret(false)?,
                OpenAiCompatibleConfig::new(
                    base.context("the OpenAI-compatible provider requires a base URL")?,
                    spec.model.clone(),
                    spec.dimensions,
                ),
            )?)
        }
        ProviderKind::Voyage => bail!("voyage embeddings are not supported yet"),
    })
}

/// An engine with explicit registration issues, owned by one terminal command.
pub(crate) struct LocalEngine {
    pub(crate) engine: Engine,
    pub(crate) workspace: ResolvedWorkspace,
    pub(crate) registration: Registration,
}

impl LocalEngine {
    /// Rejects registration failures for the projects a search would otherwise omit.
    pub(crate) fn require_registered(&self, projects: &[Name]) -> anyhow::Result<()> {
        for issue in &self.registration.issues {
            if projects.is_empty() || projects.contains(&issue.project) {
                bail!(
                    "project `{}` could not be registered: {}",
                    issue.project,
                    issue.reason
                );
            }
        }
        Ok(())
    }

    /// Reads tiers and semantic coverage without suppressing store failures.
    pub(crate) async fn status(
        &self,
        registered: &RegisteredView,
    ) -> anyhow::Result<ProjectStatus> {
        let status = self.engine.indexer().status(registered.view).await?;
        let embedding_coverage = self
            .engine
            .indexer()
            .embedding_coverage(registered.view)
            .await?;
        let active_tree_hash = if registered.source_kind == SourceKind::Directory {
            match status.active_generation {
                Some(generation) => {
                    let mut conn = self.engine.store().acquire().await?;
                    let files = content::files_at(
                        &mut conn,
                        GenerationPin {
                            view: registered.view,
                            generation,
                        },
                    )
                    .await?;
                    Some(knowell_index::tree_hash(
                        files.iter().map(|file| (&file.path, &file.content_hash)),
                    ))
                }
                None => None,
            }
        } else {
            None
        };
        Ok(ProjectStatus {
            status,
            embedding: registered.embedding.clone(),
            embedding_coverage,
            active_tree_hash,
        })
    }

    /// The configured pool, for explicit cleanup before the runtime exits.
    pub(crate) fn store(&self) -> &Store {
        self.engine.store()
    }
}

/// Actual freshness, configured embedding plan and coverage of one project view.
#[derive(Debug, Serialize)]
pub(crate) struct ProjectStatus {
    #[serde(flatten)]
    pub(crate) status: ViewStatus,
    pub(crate) embedding: EmbeddingPlan,
    pub(crate) embedding_coverage: Option<EmbeddingCoverage>,
    pub(crate) active_tree_hash: Option<ContentHash>,
}

/// Reasons a view cannot be described as fully indexed at its latest seen target.
pub(crate) fn incomplete_reasons(project: &ProjectStatus) -> Vec<String> {
    let mut reasons = Vec::new();
    let status = &project.status;
    if status.active_generation.is_none() {
        reasons.push("no index generation is active".to_owned());
    }
    if status.building_generation.is_some() {
        reasons.push("a generation is still building".to_owned());
    }
    if status.latest_seen_commit != status.active_commit {
        reasons.push("the active index does not match the latest seen commit".to_owned());
    }
    if let Some(error) = &status.last_error {
        reasons.push(error.clone());
    }
    for tier in Tier::ALL {
        match status.tiers.get(tier) {
            TierState::Done => {}
            TierState::Skipped {
                reason: TierSkip::NoProvider | TierSkip::DataPolicyLocalOnly,
            } if tier == Tier::T2 => {}
            TierState::Skipped { reason } => {
                reasons.push(format!("{} skipped: {}", tier.as_str(), reason.as_str()));
            }
            TierState::Failed { reason } => {
                reasons.push(format!("{} failed: {reason}", tier.as_str()));
            }
            TierState::Pending => reasons.push(format!("{} is pending", tier.as_str())),
            TierState::Running => reasons.push(format!("{} is running", tier.as_str())),
        }
    }
    match &project.embedding {
        EmbeddingPlan::Unavailable { reason } => {
            reasons.push(format!("embeddings unavailable: {reason}"))
        }
        EmbeddingPlan::Embed { profile, .. } => match project.embedding_coverage {
            Some(coverage)
                if coverage.complete
                    && coverage.embedded == coverage.inputs
                    && Some(coverage.generation) == status.active_generation
                    && coverage.profile == *profile => {}
            _ => {
                reasons.push("embedding coverage of the active generation is incomplete".to_owned())
            }
        },
        EmbeddingPlan::Skip {
            reason: TierSkip::BudgetExhausted,
        } => {
            reasons.push("the embedding budget is exhausted".to_owned());
        }
        EmbeddingPlan::Skip { .. } => {}
    }
    reasons
}

/// Local stdio identity; the standalone guard prevents its use against a hub.
pub(crate) fn caller() -> Caller {
    Caller {
        client: Some(ClientIdentity {
            name: "know-cli".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }),
        ..Caller::local(TransportKind::Stdio)
    }
}

/// Removes terminal controls from repository-derived human-readable output.
pub(crate) fn terminal_text(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configs(projects: &str) -> (EngineConfig, ResolvedWorkspace) {
        let engine = knowell_config::parse_engine(
            "version = 1\n[providers.local]\nkind = 'ollama'\nmodel = 'synthetic-model'\n",
        )
        .unwrap();
        let workspace = knowell_config::parse_workspace(&format!(
            "version = 1\n[workspace]\nname = 'synthetic'\ntrack = 'worktree'\n[workspace.embedding]\nprovider = 'local'\n{projects}"
        )).unwrap().resolve(std::env::temp_dir().as_path()).unwrap();
        (engine, workspace)
    }

    #[test]
    fn provider_requirements_use_the_effective_model_and_dimensions() {
        let (engine, workspace) = configs(
            "[[project]]\nname = 'one'\npath = 'one'\n[[project]]\nname = 'two'\npath = 'two'\n[project.embedding]\nmodel = 'synthetic-model'\n",
        );
        let specs = provider_specs(&engine, &workspace).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs.values().next().unwrap().model, "synthetic-model");
        assert_eq!(specs.values().next().unwrap().dimensions, 1536);
    }

    #[test]
    fn conflicting_models_or_dimensions_are_not_substituted() {
        for setting in ["model = 'different-model'", "preset = 'compact'"] {
            let (engine, workspace) = configs(&format!(
                "[[project]]\nname = 'one'\npath = 'one'\n[[project]]\nname = 'two'\npath = 'two'\n[project.embedding]\n{setting}\n"
            ));
            let error = provider_specs(&engine, &workspace).unwrap_err();
            assert!(error.to_string().contains("different models or dimensions"));
        }
    }

    #[test]
    fn unknown_and_duplicate_projects_are_errors() {
        let (_, workspace) = configs("[[project]]\nname = 'one'\npath = 'one'\n");
        let one = Name::new("one").unwrap();
        assert!(validate_projects(&workspace, std::slice::from_ref(&one)).is_ok());
        assert!(validate_projects(&workspace, &[one.clone(), one]).is_err());
        assert!(validate_projects(&workspace, &[Name::new("unknown").unwrap()]).is_err());
    }

    #[test]
    fn rejected_provider_urls_do_not_echo_the_input() {
        let spec = ProviderSpec {
            model: "synthetic-model".to_owned(),
            dimensions: 64,
        };
        for base in [
            "https://user:KNOWELL_CANARY_PASSWORD@example.invalid",
            "https://example.invalid?key=KNOWELL_CANARY_QUERY",
            "https://example.invalid#KNOWELL_CANARY_FRAGMENT",
        ] {
            let provider = ProviderConfig {
                kind: ProviderKind::OpenaiCompatible,
                api_key: None,
                base_url: Some(base.to_owned()),
                model: Some(spec.model.clone()),
            };
            let error = embedder(&provider, &spec).err().unwrap();
            assert!(!format!("{error:#}").contains("KNOWELL_CANARY"));
        }
    }

    #[test]
    fn human_output_cannot_emit_terminal_escape_sequences() {
        let output = terminal_text("file\n\u{1b}[31m\tname");
        assert_eq!(output, "file  [31m name");
        assert!(!output.chars().any(char::is_control));
    }

    #[test]
    fn catalogue_preparation_needs_no_workspace_or_provider_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("engine.toml");
        std::fs::write(
            &config_path,
            "version = 1\n[providers.unavailable]\nkind = 'gemini'\nmodel = 'synthetic-model'\napi_key = 'env:KNOWELL_CANARY_UNAVAILABLE_PROVIDER'\n",
        )
        .unwrap();
        let env = Env::from_globals(&crate::GlobalArgs {
            engine_config: Some(config_path),
            workspace_file: Some(temp.path().join("does-not-exist.toml")),
            parse_cache: false,
            lexical_spans: 1,
            verbose: 0,
            quiet: false,
        })
        .unwrap();
        let organization = Name::new("synthetic-organization").unwrap();
        let prepared = CataloguePrepared::load(&env, organization.clone()).unwrap();
        assert_eq!(prepared.organization, organization);
        assert_eq!(prepared.config.providers.len(), 1);
        assert!(
            prepared
                .config
                .providers
                .contains_key(&Name::new("unavailable").unwrap())
        );
    }

    #[test]
    fn catalogue_preparation_refuses_hub_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("engine.toml");
        std::fs::write(&config_path, "version = 1\n[server]\nrole = 'hub'\n").unwrap();
        let env = Env::from_globals(&crate::GlobalArgs {
            engine_config: Some(config_path),
            workspace_file: None,
            parse_cache: false,
            lexical_spans: 1,
            verbose: 0,
            quiet: false,
        })
        .unwrap();
        let error = CataloguePrepared::load(&env, Name::new("local").unwrap())
            .err()
            .unwrap();
        assert!(error.to_string().contains("require role `standalone`"));
    }
}
