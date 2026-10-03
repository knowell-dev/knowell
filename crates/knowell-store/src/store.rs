//! Connection pool, migrations and server checks.

use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use sqlx::pool::PoolConnection;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, Connection, PgConnection, PgPool, Postgres, Transaction};

use crate::error::{StoreError, scrub};
use crate::maintenance::{self, Maintenance};
use crate::migrations::SchemaIdentity;

/// Connection query parameters accepted in a database URL. Anything else is
/// rejected up front, because the driver would log unknown parameters
/// together with their values.
const KNOWN_PARAMETERS: &[&str] = &[
    "sslmode",
    "ssl-mode",
    "sslrootcert",
    "ssl-root-cert",
    "ssl-ca",
    "sslcert",
    "ssl-cert",
    "sslkey",
    "ssl-key",
    "statement-cache-capacity",
    "host",
    "hostaddr",
    "port",
    "dbname",
    "user",
    "password",
    "application_name",
    "options",
];

/// Pool settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreOptions {
    /// Maximum number of pooled connections.
    pub max_connections: u32,
    /// Connections kept open even when idle.
    pub min_connections: u32,
    /// How long to wait for a connection (also bounds the initial connect).
    pub acquire_timeout: Duration,
    /// Close connections idle for longer than this.
    pub idle_timeout: Option<Duration>,
    /// Recycle connections older than this.
    pub max_lifetime: Option<Duration>,
    /// `application_name` reported to the server (visible in `pg_stat_activity`).
    pub application_name: String,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            max_connections: 10,
            min_connections: 0,
            acquire_timeout: Duration::from_secs(30),
            idle_timeout: Some(Duration::from_secs(600)),
            max_lifetime: Some(Duration::from_secs(1800)),
            application_name: "knowell".to_owned(),
        }
    }
}

/// Handle to the database: a connection pool plus schema management.
///
/// Repository functions take `&mut PgConnection`; get one with
/// [`Store::acquire`] or [`Store::begin`] (a transaction derefs to a
/// connection, so the same functions compose inside caller transactions).
#[derive(Clone)]
pub struct Store {
    pool: PgPool,
    runtime: bool,
    admission_error: Arc<Mutex<Option<StoreError>>>,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The pool's own Debug output includes connect options; keep it out.
        f.debug_struct("Store")
            .field("size", &self.pool.size())
            .field("runtime", &self.runtime)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Opens an administrative/bootstrap pool without runtime admission locks.
    ///
    /// The URL may contain a password, so it is held as a secret, and no
    /// error returned from here contains it (or the password). Runtime engines
    /// must use [`Self::connect_runtime`]; raw administrator access is not fenced.
    pub async fn connect(url: &SecretString, options: &StoreOptions) -> Result<Self, StoreError> {
        Self::connect_url(url, options, false).await
    }

    /// Opens an exact-schema runtime pool fenced against cooperative maintenance.
    ///
    /// Every physical pooled connection, including reconnects, takes a session
    /// shared gate before schema validation. Pending maintenance is reported
    /// explicitly; startup never migrates or waits indefinitely for a gate.
    pub async fn connect_runtime(
        url: &SecretString,
        options: &StoreOptions,
    ) -> Result<Self, StoreError> {
        Self::connect_url(url, options, true).await
    }

    async fn connect_url(
        url: &SecretString,
        options: &StoreOptions,
        runtime: bool,
    ) -> Result<Self, StoreError> {
        let url = url.expose_secret();
        let connect = parse_url(url)?;
        let password = url_password(url);
        let decoded = password.as_deref().map(percent_decode);
        let mut secrets = vec![url];
        if let Some(p) = password.as_deref() {
            secrets.push(p);
        }
        if let Some(p) = decoded.as_deref() {
            secrets.push(p);
        }
        Self::open(connect, options, &secrets, runtime).await
    }

    /// Opens an administrative/bootstrap pool with already-built options.
    ///
    /// This low-level handle does not participate in runtime fencing. Use
    /// [`Self::connect_runtime_with`] for engine data access.
    pub async fn connect_with(
        connect: PgConnectOptions,
        options: &StoreOptions,
    ) -> Result<Self, StoreError> {
        Self::open(connect, options, &[], false).await
    }

