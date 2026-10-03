//! `know serve`: REST API, panel and MCP over Streamable HTTP.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Args, ValueEnum};
use knowell_auth::{Grant, Principal as AuthPrincipal, ResourceScope, Role, UserId};
use knowell_config::{EngineConfig, ServerRole};
use knowell_core::Name;
use knowell_mcp::{HttpServerOptions, KnowellServer};
use knowell_server::{AppState, EventBus, MemoryTokenStore, PanelMode, ServerConfig, TokenStore};
use knowell_store::identity::{self, GrantScope, NewGrant, NewPrincipal};
use knowell_store::{GrantRole, PrincipalId, PrincipalKind, Store, hierarchy};

use crate::db;
use crate::env::Env;
use crate::output::Output;
use crate::registry::Registry;
use crate::tools::{self, EngineDeps, NOT_WIRED};
use tokio_util::sync::CancellationToken;

/// Identity of the machine's own user on non-hub roles: the loopback panel
/// acts as this user, who administers the local organization.
pub(crate) const LOCAL_USER: u128 = 0x6b6e_6f77_656c_6c2d_6c6f_6361_6c00_0001;
/// Login name of that user in the database.
const LOCAL_USER_NAME: &str = "local";

/// Environment variable read when `--public-url` is not given.
const PUBLIC_URL_ENV: &str = "KNOWELL_PUBLIC_URL";
/// Environment variable (comma-separated) added to `--allow-host`.
const ALLOWED_HOSTS_ENV: &str = "KNOWELL_ALLOWED_HOSTS";

