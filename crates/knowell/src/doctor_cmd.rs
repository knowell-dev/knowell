//! `know doctor`: installation checks with remediation.
//!
//! Never prints a secret: database URLs are resolved only to connect (errors
//! are scrubbed by `knowell-store`), and for provider keys and tokens only
//! whether the referenced variable or file exists is reported.

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use clap::Args;
use knowell_config::{DatabaseMode, EngineConfig, ServerRole};
use knowell_core::SecretRef;
use knowell_pg_managed::Status as PgStatus;
use knowell_setup::{Client, ConnectOptions, Scope};
use knowell_store::{ServerInfo, Store, StoreOptions};
use serde::Serialize;

use crate::connect_cmd;
use crate::db::{self, DATABASE_NAME};
use crate::env::{self, Env};
use crate::output::Output;

#[derive(Debug, Args)]
pub(crate) struct DoctorArgs {
    /// Print the report as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Serialize)]
struct Check {
    name: &'static str,
    status: Status,
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    remediation: Option<String>,
}

impl Check {
    fn ok(name: &'static str, summary: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Ok,
            summary: summary.into(),
            remediation: None,
        }
    }

    fn warn(name: &'static str, summary: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Warn,
            summary: summary.into(),
            remediation: Some(fix.into()),
        }
    }

    fn fail(name: &'static str, summary: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Fail,
            summary: summary.into(),
            remediation: Some(fix.into()),
        }
    }
}

