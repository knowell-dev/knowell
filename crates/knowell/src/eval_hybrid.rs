//! Evaluate the real engine in a fresh, disposable database. The admin
//! connection is used only to create/drop that generated database.

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, anyhow, bail};
use knowell_auth::{Grant, GrantSet, Principal, ResourceScope, Role, UserId};
use knowell_config::{DataPolicy, EmbeddingPreset, Origin, Sourced};
use knowell_core::{Name, SecretRef};
use knowell_embed::{
    AnyEmbedder, Budget, Embedder, FakeEmbedder, GEMINI_EMBEDDING_MODEL, GeminiConfig,
    GeminiEmbedder,
};
use knowell_engine::{Access, Engine, HybridRetriever};
use knowell_eval::{Fixture, Report};
use knowell_index::{GitConfigMode, IndexerConfig, Priority, SyncOutcome};
use knowell_store::{PgConnectOptions, Store, StoreOptions};
use secrecy::ExposeSecret;
use sqlx::ConnectOptions;

const DIMENSIONS: u32 = 64;
// Text-only synchronous API price, checked against Google's pricing on 2026-10-03.
const USD_PER_MILLION: f64 = 0.20;

pub(crate) struct LiveOptions {
    key: SecretRef,
    dimensions: u32,
    max_tokens: u64,
    #[cfg(test)]
    base_url: Option<url::Url>,
}

impl LiveOptions {
    pub(crate) fn new(reference: &str, dimensions: u32, max_tokens: u64) -> anyhow::Result<Self> {
        let key = SecretRef::from_str(reference)?;
        if !matches!(dimensions, 768 | 1536 | 3072) {
            bail!("live evaluation dimensions must be 768, 1536 or 3072");
        }
        if !(1..=500_000).contains(&max_tokens) {
            bail!("live evaluation token budget must be between 1 and 500000");
        }
        Ok(Self {
            key,
            dimensions,
            max_tokens,
            #[cfg(test)]
            base_url: None,
        })
    }
}

#[derive(serde::Serialize)]
pub(crate) struct LiveEvidence {
    model: &'static str,
    dimensions: u32,
    request_dimension_field: &'static str,
    all_returned_dimensions_validated: bool,
    token_budget: u64,
    accounted_input_tokens: u64,
    accounting_note: &'static str,
    usd_per_million_tokens: f64,
    cost_estimate_usd: f64,
    provider_retries: u32,
    request_units_per_minute: Option<u32>,
    estimated_tokens_per_minute: Option<u32>,
    batch_max_inputs: usize,
    batch_max_estimated_tokens: u64,
    indexed_inputs: u64,
    elapsed_seconds: f64,
    os: &'static str,
    architecture: &'static str,
    postgres: String,
    pgvector: Option<String>,
    cache: &'static str,
}

impl LiveEvidence {
    pub(crate) fn markdown(&self) -> String {
        format!(
            "\n## Live embedding conditions\n\nModel: `{}`; dimensions: {}; all returned dimensions validated.\n\nRequest field: `{}`. Fresh database and vector cache; serial indexing; default engine weights, retrieval quotas and chunking. Input accounting: {} tokens (provider counts where available, estimates otherwise); budget: {} tokens; estimated cost: ${:.6} at ${:.2}/million text tokens. No provider or job retries. These are accounting estimates, not a provider billing guarantee.\n\nProvider rate caps: {} input request units/minute, {} estimated tokens/minute; batch caps: {} inputs, {} estimated tokens. Rate buckets initially hold one minute of capacity and refill continuously.\n\nIndexed inputs: {}; elapsed: {:.2} seconds; OS/architecture: {}/{}; PostgreSQL: {}; pgvector: {}. Latency includes cold indexing and all three retrievers; it is not search p95.\n",
            self.model,
            self.dimensions,
            self.request_dimension_field,
            self.accounted_input_tokens,
            self.token_budget,
            self.cost_estimate_usd,
            self.usd_per_million_tokens,
            self.request_units_per_minute
                .map_or_else(|| "unlimited".into(), |v| v.to_string()),
            self.estimated_tokens_per_minute
                .map_or_else(|| "unlimited".into(), |v| v.to_string()),
            self.batch_max_inputs,
            self.batch_max_estimated_tokens,
            self.indexed_inputs,
            self.elapsed_seconds,
            self.os,
            self.architecture,
            self.postgres,
            self.pgvector.as_deref().unwrap_or("unavailable")
        )
    }
}

pub(crate) fn measure(
    root: &Path,
    fixture: &Fixture,
    reference: &str,
    live: Option<&LiveOptions>,
    run: impl FnOnce(&HybridRetriever) -> anyhow::Result<Report>,
) -> anyhow::Result<(Report, Option<LiveEvidence>)> {
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
        let result = evaluate(root, fixture, admin.database(&database), live, run).await;
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
    live: Option<&LiveOptions>,
    run: impl FnOnce(&HybridRetriever) -> anyhow::Result<Report>,
) -> anyhow::Result<(Report, Option<LiveEvidence>)> {
    let store = Store::connect_with(options, &StoreOptions::default()).await?;
    let result = build_and_measure(root, fixture, &store, live, run).await;
    store.close().await;
    result
}

