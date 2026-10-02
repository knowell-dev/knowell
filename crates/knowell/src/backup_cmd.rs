//! `know backup` / `know restore` for the managed PostgreSQL. For an external
//! server the equivalent `pg_dump` / `pg_restore` commands are printed:
//! Knowell does not run client tools against servers it does not manage.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::Args;
use knowell_config::{DatabaseMode, EngineConfig};
use knowell_core::SecretRef;

use crate::db::{self, DATABASE_NAME};
use crate::env::{self, Env};
use crate::output::Output;

#[derive(Debug, Args)]
pub(crate) struct BackupArgs {
    /// Backup file to write (PostgreSQL custom format).
    file: PathBuf,
    /// Database to back up.
    #[arg(long, default_value = DATABASE_NAME)]
    database: String,
}

#[derive(Debug, Args)]
pub(crate) struct RestoreArgs {
    /// Backup file written by `know backup`.
    file: PathBuf,
    /// New database to restore into; it must not exist yet.
    #[arg(long, default_value = DATABASE_NAME)]
    into: String,
}

pub(crate) fn backup(args: BackupArgs, env: &Env, out: &mut Output) -> anyhow::Result<ExitCode> {
    let cfg = env.require_engine()?;
    let file = env::absolute(&args.file)?;
    if cfg.database.mode == DatabaseMode::External {
        return external_guidance(&cfg, out, |url| {
            format!(
                "pg_dump --format=custom --file {} {url}",
                quote(&file.display().to_string())
            )
        });
    }
    let pg = db::managed(env, &cfg)?;
    db::runtime()?.block_on(async {
        db::ensure_running(&pg).await?;
        pg.backup(&args.database, &file)
            .await
            .with_context(|| format!("cannot back up database `{}`", args.database))
    })?;
    out.line(format!(
        "backed up database `{}` to {}",
        args.database,
        file.display()
    ))?;
    out.line("the file contains indexed source code and memory: store it like the code itself")?;
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

pub(crate) fn restore(args: RestoreArgs, env: &Env, out: &mut Output) -> anyhow::Result<ExitCode> {
    let cfg = env.require_engine()?;
    let file = env::absolute(&args.file)?;
    if cfg.database.mode == DatabaseMode::External {
        return external_guidance(&cfg, out, |url| {
            format!(
                "createdb {}  # with the same connection settings\npg_restore --exit-on-error --no-owner --dbname {url} {}",
                args.into,
                quote(&file.display().to_string())
            )
        });
    }
    let pg = db::managed(env, &cfg)?;
    let result = db::runtime()?.block_on(async {
        db::ensure_running(&pg).await?;
        anyhow::Ok(pg.restore(&file, &args.into).await)
    })?;
    match result {
        Ok(()) => {}
        Err(knowell_pg_managed::Error::DatabaseExists(name)) => {
            tracing::error!(
                "database `{name}` already exists; restore into a new one with --into <name>, nothing was changed"
            );
            return Ok(ExitCode::FAILURE);
        }
        Err(err) => {
            return Err(anyhow::Error::new(err))
                .with_context(|| format!("cannot restore {}", file.display()));
        }
    }
    out.line(format!(
        "restored {} into database `{}`",
        file.display(),
        args.into
    ))?;
    if args.into != DATABASE_NAME {
        out.line(format!(
            "Knowell uses database `{DATABASE_NAME}`; restore into it (after removing the old one) to serve this data"
        ))?;
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

/// Prints the client-tool command for an external database. Exits 1:
/// nothing was backed up or restored by Knowell.
fn external_guidance(
    cfg: &EngineConfig,
    out: &mut Output,
    command: impl Fn(&str) -> String,
) -> anyhow::Result<ExitCode> {
    // The URL is referenced through the shell, never expanded here.
    let url = match &cfg.database.url {
        Some(SecretRef::Env(name)) => format!("\"${name}\""),
        Some(SecretRef::File(path)) => format!("\"$(cat {})\"", quote(&path.display().to_string())),
        None => "<connection url>".to_owned(),
    };
    out.line("this engine uses an external PostgreSQL; back it up and restore it with the PostgreSQL client tools:")?;
    out.line("")?;
    for line in command(&url).lines() {
        out.line(format!("  {line}"))?;
    }
    out.line("")?;
    out.line("nothing was written by Knowell")?;
    out.flush()?;
    Ok(ExitCode::FAILURE)
}

/// Single-quotes text for a POSIX shell when it needs quoting.
fn quote(text: &str) -> String {
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-:\\".contains(c))
    {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote("/a/b.dump"), "/a/b.dump");
        assert_eq!(quote("my file"), "'my file'");
        assert_eq!(quote("it's"), "'it'\\''s'");
    }
}
