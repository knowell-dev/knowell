//! The [`ManagedPostgres`] lifecycle.

use crate::error::{Error, Result};
use crate::extension;
use crate::install::{self, DEFAULT_MAJOR, MIN_MAJOR};
use crate::layout::Layout;
use crate::password::{self, FilePasswordStore, PasswordStore, TempSecretFile, create_private_dir};
use crate::process::{self, RunOptions, args, program_path};
use crate::state::{self, State};
use postgresql_archive::Version;
use secrecy::{ExposeSecret, SecretString};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The superuser Knowell connects as.
pub(crate) const SUPERUSER: &str = "postgres";

/// Loopback address the server listens on. Never a wildcard address.
pub const LISTEN_ADDRESS: &str = "127.0.0.1";

/// Limit for short administrative commands (`initdb`, `pg_ctl`, `psql`).
const SHORT_TIMEOUT: Duration = Duration::from_secs(120);

/// Seconds `pg_ctl` waits for startup or shutdown before giving up.
const PG_CTL_WAIT_SECONDS: &str = "60";

/// Configuration for a [`ManagedPostgres`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedConfig {
    /// Knowell home directory; everything lives under `<knowell_home>/pg`.
    pub knowell_home: PathBuf,
    /// PostgreSQL major version (default 17, minimum 15).
    pub major: u32,
    /// Upper bound for long operations: backup, restore, upgrade. Default one hour.
    pub operation_timeout: Duration,
}

impl ManagedConfig {
    /// Configuration with the default major version (17) under `knowell_home`.
    #[must_use]
    pub fn new(knowell_home: impl Into<PathBuf>) -> Self {
        Self {
            knowell_home: knowell_home.into(),
            major: DEFAULT_MAJOR,
            operation_timeout: Duration::from_secs(3600),
        }
    }

    /// Use a different PostgreSQL major version.
    #[must_use]
    pub fn with_major(mut self, major: u32) -> Self {
        self.major = major;
        self
    }
}

/// What state the managed instance is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// No distribution for the configured major version is cached.
    NotInstalled,
    /// Binaries are present but the data directory is not initialised.
    Installed,
    /// Initialised and not running.
    Stopped,
    /// A server is running on the loopback interface.
    Running {
        /// Postmaster process id.
        pid: u32,
        /// TCP port on 127.0.0.1.
        port: u16,
    },
    /// `postmaster.pid` exists but the recorded server is gone (crash, kill, power loss).
    StalePostmasterPid {
        /// Pid recorded in the file.
        pid: u32,
    },
}

/// A PostgreSQL server that Knowell installs and runs itself.
///
/// Created with [`ManagedPostgres::new`]; nothing touches the disk until a
/// lifecycle method is called. Methods take `&self`: the state lives on disk,
/// so several handles to the same home agree with each other.
#[derive(Clone)]
pub struct ManagedPostgres {
    pub(crate) layout: Layout,
    pub(crate) config: ManagedConfig,
    pub(crate) passwords: Arc<dyn PasswordStore>,
}

