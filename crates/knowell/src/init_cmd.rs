//! `know init`: engine configuration and database setup. Idempotent.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Args, ValueEnum};
use knowell_config::{DatabaseMode, EngineConfig};
use knowell_core::SecretRef;
use knowell_pg_managed::ManagedPostgres;
use knowell_store::{Store, StoreOptions};
use secrecy::SecretString;

use crate::db::{self, DATABASE_NAME};
use crate::engine_file;
use crate::env::Env;
use crate::output::Output;

#[derive(Debug, Args)]
pub(crate) struct InitArgs {
    /// Where PostgreSQL comes from [default: the mode of an existing engine
    /// configuration, else `managed`].
    #[arg(long, value_enum)]
    database: Option<DatabaseArg>,
    /// Reference to the connection URL for `external`: `env:NAME` or
    /// `file:/path`. Never the URL itself.
    #[arg(long, value_name = "REF")]
    database_url_ref: Option<String>,
    /// Directory of a pgvector bundle for the managed PostgreSQL
    /// [default: $KNOWELL_HOME/pgvector/pg<major>, when it exists].
    #[arg(long, value_name = "DIR")]
    pgvector_bundle: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum DatabaseArg {
    /// Knowell installs and runs its own loopback-only PostgreSQL.
    Managed,
    /// A PostgreSQL 17/18 server with pgvector that you operate.
    External,
    /// A team hub with PostgreSQL in Docker Compose (prints the steps).
    Compose,
}

pub(crate) fn run(args: InitArgs, env: &Env, out: &mut Output) -> anyhow::Result<ExitCode> {
    let requested = match args.database {
        Some(DatabaseArg::Compose) => return compose_instructions(out),
        Some(DatabaseArg::Managed) => Some(DatabaseMode::Managed),
        Some(DatabaseArg::External) => Some(DatabaseMode::External),
        None => None,
    };
    // SecretRefError never echoes the rejected text, which may be a URL with
    // a password pasted by mistake.
    let url_ref = args
        .database_url_ref
        .as_deref()
        .map(SecretRef::from_str)
        .transpose()
        .map_err(|e| anyhow::anyhow!("--database-url-ref: {e}"))?;

    let cfg = engine_config(env, requested, url_ref, out)?;
    let rt = db::runtime()?;
    let code = rt.block_on(async {
        match cfg.database.mode {
            DatabaseMode::Managed => {
                init_managed(env, &cfg, args.pgvector_bundle.as_deref(), out).await
            }
            DatabaseMode::External => init_external(&cfg, out).await,
        }
    })?;
    out.flush()?;
    Ok(code)
}

/// Creates the engine configuration, or checks that an existing one matches
/// the request: it is never changed behind the user's back.
fn engine_config(
    env: &Env,
    requested: Option<DatabaseMode>,
    url_ref: Option<SecretRef>,
    out: &mut Output,
) -> anyhow::Result<EngineConfig> {
    let path = &env.engine_config;
    if let Some(cfg) = env.load_engine().map_err(anyhow::Error::new)? {
        let mode = cfg.database.mode;
        if requested.is_some_and(|r| r != mode) {
            bail!(
                "{} already uses database mode `{}`; edit or remove it to change the mode",
                path.display(),
                mode_name(mode)
            );
        }
        if url_ref.is_some() && (mode != DatabaseMode::External || cfg.database.url != url_ref) {
            bail!(
                "{} already has its database settings; edit it to change the url reference",
                path.display()
            );
        }
        out.line(format!(
            "engine config: {} (exists, role {}, listen {}, database {})",
            path.display(),
            cfg.server.role.as_str(),
            cfg.server.listen,
            mode_name(mode)
        ))?;
        return Ok(cfg);
    }
    let mode = requested.unwrap_or(DatabaseMode::Managed);
    match (mode, &url_ref) {
        (DatabaseMode::External, None) => bail!(
            "--database external needs --database-url-ref, e.g. env:KNOWELL_DATABASE_URL (a reference, never the URL itself)"
        ),
        (DatabaseMode::Managed, Some(_)) => {
            bail!("--database-url-ref only applies to --database external")
        }
        _ => {}
    }
    let mut cfg = knowell_config::parse_engine("version = 1")?;
    cfg.database.mode = mode;
    cfg.database.url = url_ref;
    engine_file::write(path, &cfg)?;
    out.line(format!(
        "engine config: created {} (role {}, listen {}, database {})",
        path.display(),
        cfg.server.role.as_str(),
        cfg.server.listen,
        mode_name(mode)
    ))?;
    Ok(cfg)
}

fn mode_name(mode: DatabaseMode) -> &'static str {
    match mode {
        DatabaseMode::Managed => "managed",
        DatabaseMode::External => "external",
    }
}

