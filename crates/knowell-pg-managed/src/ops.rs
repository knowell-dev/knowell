//! Backup, restore and major-version upgrade.

use crate::error::{Error, Result};
use crate::extension;
use crate::install::MIN_MAJOR;
use crate::layout::Layout;
use crate::manager::{ManagedPostgres, Status, connection_args};
use crate::process::{self, args};
use crate::state;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Limit for the cheap `pg_upgrade --check` pass.
const CHECK_TIMEOUT: Duration = Duration::from_secs(600);

impl ManagedPostgres {
    /// Dump `database` to `dest` in PostgreSQL's custom format (`pg_dump -Fc`).
    ///
    /// The dump is written to `<dest>.partial` and renamed on success, so a
    /// failed run never leaves a file that looks like a valid backup. On Unix
    /// the result is owner-only (0600) because it contains the indexed data.
    ///
    /// # Errors
    /// [`Error::NotRunning`], [`Error::InvalidIdentifier`], [`Error::Command`]
    /// or filesystem errors.
    pub async fn backup(&self, database: &str, dest: &Path) -> Result<()> {
        if !extension::is_valid_identifier(database) {
            return Err(Error::InvalidIdentifier(database.to_string()));
        }
        let port = self.running_port().await?;
        let pg_dump = self.program("pg_dump")?;
        if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|err| Error::io(format!("creating {}", parent.display()), err))?;
        }
        let mut partial = dest.as_os_str().to_os_string();
        partial.push(".partial");
        let partial = PathBuf::from(partial);

        let (_guard, options) = self.client_options(self.config.operation_timeout)?;
        let mut argv = args(["-Fc", "-f"]);
        argv.push(partial.clone().into_os_string());
        argv.extend(connection_args(port));
        argv.push(OsString::from("-d"));
        argv.push(OsString::from(database));
        if let Err(err) = process::run(&pg_dump, &argv, &options).await {
            let _ = std::fs::remove_file(&partial);
            return Err(err);
        }
        restrict_to_owner(&partial)?;
        std::fs::rename(&partial, dest)
            .map_err(|err| Error::io(format!("moving the backup to {}", dest.display()), err))?;
        tracing::info!(database, "backup written");
        Ok(())
    }

    /// Restore a [`backup`](Self::backup) into a **new** database called
    /// `database` (`pg_restore --exit-on-error`).
    ///
    /// The database is created first and dropped again if the restore fails,
    /// so nothing half-restored remains. Extensions referenced by the dump
    /// (pgvector) must already be installed.
    ///
    /// # Errors
    /// [`Error::DatabaseExists`] if `database` exists, [`Error::NotRunning`],
    /// [`Error::InvalidIdentifier`], or the `pg_restore` failure.
    pub async fn restore(&self, source: &Path, database: &str) -> Result<()> {
        if !extension::is_valid_identifier(database) {
            return Err(Error::InvalidIdentifier(database.to_string()));
        }
        if !source.is_file() {
            return Err(Error::io(
                format!("reading the backup {}", source.display()),
                std::io::Error::from(std::io::ErrorKind::NotFound),
            ));
        }
        let port = self.running_port().await?;
        let pg_restore = self.program("pg_restore")?;
        if self.database_exists(database).await? {
            return Err(Error::DatabaseExists(database.to_string()));
        }
        self.ensure_database(database).await?;

        let (_guard, options) = self.client_options(self.config.operation_timeout)?;
        let mut argv = args(["--exit-on-error", "--no-owner"]);
        argv.extend(connection_args(port));
        argv.push(OsString::from("-d"));
        argv.push(OsString::from(database));
        argv.push(source.as_os_str().to_os_string());
        if let Err(err) = process::run(&pg_restore, &argv, &options).await {
            if let Err(cleanup) = self.drop_database(database).await {
                tracing::warn!(database, error = %cleanup, "could not drop the partially restored database");
            }
            return Err(err);
        }
        tracing::info!(database, "backup restored");
        Ok(())
    }

    /// Upgrade the cluster to `new_major` with `pg_upgrade`, returning a handle
    /// for the new version.
    ///
    /// Steps: install `new_major`, optionally install a pgvector bundle for it,
    /// initialise a new data directory with the same password, run
    /// `pg_upgrade --check`, then `pg_upgrade` in copy mode. The old data
    /// directory is **never modified or deleted**; call
    /// [`remove_data_dir`](Self::remove_data_dir) on the old handle after the new
    /// cluster has been verified. If anything fails, the new data directory is
    /// removed again and the old cluster is untouched.
    ///
    /// The cluster must be stopped. `pg_upgrade` does not carry optimizer
    /// statistics, so run `ANALYZE` after the first start.
    ///
    /// `extension_bundle` must be given when the databases use pgvector, since
    /// `pg_upgrade` needs the extension's library for the new version.
    ///
    /// # Errors
    /// [`Error::Upgrade`] for an invalid request, [`Error::NotStopped`] if the
    /// server is running, [`Error::PgUpgradeUnavailable`] if the new
    /// distribution lacks `pg_upgrade`, or the failing step's error.
    pub async fn upgrade(
        &self,
        new_major: u32,
        extension_bundle: Option<&Path>,
    ) -> Result<ManagedPostgres> {
        if new_major <= self.config.major {
            return Err(Error::Upgrade(format!(
                "target major {new_major} must be greater than the current {}",
                self.config.major
            )));
        }
        if new_major < MIN_MAJOR {
            return Err(Error::Upgrade(format!(
                "target major must be at least {MIN_MAJOR}"
            )));
        }
        match self.status().await? {
            Status::Stopped => {}
            Status::Running { .. } => return Err(Error::NotStopped),
            Status::StalePostmasterPid { pid } => {
                return Err(Error::StalePostmasterPid {
                    pid,
                    path: self.layout.postmaster_pid(),
                });
            }
            Status::NotInstalled | Status::Installed => {
                return Err(Error::NotInitialized(self.layout.data_dir()));
            }
        }

        let mut config = self.config.clone();
        config.major = new_major;
        let mut next = ManagedPostgres::new(config)?;
        next.passwords = self.passwords.clone();

        if next.layout.pg_version_file().is_file() || next.layout.data_dir().exists() {
            return Err(Error::Upgrade(format!(
                "{} already exists; remove it or finish with that cluster",
                next.layout.data_dir().display()
            )));
        }

        next.install().await?;
        let new_pg_upgrade = match next.program("pg_upgrade") {
            Ok(path) => path,
            Err(Error::MissingProgram { .. }) => {
                return Err(Error::PgUpgradeUnavailable { major: new_major });
            }
            Err(err) => return Err(err),
        };
        if let Some(bundle) = extension_bundle {
            next.install_extension_bundle(bundle).await?;
        }
        next.init_data_dir().await?;

        match self.run_pg_upgrade(&next, &new_pg_upgrade).await {
            Ok(()) => {
                tracing::info!(
                    from = self.config.major,
                    to = new_major,
                    "pg_upgrade finished; old data directory kept"
                );
                Ok(next)
            }
            Err(err) => {
                let new_data = next.layout.data_dir();
                if let Err(cleanup) = std::fs::remove_dir_all(&new_data) {
                    tracing::warn!(error = %cleanup, "could not remove the new data directory after a failed upgrade");
                }
                Err(err)
            }
        }
    }

    /// Run `pg_upgrade --check`, then the real upgrade.
    async fn run_pg_upgrade(&self, next: &ManagedPostgres, pg_upgrade: &Path) -> Result<()> {
        let (old_dist, new_dist) = (
            self.dist_dir()?.ok_or(Error::NotInstalled {
                major: self.config.major,
            })?,
            next.dist_dir()?.ok_or(Error::NotInstalled {
                major: next.config.major,
            })?,
        );
        let old_port = state::pick_free_port()?;
        let mut new_port = state::pick_free_port()?;
        while new_port == old_port {
            new_port = state::pick_free_port()?;
        }

        // Both clusters share one password, so one PGPASSFILE serves both.
        let (_guard, base) = self.client_options(self.config.operation_timeout)?;
        // pg_upgrade writes its logs into the working directory.
        let work_dir = work_dir(&next.layout)?;
        // pg_upgrade starts servers that would inherit pipes on Windows; use a file.
        let options = base
            .cwd(&work_dir)
            .output_file(&next.layout.major_dir().join("pg_upgrade.out"));

        let common: Vec<OsString> = vec![
            OsString::from("-b"),
            old_dist.join("bin").into_os_string(),
            OsString::from("-B"),
            new_dist.join("bin").into_os_string(),
            OsString::from("-d"),
            self.layout.data_dir().into_os_string(),
            OsString::from("-D"),
            next.layout.data_dir().into_os_string(),
            OsString::from("-U"),
            OsString::from("postgres"),
            OsString::from("-p"),
            OsString::from(old_port.to_string()),
            OsString::from("-P"),
            OsString::from(new_port.to_string()),
        ];
        let mut check = common.clone();
        check.push(OsString::from("--check"));
        let mut check_options = options.clone();
        check_options.timeout = CHECK_TIMEOUT;
        process::run(pg_upgrade, &check, &check_options).await?;
        process::run(pg_upgrade, &common, &options).await?;
        // pg_upgrade removes its log directory on success; drop the empty folder too.
        let _ = std::fs::remove_dir(&work_dir);
        Ok(())
    }
}

