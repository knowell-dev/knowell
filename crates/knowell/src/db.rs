//! Opening the database described by the engine configuration.
//!
//! Connection URLs are secrets: they are resolved from their reference at the
//! moment of use, held as `SecretString`, and never printed. `StoreError`
//! messages are scrubbed by `knowell-store`.

use std::time::Duration;

use anyhow::{Context, bail};
use knowell_config::{DatabaseMode, EngineConfig};
use knowell_pg_managed::{ManagedConfig, ManagedPostgres, Status};
use knowell_store::{Store, StoreOptions};
use secrecy::SecretString;

use crate::env::Env;

/// Name of the database Knowell uses on its PostgreSQL server.
pub(crate) const DATABASE_NAME: &str = "knowell";

/// Handle to the managed PostgreSQL under `$KNOWELL_HOME/pg`.
pub(crate) fn managed(env: &Env, cfg: &EngineConfig) -> anyhow::Result<ManagedPostgres> {
    if cfg.database.data_dir.is_some() {
        // The managed layout is fixed under $KNOWELL_HOME/pg; silently using
        // another directory than the configured one would hide data.
        bail!(
            "`database.data_dir` is not supported by the managed mode yet; remove it from {} (data lives under {})",
            env.engine_config.display(),
            env.home.join("pg").display()
        );
    }
    ManagedPostgres::new(ManagedConfig::new(&env.home))
        .context("cannot set up the managed PostgreSQL handle")
}

/// The connection URL of the configured database, as a secret.
pub(crate) async fn connection_url(env: &Env, cfg: &EngineConfig) -> anyhow::Result<SecretString> {
    match cfg.database.mode {
        DatabaseMode::External => {
            let Some(reference) = &cfg.database.url else {
                bail!("`database.url` is missing for mode `external`");
            };
            // The error names the reference, never a value.
            knowell_secrets::resolve(reference).context("cannot resolve the database url reference")
        }
        DatabaseMode::Managed => {
            let pg = managed(env, cfg)?;
            ensure_running(&pg).await?;
            pg.connection_url(DATABASE_NAME)
                .context("cannot build the managed database connection")
        }
    }
}

/// Makes sure the managed server runs, starting it when it is stopped. It
/// keeps running after the command ends, like a daemon.
pub(crate) async fn ensure_running(pg: &ManagedPostgres) -> anyhow::Result<u16> {
    let status = pg
        .status()
        .await
        .context("cannot read the managed PostgreSQL status")?;
    match status {
        Status::Running { port, .. } => Ok(port),
        Status::NotInstalled | Status::Installed => {
            bail!("the managed PostgreSQL is not set up; run `know init`")
        }
        Status::Stopped => {
            tracing::info!("starting the managed PostgreSQL");
            pg.start()
                .await
                .context("cannot start the managed PostgreSQL")
        }
        Status::StalePostmasterPid { pid } => {
            tracing::warn!(
                pid,
                "the managed PostgreSQL did not shut down cleanly; removing its stale pid file"
            );
            pg.clear_stale_postmaster_pid()
                .await
                .context("cannot clear the stale pid file")?;
            pg.start()
                .await
                .context("cannot start the managed PostgreSQL")
        }
    }
}

/// Connects to the configured database.
pub(crate) async fn connect(
    env: &Env,
    cfg: &EngineConfig,
    timeout: Duration,
    max_connections: u32,
) -> anyhow::Result<Store> {
    let url = connection_url(env, cfg).await?;
    let options = StoreOptions {
        max_connections,
        acquire_timeout: timeout,
        application_name: format!("know {}", env!("CARGO_PKG_VERSION")),
        ..StoreOptions::default()
    };
    Store::connect(&url, &options)
        .await
        .context("cannot open the database")
}

/// A small runtime for the short CLI commands (`serve` builds its own).
pub(crate) fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("cannot start the async runtime")
}
