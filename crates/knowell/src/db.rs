//! Opening the database described by the engine configuration.
//!
//! Connection URLs are secrets: they are resolved from their reference at the
//! moment of use, held as `SecretString`, and never printed. `StoreError`
//! messages are scrubbed by `knowell-store`.
//! Windows backup staging requires a successful ACL helper before any dump is
//! created. The helper has a bounded execution budget and is killed and reaped
//! on timeout; permission failures never permit backup creation.

use std::time::Duration;

use anyhow::{Context, bail};
use knowell_config::{DatabaseMode, EngineConfig};
use knowell_pg_managed::{ManagedConfig, ManagedPostgres, Status};
use knowell_store::{Store, StoreOptions};
use secrecy::SecretString;

use crate::env::Env;

/// Name of the database Knowell uses on its PostgreSQL server.
pub(crate) const DATABASE_NAME: &str = "knowell";

// PowerShell startup and ancestor ACL lookup can exceed 15 seconds when the
// host is busy. The budget is bounded without relaxing the access checks.
#[cfg(windows)]
const BACKUP_ACL_TIMEOUT: Duration = Duration::from_secs(60);

#[cfg(windows)]
const BACKUP_ACL_CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);

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
    Store::connect_runtime(&url, &options)
        .await
        .context("cannot open the database")
}

/// Administrative connection for explicit bootstrap/migration commands only.
pub(crate) async fn connect_admin(
    env: &Env,
    cfg: &EngineConfig,
    timeout: Duration,
    max_connections: u32,
) -> anyhow::Result<Store> {
    let url = connection_url(env, cfg).await?;
    let options = StoreOptions {
        max_connections,
        acquire_timeout: timeout,
        application_name: format!("know maintenance {}", env!("CARGO_PKG_VERSION")),
        ..StoreOptions::default()
    };
    Store::connect(&url, &options)
        .await
        .context("cannot open the maintenance database")
}

/// Creates a managed backup or requires explicit external backup/restore attestation.
/// The caller holds the database-wide exclusive gate before invoking this function.
pub(crate) async fn migration_backup(
    env: &Env,
    cfg: &EngineConfig,
    destination: Option<&std::path::Path>,
    external_confirmed: bool,
) -> anyhow::Result<()> {
    validate_migration_backup(cfg, destination, external_confirmed)?;
    match cfg.database.mode {
        DatabaseMode::Managed => {
            let destination =
                destination.context("managed migration requires a fresh backup destination")?;
            let destination = std::path::absolute(destination)?;
            let parent = destination
                .parent()
                .context("backup destination has no parent")?;
            std::fs::create_dir_all(parent)?;
            let parent = parent.canonicalize()?;
            let destination = parent.join(
                destination
                    .file_name()
                    .context("backup destination has no file name")?,
            );
            let staging = tempfile::Builder::new()
                .prefix(".knowell-backup-")
                .tempdir_in(&parent)?;
            restrict_backup_staging(staging.path()).await?;
            let dump = staging.path().join("database.dump");
            managed(env, cfg)?
                .backup(DATABASE_NAME, &dump)
                .await
                .context("pre-migration backup failed; maintenance remains closed")?;
            publish_backup(&dump, &destination)?;
        }
        DatabaseMode::External if external_confirmed => {}
        DatabaseMode::External => {
            bail!("external migration requires explicit backup and restore-test attestation")
        }
    }
    Ok(())
}

async fn restrict_backup_staging(directory: &std::path::Path) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        // Protect the empty directory before pg_dump creates any private bytes.
        // The SID comes from the process identity, not an ambient username.
        const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