async fn init_managed(
    env: &Env,
    cfg: &EngineConfig,
    bundle: Option<&Path>,
    out: &mut Output,
) -> anyhow::Result<ExitCode> {
    let pg = db::managed(env, cfg)?;
    out.line(format!(
        "postgresql: preparing PostgreSQL {} under {} (the first run downloads it)",
        pg.major(),
        env.home.join("pg").display()
    ))?;
    out.flush()?;
    let version = pg
        .install()
        .await
        .context("cannot install the managed PostgreSQL")?;
    out.line(format!("postgresql: {version} installed"))?;
    let created = pg
        .init_data_dir()
        .await
        .context("cannot initialise the managed PostgreSQL data directory")?;
    out.line(if created {
        "postgresql: data directory initialised (loopback only, random password)"
    } else {
        "postgresql: data directory already initialised"
    })?;
    let port = db::ensure_running(&pg).await?;
    out.line(format!("postgresql: running on 127.0.0.1:{port}"))?;
    let created = pg
        .ensure_database(DATABASE_NAME)
        .await
        .context("cannot create the knowell database")?;
    out.line(format!(
        "database `{DATABASE_NAME}`: {}",
        if created { "created" } else { "exists" }
    ))?;

    let vector = ensure_pgvector(env, &pg, bundle, out).await?;
    let Some(vector) = vector else {
        out.line("pgvector: NOT AVAILABLE in the managed PostgreSQL")?;
        out.line(
            "  Knowell's storage schema and semantic search need the pgvector extension, so the schema was not created.",
        )?;
        out.line(format!(
            "  Get the pgvector bundle for PostgreSQL {} and this platform from the Knowell release, unpack it to {}",
            pg.major(),
            default_bundle_dir(env, pg.major()).display()
        ))?;
        out.line("  (or pass --pgvector-bundle DIR), then run `know init` again.")?;
        out.flush()?;
        tracing::error!("init incomplete: pgvector is missing");
        return Ok(ExitCode::FAILURE);
    };
    out.line(format!("pgvector: {vector} available"))?;

    let url = pg
        .connection_url(DATABASE_NAME)
        .context("cannot build the managed database connection")?;
    migrate(&url, out).await?;
    next_steps(cfg, out)?;
    Ok(ExitCode::SUCCESS)
}

/// The pgvector version available to the managed server, installing the
/// bundle first when the extension is missing and a bundle is at hand.
async fn ensure_pgvector(
    env: &Env,
    pg: &ManagedPostgres,
    bundle: Option<&Path>,
    out: &mut Output,
) -> anyhow::Result<Option<String>> {
    let available = pg
        .extension_available("vector")
        .await
        .context("cannot query the available extensions")?;
    if available.is_some() {
        return Ok(available);
    }
    let dir = match bundle {
        Some(dir) => dir.to_path_buf(),
        None => {
            let default = default_bundle_dir(env, pg.major());
            if !default.is_dir() {
                return Ok(None);
            }
            default
        }
    };
    let flat = flatten_bundle(&dir)?;
    let installed = pg
        .install_extension_bundle(flat.as_ref().map_or(dir.as_path(), |t| t.path()))
        .await
        .with_context(|| format!("cannot install the pgvector bundle from {}", dir.display()))?;
    out.line(format!(
        "pgvector: {installed} installed from {}",
        dir.display()
    ))?;
    pg.extension_available("vector")
        .await
        .context("cannot query the available extensions")
}

fn default_bundle_dir(env: &Env, major: u32) -> PathBuf {
    env.home.join("pgvector").join(format!("pg{major}"))
}

/// The managed installer expects a flat directory; release bundles use
/// `lib/` and `share/extension/`. Returns a flattened copy for the latter.
fn flatten_bundle(dir: &Path) -> anyhow::Result<Option<tempfile::TempDir>> {
    if dir.join("vector.control").is_file() {
        return Ok(None);
    }
    let ext = dir.join("share").join("extension");
    let lib = dir.join("lib");
    if !ext.join("vector.control").is_file() || !lib.is_dir() {
        bail!(
            "{} is not a pgvector bundle (expected vector.control at its top or in share/extension/)",
            dir.display()
        );
    }
    let flat = tempfile::tempdir().context("cannot create a temporary directory")?;
    let copy_matching = |from: &Path, keep: &dyn Fn(&str) -> bool| -> anyhow::Result<()> {
        let entries =
            std::fs::read_dir(from).with_context(|| format!("cannot read {}", from.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("cannot read {}", from.display()))?;
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            let file_type = entry
                .file_type()
                .with_context(|| format!("cannot read {}", entry.path().display()))?;
            if file_type.is_file() && keep(name_str) {
                std::fs::copy(entry.path(), flat.path().join(&name))
                    .with_context(|| format!("cannot copy {}", entry.path().display()))?;
            }
        }
        Ok(())
    };
    copy_matching(&ext, &|n| {
        n == "vector.control" || (n.starts_with("vector--") && n.ends_with(".sql"))
    })?;
    copy_matching(&lib, &|n| {
        matches!(n, "vector.so" | "vector.dylib" | "vector.dll")
    })?;
    Ok(Some(flat))
}