    /// Opens a fenced runtime pool with already-built connection options.
    pub async fn connect_runtime_with(
        connect: PgConnectOptions,
        options: &StoreOptions,
    ) -> Result<Self, StoreError> {
        Self::open(connect, options, &[], true).await
    }

    async fn open(
        connect: PgConnectOptions,
        options: &StoreOptions,
        secrets: &[&str],
        runtime: bool,
    ) -> Result<Self, StoreError> {
        let connect = connect.application_name(&options.application_name);
        let failed = |err: sqlx::Error| StoreError::Connect(scrub(&err.to_string(), secrets));
        // One direct connection first: a pool retries refused connections
        // until its timeout and then reports only "timed out", while the
        // direct attempt surfaces the real cause (refused, auth, missing db).
        match tokio::time::timeout(options.acquire_timeout, connect.connect()).await {
            Ok(Ok(mut conn)) => {
                if runtime {
                    tokio::time::timeout(options.acquire_timeout, maintenance::admit(&mut conn))
                        .await
                        .map_err(|_| StoreError::MaintenanceTimeout)??;
                }
                let _ = conn.close().await;
            }
            Ok(Err(err)) => return Err(failed(err)),
            Err(_) => {
                return Err(StoreError::Connect(format!(
                    "no answer within {} ms",
                    options.acquire_timeout.as_millis()
                )));
            }
        }
        let admission_error: Arc<Mutex<Option<StoreError>>> = Arc::new(Mutex::new(None));
        let mut pool_options = PgPoolOptions::new()
            .max_connections(options.max_connections)
            .min_connections(options.min_connections)
            .acquire_timeout(options.acquire_timeout)
            .idle_timeout(options.idle_timeout)
            .max_lifetime(options.max_lifetime);
        if runtime {
            let failure = Arc::clone(&admission_error);
            pool_options = pool_options.after_connect(move |conn, _| {
                let failure = Arc::clone(&failure);
                Box::pin(async move {
                    match maintenance::admit(conn).await {
                        Ok(()) => {
                            failure
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .take();
                            Ok(())
                        }
                        Err(error) => {
                            // SQLx retries rejected connections. Retain the
                            // typed refusal so maintenance is never mistaken for
                            // an unavailable optional database.
                            *failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
                            Err(sqlx::Error::Protocol(
                                "database runtime admission refused".into(),
                            ))
                        }
                    }
                })
            });
        }
        let pool = pool_options.connect_with(connect).await.map_err(|error| {
            admission_error
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .unwrap_or_else(|| failed(error))
        })?;
        Ok(Self {
            pool,
            runtime,
            admission_error,
        })
    }

    /// Wraps an administrative/legacy pool without installing runtime hooks.
    ///
    /// The caller owns its safety. Engine entry points must use the runtime
    /// constructors; this adapter cannot retroactively fence existing sessions.
    pub fn from_pool(pool: PgPool) -> Self {
        Self {
            pool,
            runtime: false,
            admission_error: Arc::new(Mutex::new(None)),
        }
    }