#[derive(Debug, Args)]
pub(crate) struct ServeArgs {
    /// Role of this process [default: `server.role` of the engine configuration].
    #[arg(long, value_enum)]
    role: Option<RoleArg>,
    /// Listen address `ip:port` [default: `server.listen`, 127.0.0.1:7420].
    /// Only role `hub` may use a non-loopback address.
    #[arg(long, value_name = "ADDR")]
    listen: Option<SocketAddr>,
    /// Public base URL (`https://host[:port]`) under which a hub is reached,
    /// e.g. behind a TLS proxy [default: $KNOWELL_PUBLIC_URL].
    #[arg(long, value_name = "URL")]
    public_url: Option<String>,
    /// Further `host:port` accepted in Host/Origin headers (repeatable; also
    /// $KNOWELL_ALLOWED_HOSTS, comma-separated).
    #[arg(long = "allow-host", value_name = "HOST:PORT")]
    allow_hosts: Vec<String>,
    /// Serve the panel from this directory instead of the embedded build.
    #[arg(long, value_name = "DIR")]
    panel_dir: Option<PathBuf>,
    /// Run without a database: store-backed routes answer `503 store_unavailable`.
    #[arg(long)]
    no_database: bool,
    /// Organization (tenant) this server serves.
    #[arg(long, default_value = "local")]
    organization: Name,
    /// Shut down gracefully when stdin is closed (for supervisors and tests).
    #[arg(long, hide = true)]
    stop_on_stdin_close: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum RoleArg {
    /// Everything in one process on one machine.
    Standalone,
    /// Team server (the only role that may listen on a non-loopback address).
    Hub,
    /// Indexing worker next to a hub.
    Worker,
    /// Developer machine connected to a hub.
    Edge,
}

impl From<RoleArg> for ServerRole {
    fn from(role: RoleArg) -> Self {
        match role {
            RoleArg::Standalone => ServerRole::Standalone,
            RoleArg::Hub => ServerRole::Hub,
            RoleArg::Worker => ServerRole::Worker,
            RoleArg::Edge => ServerRole::Edge,
        }
    }
}

pub(crate) fn run(args: ServeArgs, env: &Env, out: &mut Output) -> anyhow::Result<ExitCode> {
    let mut engine = env.require_engine()?;
    if let Some(role) = args.role {
        engine.server.role = role.into();
    }
    if let Some(listen) = args.listen {
        engine.server.listen = listen;
    }
    // The same rules as the configuration file, applied to the overrides.
    let issues = engine.validate();
    if !issues.is_empty() {
        let text: Vec<String> = issues.iter().map(ToString::to_string).collect();
        bail!("cannot serve with these settings: {}", text.join("; "));
    }

    let mut config = ServerConfig::from_engine(&engine.server, args.organization.clone());
    config.public_base_url = args
        .public_url
        .clone()
        .or_else(|| std::env::var(PUBLIC_URL_ENV).ok().filter(|v| !v.is_empty()));
    config.extra_allowed_hosts = allowed_hosts(&args.allow_hosts);
    if let Some(dir) = &args.panel_dir {
        config.panel = PanelMode::Directory(dir.clone());
    }
    let hub = engine.server.role == ServerRole::Hub;
    if !hub {
        config.local_user = Some(UserId::new(uuid::Uuid::from_u128(LOCAL_USER)));
    }
    let registry = Registry::load(&env.home)?;
    config.workspace_files = registry.files_of(&args.organization);
    if let Err(err) = config.validate() {
        if hub && config.public_base_url.is_none() && config.extra_allowed_hosts.is_empty() {
            bail!(
                "{err}; pass --public-url URL (or set {PUBLIC_URL_ENV}) or --allow-host HOST:PORT"
            );
        }
        bail!("{err}");
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;
    rt.block_on(serve(args, env, engine, config, out))
}

async fn serve(
    args: ServeArgs,
    env: &Env,
    engine: EngineConfig,
    config: ServerConfig,
    out: &mut Output,
) -> anyhow::Result<ExitCode> {
    let store = if args.no_database {
        tracing::warn!(
            "running without a database (--no-database): store-backed routes answer 503"
        );
        None
    } else {
        Some(open_store(env, &engine).await?)
    };

    let deps = EngineDeps {
        home: env.home.clone(),
        engine: engine.clone(),
        store: store.clone(),
        workspace_files: config.workspace_files.clone(),
        organization: config.organization.clone(),
        local_user: config.local_user,
    };
    let indexing = CancellationToken::new();
    let built = tools::build_engine(&deps).await?;
    let engine_ref = built.as_ref().map(|(engine, _)| engine);
    let background = built
        .as_ref()
        .map(|(engine, workspaces)| tools::start_indexing(engine, workspaces.clone(), &indexing));
    let mcp_server = KnowellServer::new(Arc::new(tools::Tools::new(engine_ref)))
        .with_caller_resolver(Arc::new(knowell_server::AuthenticatedCallers));
    let mcp = knowell_mcp::streamable_http_router(mcp_server, &mcp_options(&config));

    let mut builder = AppState::builder(config.clone())
        .with_events(EventBus::default())
        .with_mcp(mcp);
    if engine.server.token_pepper.is_some() {
        builder = builder.with_pepper(crate::token_cmd::pepper(&engine)?);
    } else if config.role == ServerRole::Hub {
        tracing::warn!("server.token_pepper is not configured; bearer tokens are disabled");
    }
    builder = match &store {
        // Tokens, grants and the audit log live in the database.
        Some(store) => {
            if let Some(user) = config.local_user {
                ensure_local_admin(store, &config.organization, user).await?;
            }
            builder
                .with_store(store.clone())
                .with_store_tokens()
                .with_store_audit()
        }
        // Without one, the local user is the only identity (in memory) and
        // audit events stay on the `knowell::audit` log target.
        None => builder.with_token_store(memory_tokens(&config)?),
    };
    builder = match tools::rest_engine(engine_ref) {
        Some(engine) => builder.with_engine(engine),
        None => builder.engine_unavailable_reason(NOT_WIRED),
    };
    let state = builder.build().map_err(|e| anyhow::anyhow!("{e}"))?;
    let listener = knowell_server::bind(&config)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let bound = listener
        .local_addr()
        .context("cannot read the bound address")?;

    let base = local_base_url(bound);
    out.line(format!(
        "knowell {} serving as {} on {bound}",
        env!("CARGO_PKG_VERSION"),
        config.role.as_str()
    ))?;
    out.line(format!("panel: {base}/"))?;
    out.line(format!("mcp:   {base}/mcp"))?;
    if let Some(public) = &config.public_base_url {
        out.line(format!("public: {public}"))?;
    }
    out.line("press Ctrl+C to stop")?;
    out.flush()?;

    let parent_shutdown = env.parent_shutdown.clone();
    let shutdown = async move {
        tokio::select! {
            _ = shutdown_signal(args.stop_on_stdin_close) => {}
            _ = parent_shutdown.cancelled() => {}
        }
    };
    let router = knowell_server::build_router(state.clone());
    let served = knowell_server::serve(listener, router, shutdown)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"));
    indexing.cancel();
    if let Some(background) = background {
        background.finish().await?;
    }
    // Queued audit events and buffered tool usage reach the database before
    // the pool closes.
    state.flush_audit().await;
    if let Some((engine, _)) = &built
        && let Err(error) = engine.flush_usage().await
    {
        tracing::warn!(%error, "tool usage could not be stored at shutdown");
    }
    if let Some(store) = store {
        store.close().await;
    }
    served?;
    tracing::info!("stopped");
    Ok(ExitCode::SUCCESS)
}

pub(crate) async fn open_store(env: &Env, engine: &EngineConfig) -> anyhow::Result<Store> {
    let store = db::connect(env, engine, Duration::from_secs(15), 10)
        .await
        .context(
            "run `know doctor` for details, or `know serve --no-database` to run without one",
        )?;
    match store.check_server().await {
        Ok(info) => {
            for issue in info.issues() {
                tracing::warn!("{issue}");
            }
            tracing::info!(
                "database: postgresql {}, pgvector {}",
                info.server_version,
                info.vector.as_ref().map_or("missing", |v| v
                    .installed_version
                    .as_deref()
                    .unwrap_or(&v.default_version))
            );
        }
        Err(err) => tracing::warn!("cannot check the database server: {err}"),
    }
    store
        .validate_schema()
        .await
        .context("database upgrade required; run an explicit maintenance migration")?;
    Ok(store)
}

/// Tokens and grants without a database: none on a hub; on other roles
/// the local user administers the organization (for the loopback panel).
fn memory_tokens(config: &ServerConfig) -> anyhow::Result<Arc<dyn TokenStore>> {
    let tokens = MemoryTokenStore::new();
    match config.local_user {
        Some(user) => {
            let grant = Grant::new(
                AuthPrincipal::User(user),
                Role::Admin,
                ResourceScope::Organization,
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            tokens
                .add_grant(grant)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        None => tracing::warn!("a hub without a database accepts no API tokens"),
    }
    Ok(Arc::new(tokens))
}

/// Registers the machine's own user in the database as an administrator of
/// the organization (creating the organization if needed), so the loopback
/// panel sees everything when grants are read from the database.
/// Idempotent; an existing principal of another kind or a taken name is
/// reported, never overwritten.
async fn ensure_local_admin(
    store: &Store,
    organization: &Name,
    user: UserId,
) -> anyhow::Result<()> {
    let mut tx = store.begin().await?;
    let conn: &mut knowell_store::PgConnection = &mut tx;
    let org = match hierarchy::find_organization(conn, organization).await? {
        Some(org) => org,
        None => hierarchy::create_organization(conn, organization).await?,
    };
    let id = PrincipalId(user.as_uuid());
    match identity::get_principal(conn, id).await? {
        Some(existing)
            if existing.organization == org.id && existing.kind == PrincipalKind::User => {}
        Some(_) => bail!(
            "the local user id is registered differently in the database; cannot serve the local panel"
        ),
        None => {
            identity::create_principal(
                conn,
                org.id,
                &NewPrincipal {
                    id: Some(id),
                    kind: PrincipalKind::User,
                    name: Name::new(LOCAL_USER_NAME)?,
                    display_name: Some("Local user".to_owned()),
                },
            )
            .await
            .context("cannot register the local user")?;
        }
    }
    let grants = identity::grants_for_principal(conn, id).await?;
    let is_admin = grants
        .iter()
        .any(|g| g.role == GrantRole::Admin && g.scope == GrantScope::Organization);
    if !is_admin {
        identity::create_grant(
            conn,
            &NewGrant {
                principal: id,
                role: GrantRole::Admin,
                scope: GrantScope::Organization,
                created_by: None,
            },
        )
        .await
        .context("cannot grant the local user")?;
    }
    tx.commit()
        .await
        .context("cannot register the local user")?;
    Ok(())
}

fn allowed_hosts(flags: &[String]) -> Vec<String> {
    let mut hosts: Vec<String> = flags.to_vec();
    if let Ok(value) = std::env::var(ALLOWED_HOSTS_ENV) {
        hosts.extend(
            value
                .split(',')
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(str::to_owned),
        );
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

/// The MCP transport's own Host check, kept consistent with the server's
/// allow-list (the server checks first; this is defence in depth).
fn mcp_options(config: &ServerConfig) -> HttpServerOptions {
    let mut options = HttpServerOptions::default();
    let mut extra: Vec<String> = config.extra_allowed_hosts.clone();
    if let Some(url) = &config.public_base_url
        && let Some(authority) = url
            .split_once("://")
            .map(|(_, rest)| rest.trim_end_matches('/'))
    {
        extra.push(authority.to_owned());
        if let Some((host, _port)) = authority.rsplit_once(':')
            && !authority.ends_with(']')
        {
            extra.push(host.to_owned());
        }
    }
    let ip = config.listen.ip();
    if !ip.is_loopback() && !ip.is_unspecified() {
        extra.push(ip.to_string());
        extra.push(config.listen.to_string());
    }
    for host in extra {
        if !options.allowed_hosts.contains(&host) {
            options.allowed_hosts.push(host);
        }
    }
    options
}

/// `http://<addr>` for printing; an unspecified address is shown as loopback.
fn local_base_url(bound: SocketAddr) -> String {
    let mut addr = bound;
    if addr.ip().is_unspecified() {
        addr.set_ip(if addr.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        });
    }
    format!("http://{addr}")
}

/// Resolves on Ctrl+C, on SIGTERM (Unix: `docker stop`), or when stdin is
/// closed if `stdin_close` is set.
async fn shutdown_signal(stdin_close: bool) {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    if stdin_close {
        // A detached thread: a blocking read must not keep the runtime alive.
        let spawned = std::thread::Builder::new()
            .name("stdin-watch".into())
            .spawn(move || {
                use std::io::Read as _;
                let mut stdin = std::io::stdin();
                let mut buf = [0u8; 256];
                while let Ok(n) = stdin.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                }
                let _ = tx.send(());
            });
        if let Err(err) = spawned {
            tracing::warn!("cannot watch stdin: {err}");
        }
    } else {
        drop(tx);
    }
    let stdin_closed = async move {
        if rx.await.is_err() {
            // Not watching stdin: never resolve from here.
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = terminate() => {}
        () = stdin_closed => {}
    }
}

#[cfg(unix)]
async fn terminate() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut sig) => {
            sig.recv().await;
        }
        Err(err) => {
            tracing::warn!("cannot listen for SIGTERM: {err}");
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(not(unix))]
async fn terminate() {
    std::future::pending::<()>().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_shows_loopback_for_unspecified() {
        assert_eq!(
            local_base_url("0.0.0.0:7420".parse().unwrap()),
            "http://127.0.0.1:7420"
        );
        assert_eq!(
            local_base_url("[::1]:7420".parse().unwrap()),
            "http://[::1]:7420"
        );
    }

    #[test]
    fn mcp_hosts_follow_the_server_allow_list() {
        let mut config = ServerConfig::new(
            ServerRole::Hub,
            "0.0.0.0:7420".parse().unwrap(),
            Name::new("local").unwrap(),
        );
        config.public_base_url = Some("https://kn.example:8443".into());
        config.extra_allowed_hosts = vec!["127.0.0.1:5173".into()];
        let hosts = mcp_options(&config).allowed_hosts;
        for expected in [
            "localhost",
            "kn.example:8443",
            "kn.example",
            "127.0.0.1:5173",
        ] {
            assert!(hosts.iter().any(|h| h == expected), "{expected}: {hosts:?}");
        }
    }
}