impl std::fmt::Debug for ManagedPostgres {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedPostgres")
            .field("knowell_home", &self.config.knowell_home)
            .field("major", &self.config.major)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl ManagedPostgres {
    /// Create a handle using the file-based [`PasswordStore`] at
    /// `<knowell_home>/pg/password`.
    ///
    /// # Errors
    /// [`Error::InvalidConfig`] if the major version is below the supported minimum.
    pub fn new(config: ManagedConfig) -> Result<Self> {
        if config.major < MIN_MAJOR {
            return Err(Error::InvalidConfig(format!(
                "postgresql major version must be at least {MIN_MAJOR}, got {}",
                config.major
            )));
        }
        let layout = Layout::new(&config.knowell_home, config.major);
        let passwords = Arc::new(FilePasswordStore::new(layout.password_file()));
        Ok(Self {
            layout,
            config,
            passwords,
        })
    }

    /// Replace the password store (for example with an OS keychain implementation).
    #[must_use]
    pub fn with_password_store(mut self, store: Arc<dyn PasswordStore>) -> Self {
        self.passwords = store;
        self
    }

    /// Paths used by this instance.
    #[must_use]
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The configured PostgreSQL major version.
    #[must_use]
    pub fn major(&self) -> u32 {
        self.config.major
    }

    /// Directory of the installed distribution, if any.
    ///
    /// # Errors
    /// [`Error::Io`] if the cache directory cannot be listed.
    pub fn dist_dir(&self) -> Result<Option<PathBuf>> {
        Ok(install::newest_installed(&self.layout)?.map(|(_, dir)| dir))
    }

    fn require_dist(&self) -> Result<(Version, PathBuf)> {
        install::newest_installed(&self.layout)?.ok_or(Error::NotInstalled {
            major: self.config.major,
        })
    }

    /// Absolute path of a program in the distribution's `bin` directory.
    pub(crate) fn program(&self, name: &str) -> Result<PathBuf> {
        let (_, dist) = self.require_dist()?;
        let path = program_path(&dist.join("bin"), name);
        if path.is_file() {
            Ok(path)
        } else {
            Err(Error::MissingProgram {
                program: name.to_string(),
                dist,
            })
        }
    }

    /// Download (if not cached) and verify the PostgreSQL binaries for the
    /// configured major version into `<knowell_home>/pg/dist/<version>`.
    ///
    /// An already cached minor version is reused without network access.
    /// Returns the installed version.
    ///
    /// # Errors
    /// [`Error::Download`] when the release cannot be resolved, downloaded,
    /// checksum-verified or unpacked.
    pub async fn install(&self) -> Result<Version> {
        create_private_dir(self.layout.pg_root())?;
        let (version, _) = install::ensure_installed(&self.layout).await?;
        Ok(version)
    }

    /// Initialise the data directory (`initdb`) with a freshly generated
    /// superuser password and loopback-only server settings.
    ///
    /// Returns `true` if a new cluster was created and `false` if one already
    /// existed (nothing is changed then). If `initdb` fails, a directory this
    /// call created is removed again.
    ///
    /// # Errors
    /// [`Error::NotInstalled`] without binaries; [`Error::Command`] if
    /// `initdb` fails; store and filesystem errors.
    pub async fn init_data_dir(&self) -> Result<bool> {
        let initdb = self.program("initdb")?;
        if self.layout.pg_version_file().is_file() {
            return Ok(false);
        }
        create_private_dir(&self.layout.major_dir())?;
        let (password, _created) = password::load_or_create(self.passwords.as_ref())?;
        let pwfile = TempSecretFile::password_only(&self.layout.tmp_dir(), &password)?;

        let data_dir = self.layout.data_dir();
        let existed = data_dir.exists();
        let mut argv = args([
            "--username=postgres",
            "--auth=scram-sha-256",
            "--encoding=UTF8",
            // C collation: identical on every platform and across majors, which
            // keeps pg_upgrade and cross-machine restores deterministic.
            "--locale=C",
            "--data-checksums",
        ]);
        argv.push(OsString::from("--pwfile"));
        argv.push(pwfile.path().as_os_str().to_os_string());
        argv.push(OsString::from("-D"));
        argv.push(data_dir.as_os_str().to_os_string());

        let options = RunOptions::new(SHORT_TIMEOUT).redact(password.expose_secret());
        if let Err(err) = process::run(&initdb, &argv, &options).await {
            if !existed {
                let _ = std::fs::remove_dir_all(&data_dir);
            }
            return Err(err);
        }
        self.write_managed_conf(None)?;
        self.include_managed_conf()?;
        tracing::info!(major = self.config.major, "initialised data directory");
        Ok(true)
    }

    /// Append the include of Knowell's managed settings to `postgresql.conf`.
    fn include_managed_conf(&self) -> Result<()> {
        use std::io::Write;
        let path = self.layout.data_dir().join("postgresql.conf");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|err| Error::io(format!("opening {}", path.display()), err))?;
        file.write_all(b"\n# Knowell: loopback and port settings live in knowell.conf\ninclude_if_exists = 'knowell.conf'\n")
            .map_err(|err| Error::io(format!("writing {}", path.display()), err))
    }

    /// (Re)write `knowell.conf`. Called on every start so the loopback-only
    /// settings cannot be changed by editing the file.
    fn write_managed_conf(&self, port: Option<u16>) -> Result<()> {
        let path = self.layout.managed_conf();
        std::fs::write(&path, render_managed_conf(port))
            .map_err(|err| Error::io(format!("writing {}", path.display()), err))
    }

