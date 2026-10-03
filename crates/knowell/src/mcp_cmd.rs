//! `know mcp`: MCP over stdin/stdout for agent clients.
//!
//! stdout is the protocol channel: nothing else may be written to it. Logs
//! go to stderr (see `init_tracing`).

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use clap::Args;

use crate::db;
use crate::env::Env;
use crate::registry::Registry;
use crate::tools::{self, EngineDeps};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Args)]
pub(crate) struct McpArgs {}

pub(crate) fn run(_args: McpArgs, env: &Env) -> anyhow::Result<ExitCode> {
    // A missing engine configuration is fine while the engine is not wired:
    // the tools answer `not_ready` either way. An invalid one is reported.
    let engine = match env.load_engine() {
        Ok(Some(cfg)) => cfg,
        Ok(None) => {
            tracing::warn!(
                "no engine configuration at {}; run `know init` (serving with built-in defaults)",
                env.engine_config.display()
            );
            knowell_config::parse_engine("version = 1")?
        }
        Err(err) => return Err(anyhow::Error::new(err)),
    };
    let organization = knowell_core::Name::new("local")?;
    let workspace_files = Registry::load(&env.home)
        .map(|r| r.files_of(&organization))
        .unwrap_or_else(|err| {
            tracing::warn!("{err:#}");
            Default::default()
        });
    let rt = db::runtime()?;
    rt.block_on(async move {
        // Without a reachable database the tools answer `not_ready` and say why.
        let store = match crate::serve_cmd::open_store(env, &engine).await {
            Ok(store) => Some(store),
            Err(err) => {
                if err
                    .downcast_ref::<knowell_store::StoreError>()
                    .is_some_and(knowell_store::StoreError::requires_maintenance)
                {
                    return Err(err);
                }
                tracing::warn!("{err:#}");
                None
            }
        };
        let deps = EngineDeps {
            home: env.home.clone(),
            engine,
            store,
            workspace_files,
            organization,
            local_user: Some(knowell_auth::UserId::new(uuid::Uuid::from_u128(
                crate::serve_cmd::LOCAL_USER,
            ))),
        };
        let indexing = CancellationToken::new();
        let built = tools::build_engine(&deps).await?;
        let engine_ref = built.as_ref().map(|(engine, _)| engine);
        // Agents may run `know mcp` without a `know serve` running, so the
        // session keeps its workspaces current itself. Jobs are idempotent
        // and claimed with SKIP LOCKED, so several sessions share the work.
        let background = built.as_ref().map(|(engine, workspaces)| {
            tools::start_indexing(engine, workspaces.clone(), &indexing)
        });
        let result = tokio::select! {
            result = knowell_mcp::serve_stdio(Arc::new(tools::Tools::new(engine_ref))) => {
                result.context("the MCP stdio session failed")
            }
            () = env.parent_shutdown.cancelled() => Ok(()),
        };
        indexing.cancel();
        if let Some(background) = background {
            background.finish().await?;
        }
        // Buffered tool usage reaches the database before the pool closes.
        if let Some((engine, _)) = &built
            && let Err(error) = engine.flush_usage().await
        {
            tracing::warn!(%error, "tool usage of this session could not be stored");
        }
        if let Some(store) = deps.store {
            store.close().await;
        }
        result
    })?;
    Ok(ExitCode::SUCCESS)
}