    /// The underlying pool; runtime constructors install connection-level hooks.
    ///
    /// Direct access bypasses the per-operation maintenance-intent check but
    /// retains physical session locks, so exclusive migration still waits for
    /// pool closure. Prefer [`Self::acquire`] and [`Self::begin`].
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Takes a connection from the pool.
    pub async fn acquire(&self) -> Result<PoolConnection<Postgres>, StoreError> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|error| self.pool_error(error))?;
        if self.runtime
            && let Err(error) = maintenance::check_open(&mut conn).await
        {
            conn.close_on_drop();
            return Err(error);
        }
        Ok(conn)
    }

    /// Starts a transaction on a pooled connection.
    pub async fn begin(&self) -> Result<Transaction<'static, Postgres>, StoreError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| self.pool_error(error))?;
        if self.runtime {
            maintenance::check_open(&mut tx).await?;
        }
        Ok(tx)
    }

    /// Explicit administrative migration under the database-wide exclusive gate.
    ///
    /// Concurrent administrator calls serialize within the acquire timeout.
    /// Runtime pools refuse this method. Cancellation after maintenance intent
    /// was persisted leaves its UUID recorded for explicit recovery.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        if self.runtime {
            return Err(StoreError::RuntimeMigrationForbidden);
        }
        let timeout = self.pool.options().get_acquire_timeout();
        let conn = self.pool.acquire().await?.detach();
        let mut maintenance = Maintenance::start(conn, uuid::Uuid::now_v7(), timeout, true).await?;
        maintenance.acquire_exclusive(timeout).await?;
        maintenance.migrate().await?;
        maintenance.finish().await
    }

    /// Exact numbered domain schema and admission protocol required by this build.
    pub const fn latest_schema() -> SchemaIdentity {
        SchemaIdentity::current()
    }

    /// Checks immutable migration history without applying migrations or DDL.
    pub async fn validate_schema(&self) -> Result<SchemaIdentity, StoreError> {
        let mut conn = self.acquire().await?;
        crate::migrations::validate(&mut conn).await
    }

    /// Inspects known history without DDL, accepting missing known migrations.
    ///
    /// Uninitialized/empty history has version zero; corrupt, dirty and newer
    /// histories remain errors. This does not establish runtime compatibility.
    pub async fn inspect_schema(&self) -> Result<SchemaIdentity, StoreError> {
        let mut conn = self.acquire().await?;
        crate::migrations::inspect(&mut conn).await
    }

    /// Returns a persisted maintenance operation, without normal data admission.
    ///
    /// Runtime handles use a dedicated, read-only control connection so this
    /// remains available when every pooled session is checked out or replacement
    /// runtime connections are refused. This never admits a data connection;
    /// administrator handles may use it for explicit recovery.
    pub async fn maintenance_owner(&self) -> Result<Option<uuid::Uuid>, StoreError> {
        if self.runtime {
            let timeout = self.pool.options().get_acquire_timeout();
            let connect = self.pool.connect_options();
            return tokio::time::timeout(timeout, async {
                // A control query may bypass data admission, but connection
                // options and driver errors must never expose its credentials.
                let mut conn = connect.connect().await.map_err(|_| {
                    StoreError::Connect("cannot open the maintenance status connection".into())
                })?;
                let result = maintenance::owner_on(&mut conn).await;
                let _ = conn.close().await;
                result
            })
            .await
            .map_err(|_| StoreError::MaintenanceTimeout)?;
        }
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|error| self.pool_error(error))?;
        maintenance::owner_on(&mut conn).await
    }

    /// Begins or resumes `owner` on a detached administrator control connection.
    ///
    /// The coordinator lock is tried once; other owners fail immediately. This
    /// persists admission intent but does not yet wait for runtime sessions.
    pub async fn begin_maintenance(&self, owner: uuid::Uuid) -> Result<Maintenance, StoreError> {
        if self.runtime {
            return Err(StoreError::RuntimeMigrationForbidden);
        }
        let conn = self.pool.acquire().await?.detach();
        Maintenance::start(
            conn,
            owner,
            self.pool.options().get_acquire_timeout(),
            false,
        )
        .await
    }

    fn pool_error(&self, error: sqlx::Error) -> StoreError {
        self.admission_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .unwrap_or(StoreError::Database(error))
    }

    /// Reports the server version and the `vector` extension's availability
    /// and version. Works before migrations have run (`know doctor`).
    pub async fn check_server(&self) -> Result<ServerInfo, StoreError> {
        let mut conn = self.acquire().await?;
        check_server_on(&mut conn).await
    }

    /// Closes every pooled connection and waits for them to finish.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

pub(crate) async fn check_server_on(conn: &mut PgConnection) -> Result<ServerInfo, StoreError> {
    let (server_version, server_version_num): (String, String) = sqlx::query_as(
        "SELECT current_setting('server_version'), current_setting('server_version_num')",
    )
    .fetch_one(&mut *conn)
    .await?;
    let server_version_num = server_version_num.parse::<u32>().map_err(|_| {
        StoreError::Corrupt("server reported a non-numeric server_version_num".to_owned())
    })?;
    let vector: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT default_version, installed_version FROM pg_available_extensions WHERE name = 'vector'",
        )
        .fetch_optional(&mut *conn)
        .await?;
    Ok(ServerInfo {
        server_version,
        server_version_num,
        vector: vector.map(|(default_version, installed_version)| VectorExtension {
            default_version,
            installed_version,
        }),
    })
}