async fn init_external(cfg: &EngineConfig, out: &mut Output) -> anyhow::Result<ExitCode> {
    let Some(reference) = &cfg.database.url else {
        bail!("`database.url` is missing for mode `external`");
    };
    let url = match knowell_secrets::resolve(reference) {
        Ok(url) => url,
        Err(err) => {
            // Names the reference, never a value.
            out.line(format!("database: {err}"))?;
            out.line(format!(
                "  set {} to the connection URL and run `know init` again",
                reference.describe()
            ))?;
            out.flush()?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let store = connect(&url).await?;
    let info = store
        .check_server()
        .await
        .context("cannot check the database server")?;
    let issues = info.issues();
    if !issues.is_empty() {
        for issue in &issues {
            out.line(format!("database: {issue}"))?;
        }
        out.flush()?;
        store.close().await;
        tracing::error!("init incomplete: the database server is not supported");
        return Ok(ExitCode::FAILURE);
    }
    store.close().await;
    migrate(&url, out).await?;
    next_steps(cfg, out)?;
    Ok(ExitCode::SUCCESS)
}

async fn connect(url: &SecretString) -> anyhow::Result<Store> {
    let options = StoreOptions {
        max_connections: 2,
        acquire_timeout: Duration::from_secs(15),
        application_name: "know init".to_owned(),
        ..StoreOptions::default()
    };
    Store::connect(url, &options)
        .await
        .context("cannot open the database")
}

async fn migrate(url: &SecretString, out: &mut Output) -> anyhow::Result<()> {
    let store = connect(url).await?;
    let info = store
        .check_server()
        .await
        .context("cannot check the database server")?;
    store
        .migrate()
        .await
        .context("cannot apply the database migrations")?;
    let vector = info.vector.as_ref().map(|v| {
        v.installed_version
            .as_deref()
            .unwrap_or(&v.default_version)
            .to_owned()
    });
    store.close().await;
    out.line(format!(
        "database: postgresql {}, pgvector {}; schema up to date",
        info.server_version,
        vector.as_deref().unwrap_or("missing")
    ))?;
    Ok(())
}

fn next_steps(cfg: &EngineConfig, out: &mut Output) -> anyhow::Result<()> {
    out.line("")?;
    out.line("next steps:")?;
    out.line("  know workspace import [DIR]       propose knowell.toml from your repositories")?;
    out.line("  know workspace add                register the workspace with the engine")?;
    out.line("  know connect claude|codex|cursor  connect your coding agent over MCP")?;
    out.line(format!(
        "  know serve                        panel at http://{}/",
        cfg.server.listen
    ))?;
    out.line("  know doctor                       check the installation")?;
    Ok(())
}

fn compose_instructions(out: &mut Output) -> anyhow::Result<ExitCode> {
    for line in [
        "Docker Compose runs a Knowell hub next to PostgreSQL with pgvector (see deploy/README.md):",
        "",
        "  cd deploy",
        "  cp .env.example .env",
        "  printf '%s' \"$(openssl rand -hex 24)\" > secrets/postgres_password",
        "  printf 'postgres://knowell:%s@db:5432/knowell' \"$(cat secrets/postgres_password)\" > secrets/database_url",
        "  chmod 600 secrets/*",
        "  # the URL users open, e.g. https://knowell.example.com behind a TLS proxy:",
        "  echo 'KNOWELL_PUBLIC_URL=http://localhost:7420' >> .env",
        "  docker compose up -d --build",
        "",
        "Then connect developer machines to the hub:",
        "",
        "  know login <hub-url> --token-ref env:KNOWELL_HUB_TOKEN",
        "",
        "Nothing was written on this machine.",
    ] {
        out.line(line)?;
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_bundles_are_used_as_they_are() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("vector.control"),
            "default_version = '0.8.7'",
        )
        .unwrap();
        assert!(flatten_bundle(dir.path()).unwrap().is_none());
    }

    #[test]
    fn release_bundles_are_flattened() {
        let dir = tempfile::tempdir().unwrap();
        let ext = dir.path().join("share").join("extension");
        let lib = dir.path().join("lib");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(ext.join("vector.control"), "default_version = '0.8.7'").unwrap();
        std::fs::write(ext.join("vector--0.8.7.sql"), "-- sql").unwrap();
        std::fs::write(ext.join("other.control"), "").unwrap();
        std::fs::write(lib.join("vector.so"), "elf").unwrap();
        std::fs::write(dir.path().join("manifest.json"), "{}").unwrap();
        let flat = flatten_bundle(dir.path()).unwrap().unwrap();
        let mut names: Vec<String> = std::fs::read_dir(flat.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["vector--0.8.7.sql", "vector.control", "vector.so"]);
    }

    #[test]
    fn non_bundles_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(flatten_bundle(dir.path()).is_err());
    }
}
