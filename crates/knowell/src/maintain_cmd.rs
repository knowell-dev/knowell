//! Explicit database maintenance, including manager-owned installation recovery.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::Args;
use uuid::Uuid;

use crate::{db, env::Env, output::Output};

#[derive(Debug, Args)]
pub(crate) struct MaintainArgs {
    /// Inspect the persisted operation UUID and schema without changing them.
    #[arg(long, conflicts_with = "operation")]
    status: bool,
    /// Exact persisted operation UUID, or a new non-nil UUID for explicit maintenance.
    #[arg(long, required_unless_present = "status")]
    operation: Option<String>,
    /// Apply this binary's embedded migrations; recovery otherwise validates only.
    #[arg(long, requires = "operation")]
    migrate: bool,
    /// Fresh managed-database backup destination required before migrations.
    #[arg(long, requires = "migrate")]
    backup: Option<PathBuf>,
    /// Operator attestation that an external database backup and restore test completed.
    #[arg(long, requires = "migrate", conflicts_with = "backup")]
    external_backup_confirmed: bool,
    /// Attest that external connections preserve sessions and every data client participates in admission or is stopped.
    #[arg(long)]
    session_gates_confirmed: bool,
    /// Maximum wait for participating runtime connections, in seconds.
    #[arg(long, default_value = "15", value_parser = clap::value_parser!(u64).range(1..=600))]
    timeout: u64,
}

pub(crate) fn run(args: MaintainArgs, env: &Env, out: &mut Output) -> anyhow::Result<ExitCode> {
    let operation = args.operation.as_deref().map(parse_operation).transpose()?;
    let cfg = env.require_engine()?;
    if !args.status {
        db::validate_session_gates(&cfg, args.session_gates_confirmed)?;
    }
    if args.migrate {
        db::validate_migration_backup(
            &cfg,
            args.backup.as_deref(),
            args.external_backup_confirmed,
        )?;
    }
    let install = knowell_update::install::Install::for_executable(&std::env::current_exe()?)?;
    let owner = if args.status {
        None
    } else {
        install
            .as_ref()
            .map(|install| install.update_lease())
            .transpose()?
    };
    let _exclusive = install
        .as_ref()
        .zip(owner.as_ref())
        .map(|(install, owner)| install.exclusive_runtime(owner))
        .transpose()?;
    if !args.status
        && let Some(install) = &install
        && install.transaction()?.is_some()
    {
        bail!(
            "a software update is pending; recover that update before separate database maintenance"
        );
    }
    db::runtime()?.block_on(async {
        let admin = db::connect_admin(env, &cfg, Duration::from_secs(args.timeout), 1).await?;
        if args.status {
            match admin.maintenance_owner().await? {
                Some(owner) => out.line(format!("pending maintenance operation: {owner}"))?,
                None => out.line("no database maintenance operation is pending")?,
            }
            out.line(format!(
                "schema: {}; supported: {}",
                admin.inspect_schema().await?.version,
                knowell_store::Store::latest_schema().version
            ))?;
        } else {
            let operation = operation.context("maintenance operation is required")?;
            let mut maintenance = admin.begin_maintenance(operation).await?;
            maintenance
                .acquire_exclusive(Duration::from_secs(args.timeout))
                .await?;
            if args.migrate {
                db::migration_backup(
                    env,
                    &cfg,
                    args.backup.as_deref(),
                    args.external_backup_confirmed,
                )
                .await?;
                maintenance.migrate().await?;
            }
            maintenance.validate_schema().await?;
            maintenance.finish().await?;
            out.line(format!(
                "maintenance {operation} finished; runtime admission is open"
            ))?;
        }
        admin.close().await;
        anyhow::Ok(())
    })?;
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

fn parse_operation(value: &str) -> anyhow::Result<Uuid> {
    let operation = Uuid::parse_str(value)
        .map_err(|_| anyhow::anyhow!("maintenance operation must be a non-nil uuid"))?;
    if operation.is_nil() {
        bail!("maintenance operation must be a non-nil uuid")
    }
    Ok(operation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_ids_are_validated_before_configuration_without_echoing_input() {
        for value in [
            "",
            "00000000-0000-0000-0000-000000000000",
            "KNOWELL_CANARY_FAKE_TOKEN",
            "12345678-1234-1234-1234-",
        ] {
            let message = parse_operation(value).unwrap_err().to_string();
            assert_eq!(message, "maintenance operation must be a non-nil uuid");
        }
        assert!(parse_operation("12345678-1234-1234-1234-123456789abc").is_ok());
    }
}