/// Server facts reported by [`Store::check_server`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    /// `server_version`, e.g. `17.11 (Debian 17.11-1.pgdg12+2)`.
    pub server_version: String,
    /// `server_version_num`, e.g. `170011`.
    pub server_version_num: u32,
    /// The pgvector extension, if the server has it available.
    pub vector: Option<VectorExtension>,
}

/// Availability of the `vector` extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorExtension {
    /// Version `CREATE EXTENSION vector` would install.
    pub default_version: String,
    /// Version installed in the current database, if any.
    pub installed_version: Option<String>,
}

/// A reason core storage or optional semantic search is unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ServerIssue {
    /// PostgreSQL is older than 17.
    ServerTooOld {
        /// `server_version` as reported.
        found: String,
    },
    /// The `vector` extension is not available on the server.
    VectorMissing,
    /// The `vector` extension is older than 0.8.0 (needed for iterative
    /// index scans; `halfvec` needs 0.7.0).
    VectorTooOld {
        /// Installed version, or the available one if not installed yet.
        found: String,
    },
}

impl fmt::Display for ServerIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServerIssue::ServerTooOld { found } => {
                write!(f, "postgresql {found} is not supported; use postgresql 17 or 18")
            }
            ServerIssue::VectorMissing => f.write_str(
                "the pgvector extension is not available on the server; install pgvector 0.8 or newer",
            ),
            ServerIssue::VectorTooOld { found } => {
                write!(f, "pgvector {found} is too old; upgrade to 0.8 or newer")
            }
        }
    }
}

impl ServerInfo {
    /// Oldest supported `server_version_num` (PostgreSQL 17).
    pub const MIN_SERVER_VERSION_NUM: u32 = 170_000;
    /// Oldest supported pgvector version.
    pub const MIN_VECTOR_VERSION: (u32, u32, u32) = (0, 8, 0);

    /// Problems preventing full functionality, including optional semantic
    /// search. Missing or old pgvector does not prevent core storage.
    pub fn issues(&self) -> Vec<ServerIssue> {
        let mut issues = Vec::new();
        if self.server_version_num < Self::MIN_SERVER_VERSION_NUM {
            issues.push(ServerIssue::ServerTooOld {
                found: self.server_version.clone(),
            });
        }
        match &self.vector {
            None => issues.push(ServerIssue::VectorMissing),
            Some(ext) => {
                let found = ext
                    .installed_version
                    .as_deref()
                    .unwrap_or(&ext.default_version);
                let new_enough =
                    parse_version(found).is_some_and(|v| v >= Self::MIN_VECTOR_VERSION);
                if !new_enough {
                    issues.push(ServerIssue::VectorTooOld {
                        found: found.to_owned(),
                    });
                }
            }
        }
        issues
    }

    /// Whether [`ServerInfo::issues`] is empty.
    pub fn is_supported(&self) -> bool {
        self.issues().is_empty()
    }

    /// Whether the server supports core storage without semantic search.
    pub fn supports_core(&self) -> bool {
        self.server_version_num >= Self::MIN_SERVER_VERSION_NUM
    }

    /// Whether a supported pgvector version is installed in this database.
    pub fn semantic_enabled(&self) -> bool {
        self.vector.as_ref().is_some_and(|ext| {
            ext.installed_version.as_deref().is_some_and(|version| {
                parse_version(version).is_some_and(|v| v >= Self::MIN_VECTOR_VERSION)
            })
        })
    }
}

/// Parses `major.minor[.patch]`.
pub(crate) fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = match parts.next() {
        Some(p) => p.parse().ok()?,
        None => 0,
    };
    Some((major, minor, patch))
}