    /// Current state of the instance.
    ///
    /// # Errors
    /// Filesystem errors, an unreadable `postmaster.pid`, or a failure to run `pg_ctl`.
    pub async fn status(&self) -> Result<Status> {
        let Some((_, dist)) = install::newest_installed(&self.layout)? else {
            return Ok(Status::NotInstalled);
        };
        if !self.layout.pg_version_file().is_file() {
            return Ok(Status::Installed);
        }
        let pid_path = self.layout.postmaster_pid();
        let text = match std::fs::read_to_string(&pid_path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Status::Stopped),
            Err(err) => {
                return Err(Error::io(format!("reading {}", pid_path.display()), err));
            }
        };
        let mut lines = text.lines();
        let pid = lines
            .next()
            .and_then(|line| line.trim().parse::<u32>().ok())
            .ok_or_else(|| Error::State {
                path: pid_path.clone(),
                message: "first line is not a process id".to_string(),
            })?;
        // postmaster.pid: pid, data dir, start time, port, ...
        let pid_file_port = lines
            .nth(2)
            .and_then(|line| line.trim().parse::<u16>().ok());

        let pg_ctl = program_path(&dist.join("bin"), "pg_ctl");
        let mut argv = args(["status", "-D"]);
        argv.push(self.layout.data_dir().into_os_string());
        let output =
            process::run_unchecked(&pg_ctl, &argv, &RunOptions::new(SHORT_TIMEOUT)).await?;
        match output.code {
            Some(0) => {
                let port = match pid_file_port {
                    Some(port) => port,
                    None => State::load(&self.layout.state_file())?
                        .map(|s| s.port)
                        .ok_or(Error::State {
                            path: pid_path,
                            message: "server is running but its port is unknown".to_string(),
                        })?,
                };
                Ok(Status::Running { pid, port })
            }
            // 3 = no server running although a pid file exists.
            Some(3) => Ok(Status::StalePostmasterPid { pid }),
            _ => Err(Error::Command {
                program: "pg_ctl".to_string(),
                code: output
                    .code
                    .map_or_else(|| "none".to_string(), |c| c.to_string()),
                output: output.stderr.trim().to_string(),
            }),
        }
    }

    /// Start the server on `127.0.0.1` and return its port.
    ///
    /// The port is chosen at first start and kept in the state file; if it has
    /// since been taken by another program a new free port is chosen and saved.
    /// The server never listens on any other address.
    ///
    /// # Errors
    /// [`Error::AlreadyRunning`] if a server already uses this data directory,
    /// [`Error::StalePostmasterPid`] if a crashed one left its pid file,
    /// [`Error::NotInitialized`] before `init_data_dir`, or the `pg_ctl` failure
    /// (the server log is `<knowell_home>/pg/<major>/postgres.log`).
    pub async fn start(&self) -> Result<u16> {
        let pg_ctl = self.program("pg_ctl")?;
        match self.status().await? {
            Status::NotInstalled => {
                return Err(Error::NotInstalled {
                    major: self.config.major,
                });
            }
            Status::Installed => return Err(Error::NotInitialized(self.layout.data_dir())),
            Status::Running { pid, port } => return Err(Error::AlreadyRunning { pid, port }),
            Status::StalePostmasterPid { pid } => {
                return Err(Error::StalePostmasterPid {
                    pid,
                    path: self.layout.postmaster_pid(),
                });
            }
            Status::Stopped => {}
        }

        let state_path = self.layout.state_file();
        let port = match State::load(&state_path)? {
            Some(saved) if state::port_is_free(saved.port) => saved.port,
            previous => {
                let fresh = state::pick_free_port()?;
                if let Some(old) = previous {
                    tracing::warn!(
                        old_port = old.port,
                        new_port = fresh,
                        "saved port is in use; choosing a new one"
                    );
                }
                State::new(fresh).save(&state_path)?;
                fresh
            }
        };
        self.write_managed_conf(Some(port))?;

        let mut argv = args(["start", "-w", "-t", PG_CTL_WAIT_SECONDS, "-D"]);
        argv.push(self.layout.data_dir().into_os_string());
        argv.push(OsString::from("-l"));
        argv.push(self.layout.log_file().into_os_string());
        let options = RunOptions::new(SHORT_TIMEOUT).null_stdio();
        process::run(&pg_ctl, &argv, &options)
            .await
            .map_err(|error| match error {
                Error::Command { program, code, .. } => Error::Command {
                    program,
                    code,
                    output: format!(
                        "check the server log at {}",
                        self.layout.log_file().display()
                    ),
                },
                other => other,
            })?;
        tracing::info!(port, "postgresql started");
        Ok(port)
    }

    /// Stop the server (fast shutdown) and wait for it. Does nothing if it is
    /// not running.
    ///
    /// # Errors
    /// [`Error::StalePostmasterPid`] if only a stale pid file exists, or the
    /// `pg_ctl` failure.
    pub async fn stop(&self) -> Result<()> {
        match self.status().await? {
            Status::Running { .. } => {}
            Status::StalePostmasterPid { pid } => {
                return Err(Error::StalePostmasterPid {
                    pid,
                    path: self.layout.postmaster_pid(),
                });
            }
            Status::NotInstalled | Status::Installed | Status::Stopped => return Ok(()),
        }
        let pg_ctl = self.program("pg_ctl")?;
        let mut argv = args(["stop", "-m", "fast", "-w", "-t", PG_CTL_WAIT_SECONDS, "-D"]);
        argv.push(self.layout.data_dir().into_os_string());
        process::run(&pg_ctl, &argv, &RunOptions::new(SHORT_TIMEOUT)).await?;
        tracing::info!("postgresql stopped");
        Ok(())
    }

    /// Delete a `postmaster.pid` left behind by a crashed server.
    ///
    /// Returns `true` if a stale file was removed. Refuses to touch the file
    /// while a server is actually running.
    ///
    /// # Errors
    /// [`Error::AlreadyRunning`] if the server is alive; filesystem errors.
    pub async fn clear_stale_postmaster_pid(&self) -> Result<bool> {
        match self.status().await? {
            Status::Running { pid, port } => Err(Error::AlreadyRunning { pid, port }),
            Status::StalePostmasterPid { .. } => {
                let path = self.layout.postmaster_pid();
                std::fs::remove_file(&path)
                    .map_err(|err| Error::io(format!("removing {}", path.display()), err))?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Delete this major version's data directory and state file. The password
    /// and the cached binaries stay. Used to drop an old cluster once an
    /// upgrade has been verified.
    ///
    /// # Errors
    /// [`Error::NotStopped`] if a server is running or a pid file is present.
    pub async fn remove_data_dir(&self) -> Result<()> {
        match self.status().await? {
            Status::Running { .. } | Status::StalePostmasterPid { .. } => {
                return Err(Error::NotStopped);
            }
            _ => {}
        }
        let data = self.layout.data_dir();
        if data.exists() {
            std::fs::remove_dir_all(&data)
                .map_err(|err| Error::io(format!("removing {}", data.display()), err))?;
        }
        match std::fs::remove_file(self.layout.state_file()) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(Error::io("removing the state file", err)),
        }
    }

    /// The loopback port saved in the state file, if one has been chosen.
    ///
    /// # Errors
    /// [`Error::State`] if the file is malformed.
    pub fn port(&self) -> Result<Option<u16>> {
        Ok(State::load(&self.layout.state_file())?.map(|s| s.port))
    }

    /// The stored superuser password.
    pub(crate) fn password(&self) -> Result<SecretString> {
        self.passwords.get()?.ok_or_else(|| {
            Error::Password("no password is stored yet; run init_data_dir() first".to_string())
        })
    }

    /// A `postgresql://` URL for `database` on the managed server.
    ///
    /// The URL contains the password, so it is returned as a [`SecretString`];
    /// do not log it. The server need not be running, but a port must have
    /// been chosen by an earlier `start()`.
    ///
    /// # Errors
    /// [`Error::InvalidIdentifier`] for a bad database name,
    /// [`Error::NotRunning`] if no port has been chosen yet, and password store errors.
    pub fn connection_url(&self, database: &str) -> Result<SecretString> {
        if !extension::is_valid_identifier(database) {
            return Err(Error::InvalidIdentifier(database.to_string()));
        }
        let port = self.port()?.ok_or(Error::NotRunning)?;
        let password = self.password()?;
        Ok(SecretString::from(format!(
            "postgresql://{SUPERUSER}:{}@{LISTEN_ADDRESS}:{port}/{database}",
            percent_encode(password.expose_secret())
        )))
    }

    /// Port of the running server, or [`Error::NotRunning`].
    pub(crate) async fn running_port(&self) -> Result<u16> {
        match self.status().await? {
            Status::Running { port, .. } => Ok(port),
            Status::StalePostmasterPid { pid } => Err(Error::StalePostmasterPid {
                pid,
                path: self.layout.postmaster_pid(),
            }),
            _ => Err(Error::NotRunning),
        }
    }

    /// Options carrying `PGPASSFILE` for client programs. Keep the returned
    /// guard alive until the child has exited.
    pub(crate) fn client_options(&self, timeout: Duration) -> Result<(TempSecretFile, RunOptions)> {
        let password = self.password()?;
        let file = TempSecretFile::pgpass(&self.layout.tmp_dir(), SUPERUSER, &password)?;
        let options = RunOptions::new(timeout)
            .env("PGPASSFILE", file.path().as_os_str())
            // pg_upgrade connects to "localhost" by default, which hangs on some
            // Windows resolvers; an explicit loopback address avoids that.
            .env("PGHOST", LISTEN_ADDRESS)
            .env("PGCONNECT_TIMEOUT", "10")
            // Output and server messages in UTF-8 whatever the console code page.
            .env("PGCLIENTENCODING", "UTF8")
            .redact(password.expose_secret());
        Ok((file, options))
    }

    /// Run SQL in `database` through `psql` as the superuser and return trimmed
    /// stdout (tuples only, unaligned). The SQL is passed as a single argument, so
    /// callers must not interpolate untrusted text into it. On Windows `psql`
    /// decodes arguments with the ANSI code page, so write non-ASCII literals as
    /// SQL unicode escapes (`U&'w\00f6rld'`). Needs a running server.
    ///
    /// # Errors
    /// [`Error::NotRunning`] or the `psql` failure (output redacted).
    pub async fn run_sql(&self, database: &str, sql: &str) -> Result<String> {
        let port = self.running_port().await?;
        let psql = self.program("psql")?;
        let (_guard, options) = self.client_options(SHORT_TIMEOUT)?;
        let mut argv = args(["-X", "-q", "-t", "-A", "-v", "ON_ERROR_STOP=1"]);
        argv.extend(connection_args(port));
        argv.push(OsString::from("-d"));
        argv.push(OsString::from(database));
        argv.push(OsString::from("-c"));
        argv.push(OsString::from(sql));
        let output = process::run(&psql, &argv, &options).await?;
        Ok(output.stdout.trim().to_string())
    }

    /// Create `name` if it does not exist. Returns `true` if it was created.
    ///
    /// # Errors
    /// [`Error::InvalidIdentifier`], [`Error::NotRunning`] or a `psql` failure.
    pub async fn ensure_database(&self, name: &str) -> Result<bool> {
        if !extension::is_valid_identifier(name) {
            return Err(Error::InvalidIdentifier(name.to_string()));
        }
        if self.database_exists(name).await? {
            return Ok(false);
        }
        self.run_sql("postgres", &format!("CREATE DATABASE \"{name}\""))
            .await?;
        Ok(true)
    }

    /// Whether a database called `name` exists.
    pub(crate) async fn database_exists(&self, name: &str) -> Result<bool> {
        if !extension::is_valid_identifier(name) {
            return Err(Error::InvalidIdentifier(name.to_string()));
        }
        let found = self
            .run_sql(
                "postgres",
                &format!("SELECT 1 FROM pg_database WHERE datname = '{name}'"),
            )
            .await?;
        Ok(found == "1")
    }

    /// Drop a database; used to clean up after a failed restore.
    pub(crate) async fn drop_database(&self, name: &str) -> Result<()> {
        if !extension::is_valid_identifier(name) {
            return Err(Error::InvalidIdentifier(name.to_string()));
        }
        self.run_sql("postgres", &format!("DROP DATABASE IF EXISTS \"{name}\""))
            .await?;
        Ok(())
    }

    /// Default version of `extension` according to `pg_available_extensions`,
    /// or `None` if the server cannot install it (for example pgvector's
    /// files have not been placed by [`install_extension_bundle`](Self::install_extension_bundle)).
    ///
    /// # Errors
    /// [`Error::NotRunning`], [`Error::InvalidIdentifier`] or a `psql` failure.
    pub async fn extension_available(&self, extension: &str) -> Result<Option<String>> {
        if !extension::is_valid_identifier(extension) {
            return Err(Error::InvalidIdentifier(extension.to_string()));
        }
        let version = self
            .run_sql(
                "postgres",
                &format!(
                    "SELECT default_version FROM pg_available_extensions WHERE name = '{extension}'"
                ),
            )
            .await?;
        Ok(if version.is_empty() {
            None
        } else {
            Some(version)
        })
    }

    /// Validate a pgvector build and copy it into the managed distribution
    /// (see the [`extension`](crate) module docs for the bundle layout).
    /// Returns the pgvector version declared by the bundle.
    ///
    /// The server does not need to be stopped; `CREATE EXTENSION vector` sees
    /// the files immediately.
    ///
    /// # Errors
    /// [`Error::Bundle`] if the bundle is incomplete or malformed,
    /// [`Error::MissingProgram`] if the distribution lacks `pg_config`.
    pub async fn install_extension_bundle(&self, bundle_dir: &Path) -> Result<String> {
        let bundle = extension::validate_bundle(bundle_dir)?;
        let pg_config = self.program("pg_config")?;
        let options = RunOptions::new(SHORT_TIMEOUT);
        let pkglibdir = pg_config_dir(&pg_config, "--pkglibdir", &options).await?;
        let sharedir = pg_config_dir(&pg_config, "--sharedir", &options).await?;
        extension::install_bundle(&bundle, &pkglibdir, &sharedir.join("extension"))?;
        tracing::info!(version = %bundle.version, "pgvector bundle installed");
        Ok(bundle.version)
    }
}

/// Ask `pg_config` for a directory.
async fn pg_config_dir(pg_config: &Path, flag: &str, options: &RunOptions) -> Result<PathBuf> {
    let output = process::run(pg_config, &args([flag]), options).await?;
    let text = output.stdout.trim();
    if text.is_empty() {
        return Err(Error::Command {
            program: "pg_config".to_string(),
            code: "0".to_string(),
            output: format!("{flag} printed nothing"),
        });
    }
    Ok(PathBuf::from(text))
}

/// Arguments that point a client tool at the loopback server without a password.
pub(crate) fn connection_args(port: u16) -> Vec<OsString> {
    vec![
        OsString::from("-h"),
        OsString::from(LISTEN_ADDRESS),
        OsString::from("-p"),
        OsString::from(port.to_string()),
        OsString::from("-U"),
        OsString::from(SUPERUSER),
        OsString::from("-w"),
    ]
}

/// Text of `knowell.conf`.
///
/// `listen_addresses` is a single loopback address; Unix sockets are switched
/// off where the platform has them so TCP on 127.0.0.1 is the only entrance.
pub(crate) fn render_managed_conf(port: Option<u16>) -> String {
    let mut text = String::from("# Written by Knowell on every start; edits are overwritten.\n");
    text.push_str(&format!("listen_addresses = '{LISTEN_ADDRESS}'\n"));
    if let Some(port) = port {
        text.push_str(&format!("port = {port}\n"));
    }
    // Windows builds before PostgreSQL 18 do not know this setting.
    if !cfg!(windows) {
        text.push_str("unix_socket_directories = ''\n");
    }
    text
}

/// Percent-encode everything outside the URL "unreserved" set.
pub(crate) fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// In-memory store to test the trait seam and redaction.
    #[derive(Default)]
    struct MemoryStore(Mutex<Option<String>>);

    impl std::fmt::Debug for MemoryStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("MemoryStore(<redacted>)")
        }
    }

    impl PasswordStore for MemoryStore {
        fn get(&self) -> Result<Option<SecretString>> {
            let guard = self
                .0
                .lock()
                .map_err(|_| Error::Password("poisoned".into()))?;
            Ok(guard.clone().map(SecretString::from))
        }
        fn set(&self, password: &SecretString) -> Result<()> {
            let mut guard = self
                .0
                .lock()
                .map_err(|_| Error::Password("poisoned".into()))?;
            *guard = Some(password.expose_secret().to_string());
            Ok(())
        }
    }

    fn manager(home: &Path) -> ManagedPostgres {
        ManagedPostgres::new(ManagedConfig::new(home)).unwrap()
    }

    #[test]
    fn default_major_is_17_and_old_majors_are_rejected() {
        assert_eq!(ManagedConfig::new("h").major, 17);
        assert!(ManagedPostgres::new(ManagedConfig::new("h").with_major(9)).is_err());
        assert!(ManagedPostgres::new(ManagedConfig::new("h").with_major(18)).is_ok());
    }

    #[test]
    fn managed_conf_is_loopback_only() {
        let text = render_managed_conf(Some(54329));
        assert!(text.contains("listen_addresses = '127.0.0.1'"));
        assert!(text.contains("port = 54329"));
        assert!(!text.contains("0.0.0.0"));
        assert!(!text.contains('*'));
        assert!(!render_managed_conf(None).contains("port ="));
    }

    #[test]
    fn percent_encoding_escapes_reserved_characters() {
        assert_eq!(percent_encode("abcXYZ019-._~"), "abcXYZ019-._~");
        assert_eq!(
            percent_encode("a:b@c/d?e#f g%"),
            "a%3Ab%40c%2Fd%3Fe%23f%20g%25"
        );
        assert_eq!(percent_encode("é"), "%C3%A9");
    }

    #[test]
    fn connection_url_needs_a_chosen_port_and_a_password() {
        let home = tempfile::tempdir().unwrap();
        let pg = manager(home.path());
        assert!(matches!(
            pg.connection_url("knowell"),
            Err(Error::NotRunning)
        ));

        State::new(54329).save(&pg.layout().state_file()).unwrap();
        assert!(matches!(
            pg.connection_url("knowell"),
            Err(Error::Password(_))
        ));

        let pg = pg.with_password_store(Arc::new(MemoryStore::default()));
        pg.passwords
            .set(&SecretString::from("KNOWELL_CANARY_a:b".to_string()))
            .unwrap();
        let url = pg.connection_url("knowell").unwrap();
        assert_eq!(
            url.expose_secret(),
            "postgresql://postgres:KNOWELL_CANARY_a%3Ab@127.0.0.1:54329/knowell"
        );
    }

    #[test]
    fn connection_url_rejects_bad_database_names() {
        let home = tempfile::tempdir().unwrap();
        let pg = manager(home.path());
        for bad in ["", "A", "a b", "a/b", "x?sslmode=disable", "a\"b"] {
            assert!(matches!(
                pg.connection_url(bad),
                Err(Error::InvalidIdentifier(_))
            ));
        }
    }

    #[test]
    fn debug_and_url_do_not_leak_the_password() {
        let home = tempfile::tempdir().unwrap();
        let pg = manager(home.path()).with_password_store(Arc::new(MemoryStore::default()));
        pg.passwords
            .set(&SecretString::from("KNOWELL_CANARY_pw".to_string()))
            .unwrap();
        State::new(54329).save(&pg.layout().state_file()).unwrap();
        let url = pg.connection_url("knowell").unwrap();
        let shown = format!("{pg:?} {url:?} {:?}", pg.passwords);
        assert!(!shown.contains("KNOWELL_CANARY_pw"), "{shown}");
        assert!(shown.contains("redacted"));
    }

    #[tokio::test]
    async fn status_is_not_installed_on_an_empty_home() {
        let home = tempfile::tempdir().unwrap();
        let pg = manager(home.path());
        assert_eq!(pg.status().await.unwrap(), Status::NotInstalled);
        assert!(matches!(
            pg.start().await,
            Err(Error::NotInstalled { major: 17 })
        ));
        assert!(matches!(
            pg.init_data_dir().await,
            Err(Error::NotInstalled { .. })
        ));
        // Stopping something that is not installed is a no-op.
        pg.stop().await.unwrap();
    }

    #[tokio::test]
    async fn status_is_installed_when_binaries_exist_but_no_cluster() {
        let home = tempfile::tempdir().unwrap();
        let pg = manager(home.path());
        std::fs::create_dir_all(pg.layout().dist_root().join("17.5.0").join("bin")).unwrap();
        assert_eq!(pg.status().await.unwrap(), Status::Installed);
        assert!(matches!(
            pg.start().await,
            Err(Error::MissingProgram { .. })
        ));
    }
}
