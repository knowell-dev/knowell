//! Evaluate the real engine in a fresh, disposable database. The admin
//! connection is used only to create/drop that generated database.

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use knowell_auth::{Grant, GrantSet, Principal, ResourceScope, Role, UserId};
use knowell_config::{EmbeddingPreset, Origin, Sourced};
use knowell_core::{Name, SecretRef};
use knowell_embed::{AnyEmbedder, FAKE_MODEL, FakeEmbedder};
use knowell_engine::{Access, Engine, HybridRetriever};
use knowell_eval::{Fixture, Report};
use knowell_index::{GitConfigMode, IndexerConfig, Priority, SyncOutcome};
use knowell_store::{PgConnectOptions, Store, StoreOptions};
use secrecy::ExposeSecret;
use sqlx::ConnectOptions;

const DIMENSIONS: u32 = 64;

pub(crate) fn measure(
    root: &Path,
    fixture: &Fixture,
    reference: &str,
    run: impl FnOnce(&HybridRetriever) -> anyhow::Result<Report>,
) -> anyhow::Result<Report> {
    let reference = SecretRef::from_str(reference)?;
    let url = knowell_secrets::resolve(&reference)?;
    let admin = PgConnectOptions::from_str(url.expose_secret())
        .map_err(|_| anyhow!("the evaluation database URL reference is invalid"))?
        .disable_statement_logging();
    let rt = crate::db::runtime()?;
    rt.block_on(async {
        let mut connection = admin
            .connect()
            .await
            .map_err(|_| anyhow!("cannot connect to the evaluation PostgreSQL server"))?;
        // Only this generated identifier enters SQL; neither the URL nor any
        // caller-controlled identifier can select a database to drop.
        let database = format!("knowell_eval_{}", uuid::Uuid::now_v7().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE DATABASE \"{database}\""
        )))
        .execute(&mut connection)
        .await
        .map_err(|_| anyhow!("cannot create the evaluation database; the role needs CREATEDB"))?;
        let result = evaluate(root, fixture, admin.database(&database), run).await;
        let cleanup = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE \"{database}\" WITH (FORCE)"
        )))
        .execute(&mut connection)
        .await;
        if cleanup.is_err() {
            // The name is generated, not taken from secret connection input.
            tracing::error!("cannot remove scratch database {database}; remove it manually");
            return Err(result
                .err()
                .unwrap_or_else(|| anyhow!("evaluation database cleanup failed")));
        }
        result
    })
}

async fn evaluate(
    root: &Path,
    fixture: &Fixture,
    options: PgConnectOptions,
    run: impl FnOnce(&HybridRetriever) -> anyhow::Result<Report>,
) -> anyhow::Result<Report> {
    let store = Store::connect_with(options, &StoreOptions::default()).await?;
    let result = build_and_measure(root, fixture, &store, run).await;
    store.close().await;
    result
}

async fn build_and_measure(
    root: &Path,
    fixture: &Fixture,
    store: &Store,
    run: impl FnOnce(&HybridRetriever) -> anyhow::Result<Report>,
) -> anyhow::Result<Report> {
    store.migrate().await?;
    if !store.check_server().await?.semantic_enabled() {
        bail!(
            "hybrid evaluation requires pgvector 0.8 or newer; no lexical-only fallback is measured"
        );
    }
    let workspace = knowell_config::load_workspace(&root.join("knowell.toml"))?;
    let mut resolved = workspace.resolve(root)?;
    let provider = Name::new("eval")?;
    for project in &mut resolved.projects {
        project.embedding.provider = Some(sourced(provider.clone()));
        project.embedding.model = Some(sourced(FAKE_MODEL.to_owned()));
        project.embedding.preset = sourced(EmbeddingPreset::Custom);
        project.embedding.dimensions = sourced(DIMENSIONS);
    }
    let engine_config = knowell_config::parse_engine(&format!(
        "version = 1\n[providers.eval]\nkind = \"ollama\"\nmodel = \"{FAKE_MODEL}\"\n"
    ))?;
    let data = tempfile::tempdir().context("cannot create evaluation index directory")?;
    let mut config = IndexerConfig::new(data.path(), Name::new("eval")?);
    config.git_config = GitConfigMode::Isolated;
    // Stable insertion order also makes approximate index construction
    // reproducible; these measurements do not benchmark indexing throughput.
    config.concurrency = 1;
    let engine = Engine::builder(store.clone(), config)
        .engine_config(&engine_config)
        .embedder(
            provider,
            Arc::new(AnyEmbedder::Fake(FakeEmbedder::new(DIMENSIONS)?)),
        )
        .workspace(resolved.clone())
        .build()
        .await?;
    let (registration, outcomes) = engine
        .indexer()
        .index_workspace(&resolved, Priority::Interactive)
        .await?;
    if !registration.issues.is_empty()
        || registration.views.len() != fixture.projects().len()
        || outcomes
            .iter()
            .any(|outcome| matches!(outcome, SyncOutcome::Failed { .. }))
    {
        bail!("not every synthetic project was indexed; refusing an incomplete hybrid measurement");
    }
    for view in &registration.views {
        let coverage = engine.indexer().embedding_coverage(view.view).await?;
        if !coverage.is_some_and(|c| c.complete && c.inputs == c.embedded) {
            bail!(
                "synthetic embedding coverage is incomplete; refusing a degraded hybrid measurement"
            );
        }
    }
    let user = Principal::User(UserId::new(uuid::Uuid::from_u128(1)));
    let mut grants = GrantSet::new();
    grants.add(Grant::new(
        user.clone(),
        Role::Admin,
        ResourceScope::Organization,
    )?);
    let access = Access::new(user, Arc::new(grants));
    let hybrid = HybridRetriever::new(&engine, &access, &Name::new(fixture.name())?).await?;
    run(&hybrid)
}

fn sourced<T>(value: T) -> Sourced<T> {
    Sourced {
        value,
        origin: Origin::Workspace,
    }
}