/// Validates and parses a database URL without ever echoing it.
fn parse_url(url: &str) -> Result<PgConnectOptions, StoreError> {
    const EXPECTED: &str =
        "expected postgres://[user[:password]@]host[:port]/database[?parameters]";
    if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
        return Err(StoreError::InvalidUrl(EXPECTED));
    }
    if let Some((_, query)) = url.split_once('?') {
        let query = query.split('#').next().unwrap_or_default();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let key = pair.split('=').next().unwrap_or_default();
            let known = KNOWN_PARAMETERS.contains(&key) || key.starts_with("options[");
            if !known {
                return Err(StoreError::InvalidUrl(
                    "unsupported connection parameter; supported: sslmode, sslrootcert, sslcert, sslkey, host, hostaddr, port, dbname, user, password, application_name, options, statement-cache-capacity",
                ));
            }
        }
    }
    PgConnectOptions::from_str(url).map_err(|_| StoreError::InvalidUrl(EXPECTED))
}

/// The raw (still percent-encoded) password of a URL's authority or of a
/// `password=` query parameter, for scrubbing error messages.
fn url_password(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r)?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if let Some((userinfo, _)) = authority.rsplit_once('@')
        && let Some((_, password)) = userinfo.split_once(':')
        && !password.is_empty()
    {
        return Some(password.to_owned());
    }
    let (_, query) = url.split_once('?')?;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("password="))
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
}

/// Decodes `%XX` escapes; malformed escapes are kept as they are.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%'
            && let (Some(hi), Some(lo)) = (bytes.get(i + 1), bytes.get(i + 2))
            && let (Some(hi), Some(lo)) = (hex_digit(*hi), hex_digit(*lo))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(b);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANARY: &str = "KNOWELL_CANARY_pw_7f3a";

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("0.8.7"), Some((0, 8, 7)));
        assert_eq!(parse_version("0.8"), Some((0, 8, 0)));
        assert_eq!(parse_version("x"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn reports_server_issues() {
        let mut info = ServerInfo {
            server_version: "17.2".into(),
            server_version_num: 170_002,
            vector: Some(VectorExtension {
                default_version: "0.8.0".into(),
                installed_version: None,
            }),
        };
        assert!(info.is_supported());
        info.server_version_num = 160_004;
        info.vector = Some(VectorExtension {
            default_version: "0.9.0".into(),
            installed_version: Some("0.7.4".into()),
        });
        let issues = info.issues();
        assert_eq!(issues.len(), 2);
        assert!(issues[0].to_string().contains("postgresql 17 or 18"));
        assert!(issues[1].to_string().contains("0.7.4"));
        info.vector = None;
        assert!(info.issues().contains(&ServerIssue::VectorMissing));
    }

    #[test]
    fn extracts_passwords_for_scrubbing() {
        let url = format!("postgres://u:{CANARY}@h:5432/db");
        assert_eq!(url_password(&url).as_deref(), Some(CANARY));
        let url = format!("postgres://h/db?sslmode=disable&password={CANARY}");
        assert_eq!(url_password(&url).as_deref(), Some(CANARY));
        assert_eq!(url_password("postgres://u@h/db"), None);
        assert_eq!(percent_decode("a%40b%2"), "a@b%2");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn url_errors_never_echo_the_url() {
        let cases = [
            format!("mysql://u:{CANARY}@h/db"),
            format!("postgres://u:{CANARY}@h:notaport/db"),
            format!("postgres://u@h/db?passwd={CANARY}"),
            format!("postgres://u@h/db?{CANARY}"),
            format!("postgres://u:{CANARY}@h/db?sslmode={CANARY}"),
        ];
        for url in cases {
            let err = parse_url(&url).unwrap_err();
            assert!(matches!(err, StoreError::InvalidUrl(_)), "{url}");
            for text in [err.to_string(), format!("{err:?}")] {
                assert!(!text.contains(CANARY), "leaked in: {text}");
            }
        }
        assert!(parse_url("postgres://u:p@localhost:5432/db?sslmode=disable").is_ok());
        assert!(parse_url("postgresql://localhost/db?options[search_path]=x").is_ok());
    }
}