/// A scratch directory for `pg_upgrade` logs, inside the new major's folder.
fn work_dir(layout: &Layout) -> Result<PathBuf> {
    let dir = layout.major_dir().join("upgrade-work");
    std::fs::create_dir_all(&dir)
        .map_err(|err| Error::io(format!("creating {}", dir.display()), err))?;
    Ok(dir)
}

/// Make `path` readable by its owner only (Unix); a no-op elsewhere, where the
/// containing directory's inherited rights apply.
fn restrict_to_owner(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|err| Error::io(format!("restricting {}", path.display()), err))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{Error, ManagedConfig, ManagedPostgres};

    fn manager(home: &std::path::Path, major: u32) -> ManagedPostgres {
        ManagedPostgres::new(ManagedConfig::new(home).with_major(major)).unwrap()
    }

    #[tokio::test]
    async fn upgrade_rejects_same_or_older_major() {
        let home = tempfile::tempdir().unwrap();
        let pg = manager(home.path(), 17);
        for target in [17, 16] {
            assert!(matches!(
                pg.upgrade(target, None).await,
                Err(Error::Upgrade(_))
            ));
        }
    }

    #[tokio::test]
    async fn upgrade_requires_an_initialised_cluster() {
        let home = tempfile::tempdir().unwrap();
        let pg = manager(home.path(), 17);
        assert!(matches!(
            pg.upgrade(18, None).await,
            Err(Error::NotInitialized(_))
        ));
    }

    #[tokio::test]
    async fn operations_need_a_running_server() {
        let home = tempfile::tempdir().unwrap();
        let pg = manager(home.path(), 17);
        let dest = home.path().join("b.dump");
        assert!(matches!(
            pg.backup("knowell", &dest).await,
            Err(Error::NotRunning)
        ));
        assert!(matches!(
            pg.backup("Bad Name", &dest).await,
            Err(Error::InvalidIdentifier(_))
        ));
        assert!(matches!(
            pg.restore(&dest, "knowell").await,
            Err(Error::Io { .. })
        ));
        assert!(!dest.exists());
    }
}