try {
    $path = [Environment]::GetEnvironmentVariable('KNOWELL_BACKUP_STAGING')
    $owner = [Security.Principal.WindowsIdentity]::GetCurrent().User
    $trusted = @($owner.Value, 'S-1-5-18', 'S-1-5-32-544')
    $dangerous = [Security.AccessControl.FileSystemRights]::Delete -bor [Security.AccessControl.FileSystemRights]::DeleteSubdirectoriesAndFiles -bor [Security.AccessControl.FileSystemRights]::ChangePermissions -bor [Security.AccessControl.FileSystemRights]::TakeOwnership
    $cursor = [IO.Directory]::GetParent($path)
    while ($null -ne $cursor) {
        $parentAcl = [IO.Directory]::GetAccessControl($cursor.FullName)
        if ($null -ne $cursor.Parent -and $trusted -notcontains $parentAcl.GetOwner([Security.Principal.SecurityIdentifier]).Value) { exit 1 }
        foreach ($entry in $parentAcl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
            if (($entry.PropagationFlags -band [Security.AccessControl.PropagationFlags]::InheritOnly) -ne 0) { continue }
            if ($entry.AccessControlType -eq 'Allow' -and ($entry.FileSystemRights -band $dangerous) -ne 0 -and $trusted -notcontains $entry.IdentityReference.Value) { exit 1 }
        }
        $cursor = $cursor.Parent
    }
    $acl = [Security.AccessControl.DirectorySecurity]::new()
    $acl.SetOwner($owner)
    $acl.SetAccessRuleProtection($true, $false)
    $rule = [Security.AccessControl.FileSystemAccessRule]::new($owner, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
    $acl.AddAccessRule($rule)
    [IO.Directory]::SetAccessControl($path, $acl)
} catch { exit 1 }
"#;
        let mut child = start_backup_acl_script(directory, SCRIPT)
            .context("cannot secure the private backup directory")?;
        let status = wait_backup_acl_script(&mut child, BACKUP_ACL_TIMEOUT).await?;
        if !status.success() {
            bail!(
                "cannot secure the private backup directory; choose a private parent without other users' delete or permission-changing access"
            )
        }
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let uid = std::fs::metadata(directory)?.uid();
        let parent = directory
            .parent()
            .context("backup directory has no parent")?
            .canonicalize()?;
        for ancestor in parent.ancestors() {
            let metadata = std::fs::metadata(ancestor)?;
            let mode = metadata.permissions().mode();
            if metadata.uid() != uid && metadata.uid() != 0
                || mode & 0o022 != 0 && mode & 0o1000 == 0
            {
                bail!(
                    "backup parent must be private and owned by the current user or administrator"
                )
            }
        }
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(windows)]
fn start_backup_acl_script(
    directory: &std::path::Path,
    script: &str,
) -> std::io::Result<tokio::process::Child> {
    tokio::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("KNOWELL_BACKUP_STAGING", directory)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0800_0000)
        .kill_on_drop(true)
        .spawn()
}

#[cfg(windows)]
async fn wait_backup_acl_script(
    child: &mut tokio::process::Child,
    timeout: Duration,
) -> anyhow::Result<std::process::ExitStatus> {
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => status.context("cannot wait for the backup security helper"),
        Err(elapsed) => {
            // Explicitly reap a timed-out child before returning. kill_on_drop
            // also protects callers that cancel this operation during cleanup.
            tokio::time::timeout(BACKUP_ACL_CLEANUP_TIMEOUT, child.kill())
                .await
                .context("backup security helper did not stop before the cleanup deadline")?
                .context("cannot stop the timed out backup security helper")?;
            Err(elapsed).context("securing the private backup directory timed out")
        }
    }
}

fn publish_backup(dump: &std::path::Path, destination: &std::path::Path) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(dump)?;
    if !metadata.is_file() || metadata.len() == 0 {
        bail!("pre-migration backup is empty or invalid")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dump, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    std::fs::OpenOptions::new()
        .write(true)
        .open(dump)?
        .sync_all()?;
    #[cfg(not(windows))]
    std::fs::File::open(dump)?.sync_all()?;
    // Same-volume hard-link publication cannot replace a competing file or link.
    std::fs::hard_link(dump, destination)
        .context("backup destination already exists or cannot be published")?;
    #[cfg(unix)]
    std::fs::File::open(
        destination
            .parent()
            .context("backup destination has no parent")?,
    )?
    .sync_all()?;
    Ok(())
}

