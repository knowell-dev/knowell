//! Error type for the managed PostgreSQL crate.
//!
//! Messages are lowercase, actionable and never contain the superuser password
//! or a connection URL.

use std::path::PathBuf;

/// Result alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong while managing the embedded PostgreSQL instance.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A filesystem operation failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted, including the path when useful.
        context: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },

    /// The supplied configuration is not usable.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    /// A database or extension name does not match the accepted pattern.
    #[error(
        "invalid identifier {0:?}: use 1-63 characters of a-z, 0-9 and underscore, not starting with a digit"
    )]
    InvalidIdentifier(String),

    /// The binaries for the configured major version are not installed.
    #[error("postgresql {major} is not installed; run install() first")]
    NotInstalled {
        /// Configured major version.
        major: u32,
    },

    /// The data directory has not been initialised.
    #[error("the data directory {0} is not initialised; run init_data_dir() first")]
    NotInitialized(PathBuf),

    /// The data directory is already in use by a running server.
    #[error("postgresql is already running (pid {pid}, port {port}); stop it first")]
    AlreadyRunning {
        /// Postmaster process id.
        pid: u32,
        /// Port the server listens on.
        port: u16,
    },

    /// A `postmaster.pid` exists but no such server is running.
    #[error(
        "stale postmaster.pid for pid {pid} in {path}; no server is running. \
         Confirm no other postgres uses this data directory, then call clear_stale_postmaster_pid()"
    )]
    StalePostmasterPid {
        /// Pid recorded in the file.
        pid: u32,
        /// Location of the pid file.
        path: PathBuf,
    },

    /// An operation needs a running server.
    #[error("postgresql is not running; start it first")]
    NotRunning,

    /// An operation needs a stopped server.
    #[error("postgresql must be stopped for this operation")]
    NotStopped,

    /// Downloading or unpacking the PostgreSQL distribution failed.
    #[error("could not obtain postgresql {major}: {message}")]
    Download {
        /// Requested major version.
        major: u32,
        /// Underlying failure, without secrets.
        message: String,
    },

    /// A PostgreSQL client or server program exited unsuccessfully.
    #[error("{program} failed (exit code {code}): {output}")]
    Command {
        /// Program name, e.g. `pg_dump`.
        program: String,
        /// Exit code, or `none` if terminated by a signal.
        code: String,
        /// Tail of the combined output, password redacted.
        output: String,
    },

    /// A program did not finish in time and was killed.
    #[error("{program} did not finish within {seconds} seconds and was stopped")]
    Timeout {
        /// Program name.
        program: String,
        /// The limit that was exceeded.
        seconds: u64,
    },

    /// A required program is missing from the distribution.
    #[error("{program} is not part of the postgresql distribution at {dist}")]
    MissingProgram {
        /// Program name.
        program: String,
        /// Distribution directory searched.
        dist: PathBuf,
    },

    /// The state file is unreadable or malformed.
    #[error("state file {path} is invalid: {message}")]
    State {
        /// State file location.
        path: PathBuf,
        /// What is wrong with it.
        message: String,
    },

    /// The password store failed or holds no password.
    #[error("password store: {0}")]
    Password(String),

    /// The pgvector bundle is missing files or is malformed.
    #[error("invalid extension bundle: {0}")]
    Bundle(String),

    /// A database that must not exist already does.
    #[error("database {0:?} already exists")]
    DatabaseExists(String),

    /// The requested upgrade cannot be performed.
    #[error("upgrade not possible: {0}")]
    Upgrade(String),

    /// `pg_upgrade` is not shipped in the distribution.
    #[error(
        "pg_upgrade is not available in the postgresql {major} distribution; upgrade manually with backup() and restore()"
    )]
    PgUpgradeUnavailable {
        /// Major version lacking the tool.
        major: u32,
    },
}

impl Error {
    /// Build an [`Error::Io`] with context.
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}