/// How long a database check may wait (compose health checks allow 10 s).
const DB_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn run(args: DoctorArgs, env: &Env, out: &mut Output) -> anyhow::Result<ExitCode> {
    let rt = db::runtime()?;
    let mut checks = Vec::new();

    let engine = engine_check(env, &mut checks);
    workspace_check(env, engine.as_ref(), &mut checks);
    match &engine {
        Some(cfg) => rt.block_on(database_checks(env, cfg, &mut checks)),
        None => checks.push(Check::warn(
            "database",
            "not checked: no valid engine configuration",
            "fix the engine configuration first",
        )),
    }
    checks.push(git_check());
    checks.push(rt.block_on(panel_check()));
    if let Some(cfg) = &engine {
        checks.push(providers_check(cfg));
        if cfg.server.role == ServerRole::Edge {
            checks.push(hub_check(cfg));
        }
    }
    checks.push(agents_check(env));

    let worst = checks.iter().map(|c| c.status).max().unwrap_or(Status::Ok);
    if args.json {
        let value = serde_json::json!({ "status": worst, "checks": checks });
        out.line(serde_json::to_string_pretty(&value)?)?;
    } else {
        for check in &checks {
            let tag = match check.status {
                Status::Ok => "[ ok ]",
                Status::Warn => "[warn]",
                Status::Fail => "[FAIL]",
            };
            out.line(format!("{tag} {:<17} {}", check.name, check.summary))?;
            if let Some(fix) = &check.remediation {
                out.line(format!("       {:<17} -> {fix}", ""))?;
            }
        }
        let failed = checks.iter().filter(|c| c.status == Status::Fail).count();
        let warned = checks.iter().filter(|c| c.status == Status::Warn).count();
        out.line(format!("{failed} failed, {warned} warning(s)"))?;
    }
    out.flush()?;
    Ok(if worst == Status::Fail {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn engine_check(env: &Env, checks: &mut Vec<Check>) -> Option<EngineConfig> {
    let path = env.engine_config.display();
    match env.load_engine() {
        Ok(Some(cfg)) => {
            checks.push(Check::ok(
                "engine_config",
                format!(
                    "{path} (role {}, listen {}, database {})",
                    cfg.server.role.as_str(),
                    cfg.server.listen,
                    match cfg.database.mode {
                        DatabaseMode::Managed => "managed",
                        DatabaseMode::External => "external",
                    }
                ),
            ));
            Some(cfg)
        }
        Ok(None) => {
            checks.push(Check::fail(
                "engine_config",
                format!("no engine configuration at {path}"),
                "run `know init`",
            ));
            None
        }
        // ConfigError never quotes configuration values.
        Err(err) => {
            checks.push(Check::fail(
                "engine_config",
                err.to_string(),
                "fix the file (`know config schema engine` prints its schema)",
            ));
            None
        }
    }
}

fn workspace_check(env: &Env, engine: Option<&EngineConfig>, checks: &mut Vec<Check>) {
    const NAME: &str = "workspace_config";
    let file = match env.find_workspace() {
        Ok(Some(file)) => file,
        Ok(None) => {
            checks.push(Check::warn(
                NAME,
                "no knowell.toml in this directory or above",
                "run `know workspace import` in the folder holding your repositories",
            ));
            return;
        }
        Err(err) => {
            checks.push(Check::fail(
                NAME,
                format!("{err:#}"),
                "pass an existing --workspace file",
            ));
            return;
        }
    };
    let config = match knowell_config::load_workspace(&file) {
        Ok(config) => config,
        Err(err) => {
            checks.push(Check::fail(
                NAME,
                err.to_string(),
                format!("fix {} (`know config check` shows details)", file.display()),
            ));
            return;
        }
    };
    let base = match env::parent_dir(&file) {
        Ok(base) => base,
        Err(err) => {
            checks.push(Check::fail(NAME, format!("{err:#}"), "check the path"));
            return;
        }
    };
    let resolved = match config.resolve(&base) {
        Ok(resolved) => resolved,
        Err(issues) => {
            checks.push(Check::fail(
                NAME,
                format!("{}: {issues}", file.display()),
                "fix the listed settings (`know config check` shows details)",
            ));
            return;
        }
    };
    if let Some(engine) = engine {
        let issues = resolved.check_against(engine);
        if !issues.is_empty() {
            let text: Vec<String> = issues.iter().map(ToString::to_string).collect();
            checks.push(Check::fail(
                NAME,
                format!("{}: {}", file.display(), text.join("; ")),
                "define the providers the workspace uses in the engine configuration, or change the workspace",
            ));
            return;
        }
    }
    let missing: Vec<String> = resolved
        .projects
        .iter()
        .filter(|p| !p.path.is_dir())
        .map(|p| p.name.to_string())
        .collect();
    if missing.is_empty() {
        checks.push(Check::ok(
            NAME,
            format!(
                "{}: workspace `{}`, {} project(s)",
                file.display(),
                resolved.name,
                resolved.projects.len()
            ),
        ));
    } else {
        checks.push(Check::warn(
            NAME,
            format!("project directories missing: {}", missing.join(", ")),
            "clone them or fix their `path`",
        ));
    }
}

async fn database_checks(env: &Env, cfg: &EngineConfig, checks: &mut Vec<Check>) {
    const DB: &str = "database";
    let url = match cfg.database.mode {
        DatabaseMode::Managed => {
            let pg = match db::managed(env, cfg) {
                Ok(pg) => pg,
                Err(err) => {
                    checks.push(Check::fail(
                        "managed_postgres",
                        format!("{err:#}"),
                        "fix the engine configuration",
                    ));
                    return;
                }
            };
            let status = match pg.status().await {
                Ok(status) => status,
                Err(err) => {
                    checks.push(Check::fail(
                        "managed_postgres",
                        format!("cannot read the status: {err}"),
                        format!("see {}", pg.layout().log_file().display()),
                    ));
                    return;
                }
            };
            let not_running = |checks: &mut Vec<Check>| {
                checks.push(Check::warn(
                    DB,
                    "not checked: the managed PostgreSQL is not running",
                    "start it with `know serve` (or `know init`)",
                ));
            };
            match status {
                PgStatus::NotInstalled => {
                    checks.push(Check::fail(
                        "managed_postgres",
                        format!("PostgreSQL {} is not installed", pg.major()),
                        "run `know init`",
                    ));
                    return;
                }
                PgStatus::Installed => {
                    checks.push(Check::fail(
                        "managed_postgres",
                        "installed but the data directory is not initialised",
                        "run `know init`",
                    ));
                    return;
                }
                PgStatus::Stopped => {
                    checks.push(Check::warn(
                        "managed_postgres",
                        format!("PostgreSQL {} is stopped", pg.major()),
                        "`know serve` starts it",
                    ));
                    not_running(checks);
                    return;
                }
                PgStatus::StalePostmasterPid { pid } => {
                    checks.push(Check::warn(
                        "managed_postgres",
                        format!("not running; a stale pid file of process {pid} remains"),
                        "`know serve` removes the stale file and starts it",
                    ));
                    not_running(checks);
                    return;
                }
                PgStatus::Running { pid, port } => checks.push(Check::ok(
                    "managed_postgres",
                    format!(
                        "PostgreSQL {} running on 127.0.0.1:{port} (pid {pid})",
                        pg.major()
                    ),
                )),
            }
            match pg.connection_url(DATABASE_NAME) {
                Ok(url) => url,
                Err(err) => {
                    checks.push(Check::fail(DB, format!("{err}"), "run `know init`"));
                    return;
                }
            }
        }
        DatabaseMode::External => {
            let Some(reference) = &cfg.database.url else {
                checks.push(Check::fail(
                    DB,
                    "`database.url` is missing",
                    "run `know init --database external --database-url-ref env:NAME`",
                ));
                return;
            };
            match knowell_secrets::resolve(reference) {
                Ok(url) => url,
                // Names the reference, never a value.
                Err(err) => {
                    checks.push(Check::fail(
                        DB,
                        err.to_string(),
                        format!(
                            "provide the connection URL through {}",
                            reference.describe()
                        ),
                    ));
                    return;
                }
            }
        }
    };

    let options = StoreOptions {
        max_connections: 1,
        acquire_timeout: DB_TIMEOUT,
        application_name: "know doctor".to_owned(),
        ..StoreOptions::default()
    };
    let store = match Store::connect(&url, &options).await {
        Ok(store) => store,
        Err(err) => {
            // StoreError is scrubbed of the URL and its password.
            checks.push(Check::fail(
                DB,
                err.to_string(),
                "check that the server runs and the URL is right (it is never printed)",
            ));
            return;
        }
    };
    let info = store.check_server().await;
    store.close().await;
    let info = match info {
        Ok(info) => info,
        Err(err) => {
            checks.push(Check::fail(DB, err.to_string(), "check the server"));
            return;
        }
    };
    if info.server_version_num < ServerInfo::MIN_SERVER_VERSION_NUM {
        checks.push(Check::fail(
            DB,
            format!("PostgreSQL {} is not supported", info.server_version),
            "use PostgreSQL 17 or 18",
        ));
    } else {
        checks.push(Check::ok(
            DB,
            format!("reachable, PostgreSQL {}", info.server_version),
        ));
    }
    checks.push(pgvector_check(&info, cfg.database.mode));
}

fn pgvector_check(info: &ServerInfo, mode: DatabaseMode) -> Check {
    const NAME: &str = "pgvector";
    let Some(vector) = &info.vector else {
        let fix = match mode {
            DatabaseMode::Managed => {
                "unpack the pgvector bundle of the Knowell release to $KNOWELL_HOME/pgvector/pg<major> and run `know init`"
            }
            DatabaseMode::External => {
                "install pgvector 0.8 or newer on the server and run `know init`"
            }
        };
        return Check::warn(
            NAME,
            "pgvector is unavailable; semantic search is disabled, core storage remains available",
            fix,
        );
    };
    let too_old = info
        .issues()
        .into_iter()
        .any(|i| matches!(i, knowell_store::ServerIssue::VectorTooOld { .. }));
    let version = vector
        .installed_version
        .as_deref()
        .unwrap_or(&vector.default_version);
    if too_old {
        return Check::warn(
            NAME,
            format!("pgvector {version} is too old; semantic search is disabled"),
            "upgrade pgvector to 0.8 or newer and run `know init`",
        );
    }
    match &vector.installed_version {
        Some(installed) => Check::ok(NAME, format!("pgvector {installed} installed")),
        None => Check::warn(
            NAME,
            format!(
                "pgvector {} available but not installed in database `{DATABASE_NAME}`: semantic search is disabled",
                vector.default_version
            ),
            "run `know init`",
        ),
    }
}

fn git_check() -> Check {
    match std::process::Command::new("git").arg("--version").output() {
        Ok(output) if output.status.success() => Check::ok(
            "git",
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ),
        Ok(_) | Err(_) => Check::warn(
            "git",
            "git is not available on PATH",
            "install git (history, blame and fixtures use it)",
        ),
    }
}

/// Requests `/` from an in-process router to see whether the real panel or
/// the placeholder page was embedded at build time.
async fn panel_check() -> Check {
    use tower::ServiceExt as _;
    const NAME: &str = "panel";
    let probe = async {
        let organization = knowell_core::Name::new("local")?;
        let config = knowell_server::ServerConfig::new(
            ServerRole::Standalone,
            std::net::SocketAddr::from(([127, 0, 0, 1], 7420)),
            organization,
        );
        let state = knowell_server::AppState::builder(config)
            .build()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let router = knowell_server::build_router(state);
        let request = axum::http::Request::builder()
            .uri("/")
            .header("host", "127.0.0.1:7420")
            .body(axum::body::Body::empty())?;
        let response = router.oneshot(request).await?;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
        anyhow::Ok((status, String::from_utf8_lossy(&body).into_owned()))
    };
    match probe.await {
        Ok((status, body)) if status.is_success() && body.contains("panel not built") => {
            Check::warn(
                NAME,
                "this build embeds the placeholder page, not the panel",
                "build the panel (`npm --prefix panel run build`) and rebuild knowell-server",
            )
        }
        Ok((status, _)) if status.is_success() => Check::ok(NAME, "embedded"),
        Ok((status, _)) => Check::warn(
            NAME,
            format!("the panel answered {status}"),
            "rebuild the binary",
        ),
        Err(err) => Check::warn(
            NAME,
            format!("cannot probe the panel: {err:#}"),
            "rebuild the binary",
        ),
    }
}

/// Whether a secret reference points at something that exists. The value is
/// never read for display.
fn reference_exists(reference: &SecretRef) -> bool {
    match reference {
        SecretRef::Env(name) => std::env::var_os(name).is_some_and(|v| !v.is_empty()),
        SecretRef::File(path) => Path::new(path).is_file(),
    }
}

fn providers_check(cfg: &EngineConfig) -> Check {
    const NAME: &str = "providers";
    if cfg.providers.is_empty() {
        return Check::ok(NAME, "none configured: lexical and graph search only");
    }
    let mut present = Vec::new();
    let mut missing = Vec::new();
    for (name, provider) in &cfg.providers {
        match &provider.api_key {
            Some(reference) if !reference_exists(reference) => {
                missing.push(format!("{name} ({})", reference.describe()));
            }
            _ => present.push(format!("{name} ({})", provider.kind.as_str())),
        }
    }
    if missing.is_empty() {
        Check::ok(NAME, present.join(", "))
    } else {
        Check::fail(
            NAME,
            format!("API key reference not set: {}", missing.join(", ")),
            "set the referenced environment variable or create the file (values are never shown)",
        )
    }
}

fn hub_check(cfg: &EngineConfig) -> Check {
    const NAME: &str = "hub";
    let Some(hub) = &cfg.hub else {
        return Check::fail(
            NAME,
            "role edge without a hub",
            "run `know login <hub-url>`",
        );
    };
    if reference_exists(&hub.token) {
        Check::ok(
            NAME,
            format!("{} (token {})", hub.url, hub.token.describe()),
        )
    } else {
        Check::fail(
            NAME,
            format!("token reference {} is not set", hub.token.describe()),
            "set it to a token issued by the hub (the value is never shown)",
        )
    }
}

/// An agent client counts as connected when a dry-run `connect` would change
/// nothing (the entry, instructions and hook are all in place).
fn agents_check(env: &Env) -> Check {
    const NAME: &str = "agent_clients";
    let (home, project) = match (env::user_home(), connect_cmd::project_dir(None, env)) {
        (Ok(home), Ok(project)) => (home, project),
        (Err(err), _) | (_, Err(err)) => {
            return Check::warn(NAME, format!("not checked: {err:#}"), "set HOME");
        }
    };
    let mut launcher = ConnectOptions::new(&home, &project);
    if let Err(err) = connect_cmd::configure_launcher(env, &mut launcher) {
        return Check::warn(
            NAME,
            format!("not checked: {err:#}"),
            "check the selected engine and workspace paths",
        );
    }
    let mut connected = Vec::new();
    let mut problems = Vec::new();
    for client in [Client::Claude, Client::Codex, Client::Cursor] {
        for (scope, label) in [(Scope::Project, "project"), (Scope::User, "user")] {
            let mut opts = launcher.clone();
            opts.scope = scope;
            opts.dry_run = true;
            match knowell_setup::connect(client, &opts) {
                Ok(report) if report.changed_files.is_empty() => {
                    connected.push(format!("{} ({label})", client.as_str()));
                }
                Ok(_) => {}
                Err(err) => problems.push(format!("{} ({label}): {err}", client.as_str())),
            }
        }
    }
    if !problems.is_empty() {
        return Check::warn(
            NAME,
            problems.join("; "),
            "fix the client configuration files named above",
        );
    }
    if connected.is_empty() {
        Check::warn(
            NAME,
            "no agent client has the current source-output connection settings",
            "run or repeat `know connect claude` (or codex, cursor)",
        )
    } else {
        Check::ok(
            NAME,
            format!("source-output settings ready: {}", connected.join(", ")),
        )
    }
}