/// Checks backup authorization and refuses overwriting any existing destination.
pub(crate) fn validate_migration_backup(
    cfg: &EngineConfig,
    destination: Option<&std::path::Path>,
    external_confirmed: bool,
) -> anyhow::Result<()> {
    match cfg.database.mode {
        DatabaseMode::Managed => {
            let destination =
                destination.context("managed migration requires a fresh backup destination")?;
            match std::fs::symlink_metadata(destination) {
                Ok(_) => bail!("backup destination exists; select a fresh file"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        DatabaseMode::External if external_confirmed => {}
        DatabaseMode::External => {
            bail!("external migration requires explicit backup and restore-test attestation")
        }
    }
    Ok(())
}

/// Refuses external maintenance unless its session/admission preconditions are explicit.
/// Transparent transaction poolers cannot be reliably detected from SQL alone.
pub(crate) fn validate_session_gates(cfg: &EngineConfig, confirmed: bool) -> anyhow::Result<()> {
    if cfg.database.mode == DatabaseMode::External && !confirmed {
        bail!(
            "external maintenance requires --session-gates-confirmed: use direct/session connections and stop clients without runtime admission; transaction/statement poolers are unsupported"
        );
    }
    Ok(())
}

/// A small runtime for the short CLI commands (`serve` builds its own).
pub(crate) fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("cannot start the async runtime")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_backup_publication_never_replaces_a_competing_destination() {
        let directory = tempfile::tempdir().unwrap();
        let dump = directory.path().join("synthetic.dump");
        let destination = directory.path().join("published.dump");
        let cfg = knowell_config::parse_engine("version = 1").unwrap();
        std::fs::write(&dump, b"synthetic database dump").unwrap();
        validate_migration_backup(&cfg, Some(&destination), false).unwrap();
        std::fs::write(&destination, b"competing synthetic backup").unwrap();
        assert!(publish_backup(&dump, &destination).is_err());
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"competing synthetic backup"
        );
        assert!(validate_migration_backup(&cfg, Some(&destination), false).is_err());
        let fresh = directory.path().join("fresh.dump");
        publish_backup(&dump, &fresh).unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"synthetic database dump");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(fresh).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let empty = directory.path().join("empty.dump");
        std::fs::write(&empty, b"").unwrap();
        let absent = directory.path().join("absent.dump");
        assert!(publish_backup(&empty, &absent).is_err());
        assert!(!absent.exists());
    }

    #[cfg(unix)]
    #[test]
    fn backup_publication_refuses_dangling_links_and_linked_dump_input() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let dump = directory.path().join("synthetic.dump");
        std::fs::write(&dump, b"synthetic database dump").unwrap();
        let absent = directory.path().join("absent.dump");
        let linked = directory.path().join("linked.dump");
        symlink(&absent, &linked).unwrap();
        let cfg = knowell_config::parse_engine("version = 1").unwrap();
        assert!(validate_migration_backup(&cfg, Some(&linked), false).is_err());
        assert!(publish_backup(&dump, &linked).is_err());
        assert!(std::fs::symlink_metadata(&linked).unwrap().is_symlink());
        let input_link = directory.path().join("input-link.dump");
        symlink(&dump, &input_link).unwrap();
        assert!(publish_backup(&input_link, &absent).is_err());
        assert!(!absent.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backup_staging_refuses_shared_writable_parents_before_private_bytes() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir_in(parent.path()).unwrap();
        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(restrict_backup_staging(staging.path()).await.is_err());
        assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 0);
        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        restrict_backup_staging(staging.path()).await.unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn backup_staging_acl_is_protected_and_inherits_only_the_current_sid() {
        let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        restrict_backup_staging(directory.path()).await.unwrap();
        let script = r#"
$ErrorActionPreference = 'Stop'
try {
    $path = [Environment]::GetEnvironmentVariable('KNOWELL_BACKUP_STAGING')
    $acl = [IO.Directory]::GetAccessControl($path)
    $owner = [Security.Principal.WindowsIdentity]::GetCurrent().User
    $rules = @($acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]))
    if (!$acl.AreAccessRulesProtected -or $rules.Count -ne 1) { exit 1 }
    if ($rules[0].IdentityReference -ne $owner -or $rules[0].IsInherited) { exit 1 }
    if ([int]$rules[0].InheritanceFlags -ne 3 -or $rules[0].AccessControlType -ne 'Allow') { exit 1 }
    $everyone = [Security.Principal.SecurityIdentifier]::new('S-1-1-0')
    $unsafeRule = [Security.AccessControl.FileSystemAccessRule]::new($everyone, 'DeleteSubdirectoriesAndFiles', 'None', 'None', 'Allow')
    $acl.AddAccessRule($unsafeRule)
    [IO.Directory]::SetAccessControl($path, $acl)
} catch { exit 1 }
"#;
        let mut child = start_backup_acl_script(directory.path(), script).unwrap();
        let status = wait_backup_acl_script(&mut child, BACKUP_ACL_TIMEOUT)
            .await
            .unwrap();
        assert!(status.success());
        let nested = tempfile::tempdir_in(directory.path()).unwrap();
        assert!(restrict_backup_staging(nested.path()).await.is_err());
        assert_eq!(std::fs::read_dir(nested.path()).unwrap().count(), 0);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn backup_staging_helper_timeout_is_an_error_and_reaps_the_child() {
        let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let mut child =
            start_backup_acl_script(directory.path(), "Start-Sleep -Seconds 120").unwrap();
        let error = wait_backup_acl_script(&mut child, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(
            error
                .downcast_ref::<tokio::time::error::Elapsed>()
                .is_some()
        );
        assert!(child.try_wait().unwrap().is_some());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}