async fn build_and_measure(
    root: &Path,
    fixture: &Fixture,
    store: &Store,
    live: Option<&LiveOptions>,
    run: impl FnOnce(&HybridRetriever) -> anyhow::Result<Report>,
) -> anyhow::Result<(Report, Option<LiveEvidence>)> {
    let started = Instant::now();
    store.migrate().await?;
    if !store.check_server().await?.semantic_enabled() {
        bail!(
            "hybrid evaluation requires pgvector 0.8 or newer; no lexical-only fallback is measured"
        );
    }
    let workspace = knowell_config::load_workspace(&root.join("knowell.toml"))?;
    let mut resolved = workspace.resolve(root)?;
    let provider = Name::new("eval")?;
    let budget = live
        .map(|options| Budget::new(Some(options.max_tokens), None, USD_PER_MILLION))
        .transpose()?;
    let mut live_rate_limits = None;
    let embedder = match live {
        Some(options) => {
            let mut config = GeminiConfig {
                dimensions: options.dimensions,
                budget: budget.clone(),
                ..GeminiConfig::default()
            };
            config.limits.max_concurrency = 1;
            // Keep the initial burst plus one minute of refill below the
            // observed free-tier 100 input/minute and 30,000 token/minute quotas.
            config.batch.max_entries = 40;
            config.batch.max_batch_tokens = 8_192;
            config.limits.requests_per_minute = Some(40);
            config.limits.tokens_per_minute = Some(10_000);
            config.limits.retry.max_retries = 0;
            #[cfg(test)]
            {
                config.base_url = options.base_url.clone();
                config.limits.requests_per_minute = None;
                config.limits.tokens_per_minute = None;
            }
            live_rate_limits = Some((config.batch, config.limits));
            AnyEmbedder::Gemini(GeminiEmbedder::new(
                knowell_secrets::resolve(&options.key)?,
                config,
            )?)
        }
        None => AnyEmbedder::Fake(FakeEmbedder::new(DIMENSIONS)?),
    };
    let model = embedder.profile().model.clone();
    let dimensions = embedder.profile().dimensions;
    for project in &mut resolved.projects {
        project.embedding.provider = Some(sourced(provider.clone()));
        project.embedding.model = Some(sourced(model.clone()));
        project.embedding.preset = sourced(EmbeddingPreset::Custom);
        project.embedding.dimensions = sourced(dimensions);
        if live.is_some() {
            // The explicit live command operates only on our newly generated
            // synthetic fixture, never a registered or supplied workspace.
            project.data_policy = sourced(DataPolicy::Cloud);
        }
    }
    let kind = if live.is_some() { "gemini" } else { "ollama" };
    let key_reference = live
        .map(|options| {
            format!(
                "api_key = {}\n",
                toml::Value::String(options.key.to_string())
            )
        })
        .unwrap_or_default();
    let engine_config = knowell_config::parse_engine(&format!(
        "version = 1\n[providers.eval]\nkind = \"{kind}\"\nmodel = \"{model}\"\n{key_reference}"
    ))?;
    let data = tempfile::tempdir().context("cannot create evaluation index directory")?;
    let mut config = IndexerConfig::new(data.path(), Name::new("eval")?);
    config.git_config = GitConfigMode::Isolated;
    // Stable insertion order also makes approximate index construction
    // reproducible; these measurements do not benchmark indexing throughput.
    config.concurrency = 1;
    config.jobs.max_attempts = 1;
    let engine = Engine::builder(store.clone(), config)
        .engine_config(&engine_config)
        .embedder(provider, Arc::new(embedder))
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
            let status = engine.indexer().status(view.view).await?;
            bail!(
                "synthetic embedding coverage is incomplete for {}: {:?}; refusing a degraded hybrid measurement",
                view.project,
                status.tiers.t2
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
    let report = run(&hybrid)?;
    let evidence = match (live, budget) {
        (Some(options), Some(budget)) => {
            let server = store.check_server().await?;
            let (batch, limits) =
                live_rate_limits.context("live evaluation rate limits are missing")?;
            Some(LiveEvidence {
                model: GEMINI_EMBEDDING_MODEL,
                dimensions,
                request_dimension_field: "requests[].outputDimensionality",
                all_returned_dimensions_validated: true,
                token_budget: options.max_tokens,
                accounted_input_tokens: budget.spent_tokens(),
                accounting_note: "provider-reported when available, otherwise estimated; not a billing guarantee",
                usd_per_million_tokens: USD_PER_MILLION,
                cost_estimate_usd: budget.spent_usd(),
                provider_retries: 0,
                request_units_per_minute: limits.requests_per_minute,
                estimated_tokens_per_minute: limits.tokens_per_minute,
                batch_max_inputs: batch.max_entries,
                batch_max_estimated_tokens: batch.max_batch_tokens,
                indexed_inputs: engine.indexer().stats().inputs_embedded,
                elapsed_seconds: started.elapsed().as_secs_f64(),
                os: std::env::consts::OS,
                architecture: std::env::consts::ARCH,
                postgres: server.server_version,
                pgvector: server.vector.and_then(|v| v.installed_version),
                cache: "fresh scratch database, empty vector and lexical caches at indexing start",
            })
        }
        _ => None,
    };
    Ok((report, evidence))
}

fn sourced<T>(value: T) -> Sourced<T> {
    Sourced {
        value,
        origin: Origin::Workspace,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::HeaderMap, routing::post};
    use knowell_eval::{FixtureSpec, QuerySet, Scale, WriteOptions, generate, walk_fixture};
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_measurement_indexes_only_synthetic_content_and_records_conditions() {
        if std::env::var_os("KNOWELL_TEST_DATABASE_URL").is_none() {
            eprintln!("skipping live mock integration: KNOWELL_TEST_DATABASE_URL is not set");
            return;
        }
        let count = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&count);
        let estimated = Arc::new(AtomicU64::new(0));
        let sent_estimates = Arc::clone(&estimated);
        let app = Router::new().route("/v1beta/models/gemini-embedding-2:batchEmbedContents", post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let calls = Arc::clone(&calls);
            let sent_estimates = Arc::clone(&sent_estimates);
            async move {
                assert!(headers.get("x-goog-api-key").is_some_and(|v| v == "KNOWELL_CANARY_fake_live_key"));
                calls.fetch_add(1, Ordering::SeqCst);
                let requests = body["requests"].as_array().unwrap();
                assert!(requests.len() <= 40);
                let estimated_tokens: u64 = requests.iter().map(|request| {
                    knowell_embed::estimate_tokens(request["content"]["parts"][0]["text"].as_str().unwrap())
                }).sum();
                assert!(estimated_tokens <= 8_192);
                sent_estimates.fetch_add(estimated_tokens, Ordering::SeqCst);
                let embeddings: Vec<Value> = requests.iter().map(|request| {
                    assert_eq!(request["model"], "models/gemini-embedding-2");
                    assert_eq!(request["outputDimensionality"], 768);
                    let text = request["content"]["parts"][0]["text"].as_str().unwrap();
                    // Fixture secrets are excluded/redacted before the provider boundary.
                    assert!(!text.contains("KNOWELL_CANARY_") && !text.contains("AKIA"));
                    json!({"values": vec![1.0_f32; 768]})
                }).collect();
                Json(json!({"embeddings": embeddings, "usageMetadata": {"promptTokenCount": requests.len() * 7}}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = url::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let result = tokio::task::spawn_blocking(move || {
            let fixture = generate(&FixtureSpec {
                seed: 42,
                scale: Scale::Small,
            });
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("fixture");
            fixture
                .write_to(&root, &WriteOptions { git: true })
                .unwrap();
            let corpus = walk_fixture(&root, &fixture).unwrap().corpus;
            let queries = QuerySet::builtin().unwrap();
            let key_file = dir.path().join("fake-key");
            std::fs::write(&key_file, "KNOWELL_CANARY_fake_live_key").unwrap();
            let reference = format!("file:{}", key_file.display());
            let mut options = LiveOptions::new(&reference, 768, 500_000).unwrap();
            options.base_url = Some(base);
            let result = measure(
                &root,
                &fixture,
                "env:KNOWELL_TEST_DATABASE_URL",
                Some(&options),
                |hybrid| Ok(knowell_eval::run(&corpus, &queries, &[hybrid], 10)?),
            )
            .unwrap();
            let calls_before_budget_test = count.load(Ordering::SeqCst);
            options.max_tokens = 1;
            let refused = measure(
                &root,
                &fixture,
                "env:KNOWELL_TEST_DATABASE_URL",
                Some(&options),
                |hybrid| Ok(knowell_eval::run(&corpus, &queries, &[hybrid], 10)?),
            );
            assert!(refused.is_err());
            assert_eq!(count.load(Ordering::SeqCst), calls_before_budget_test);
            result
        })
        .await
        .unwrap();
        server.abort();
        assert!(result.0.retriever("hybrid").is_some());
        let evidence = result.1.unwrap();
        assert_eq!(evidence.dimensions, 768);
        assert!(evidence.all_returned_dimensions_validated);
        assert!(evidence.accounted_input_tokens > 0 && evidence.indexed_inputs > 0);
        assert_eq!(evidence.provider_retries, 0);
        eprintln!(
            "synthetic mock indexed {} inputs; sent {} estimated tokens",
            evidence.indexed_inputs,
            estimated.load(Ordering::SeqCst)
        );
        assert!(
            !serde_json::to_string(&evidence)
                .unwrap()
                .contains("KNOWELL_CANARY")
        );
    }
}
