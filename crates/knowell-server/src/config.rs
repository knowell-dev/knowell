//! Server configuration: role, listen address, panel mode, public URL,
//! limits and session lifetimes.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use knowell_auth::{OriginPolicy, UserId};
use knowell_config::ServerRole;
use knowell_core::Name;

use crate::error::ServerError;

/// Where the panel's static files come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelMode {
    /// The files embedded at build time: `panel/dist` when it existed during
    /// the build, otherwise a placeholder page explaining how to build it.
    Embedded,
    /// Files read at run time from this directory (panel development). The
    /// directory must exist when the server state is built.
    Directory(PathBuf),
    /// No panel: `/` answers 404.
    Disabled,
}

/// Request limits. Sizes are bytes, durations wall-clock time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Largest JSON request body on `/api/v1` (default 1 MiB).
    pub api_body_bytes: usize,
    /// Largest webhook delivery (default 10 MiB; GitHub caps payloads at 25 MB,
    /// push events are far smaller).
    pub webhook_body_bytes: usize,
    /// Largest MCP request announced by `Content-Length` (default 4 MiB, the
    /// MCP transport's own limit).
    pub mcp_body_bytes: usize,
    /// Time allowed to produce response headers on `/api/v1` and webhooks
    /// (default 30 s). Streams (SSE, MCP) are not cut by it.
    pub request_timeout: Duration,
    /// Interval of `heartbeat` events on the progress stream (default 15 s).
    pub sse_heartbeat: Duration,
    /// Most jobs returned by one job listing (default 200).
    pub max_jobs_listed: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            api_body_bytes: 1024 * 1024,
            webhook_body_bytes: 10 * 1024 * 1024,
            mcp_body_bytes: 4 * 1024 * 1024,
            request_timeout: Duration::from_secs(30),
            sse_heartbeat: Duration::from_secs(15),
            max_jobs_listed: 200,
        }
    }
}

/// Panel session lifetimes (sessions live in memory: a restart signs
/// everyone out).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSettings {
    /// A session unused for this long expires (default 12 h).
    pub idle_timeout: Duration,
    /// A session expires this long after creation regardless of use (default 7 days).
    pub absolute_lifetime: Duration,
    /// Most live sessions; the least recently used is evicted beyond it (default 1024).
    pub max_sessions: usize,
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(12 * 3600),
            absolute_lifetime: Duration::from_secs(7 * 24 * 3600),
            max_sessions: 1024,
        }
    }
}

/// Everything the server needs to know about its deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Role of this process.
    pub role: ServerRole,
    /// Listen address. Every role except `hub` must use a loopback address
    /// (the same rule as `knowell-config`).
    pub listen: SocketAddr,
    /// Panel source.
    pub panel: PanelMode,
    /// Public base URL (`https://knowell.example.com[:port]`) when the server
    /// is reached under a name or behind a TLS proxy. Its authority joins the
    /// `Host` / `Origin` allow-list; `https` also makes cookies `Secure` and
    /// adds HSTS.
    pub public_base_url: Option<String>,
    /// Further `host:port` values to accept in `Host` / `Origin`, e.g. the
    /// panel's Vite dev server `127.0.0.1:5173`.
    pub extra_allowed_hosts: Vec<String>,
    /// The organization (tenant) this server serves.
    pub organization: Name,
    /// The user the loopback panel acts as. Sessions are issued to this user
    /// without a login only when the role is not `hub`; `None` disables
    /// that, so the panel needs a token login.
    pub local_user: Option<UserId>,
    /// Version string reported by `/api/v1/health` (the binary's version).
    pub version: String,
    /// Workspace configuration files (`knowell.toml`) by workspace name, used
    /// to show effective settings and where they come from.
    pub workspace_files: BTreeMap<Name, PathBuf>,
    /// Request limits.
    pub limits: Limits,
    /// Panel session lifetimes.
    pub sessions: SessionSettings,
}

impl ServerConfig {
    /// A configuration with defaults: embedded panel, no public URL, no local
    /// user, default limits and session lifetimes.
    pub fn new(role: ServerRole, listen: SocketAddr, organization: Name) -> Self {
        Self {
            role,
            listen,
            panel: PanelMode::Embedded,
            public_base_url: None,
            extra_allowed_hosts: Vec::new(),
            organization,
            local_user: None,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            workspace_files: BTreeMap::new(),
            limits: Limits::default(),
            sessions: SessionSettings::default(),
        }
    }

    /// Role and listen address from the engine configuration file's
    /// `[server]` table, defaults for everything else.
    pub fn from_engine(server: &knowell_config::ServerConfig, organization: Name) -> Self {
        Self::new(server.role, server.listen, organization)
    }

    /// Checks every rule; called when the server state is built.
    ///
    /// # Errors
    /// [`ServerError::Config`] naming the first violated rule.
    pub fn validate(&self) -> Result<(), ServerError> {
        let loopback = self.listen.ip().is_loopback();
        if self.role != ServerRole::Hub && !loopback {
            return Err(ServerError::Config(format!(
                "role `{}` must listen on a loopback address (127.0.0.1 or ::1) so the panel is not exposed to the network; only role `hub` may bind other interfaces",
                self.role.as_str()
            )));
        }
        if self.listen.port() == 0 {
            return Err(ServerError::Config(
                "listen port must not be 0; the panel's host allow-list needs the real port"
                    .to_owned(),
            ));
        }
        if let Some(url) = &self.public_base_url {
            PublicUrl::parse(url)?;
        }
        if self.role == ServerRole::Hub
            && !loopback
            && self.public_base_url.is_none()
            && self.extra_allowed_hosts.is_empty()
        {
            return Err(ServerError::Config(
                "a hub listening on a non-loopback address needs `public_base_url` (or `extra_allowed_hosts`) so remote clients pass the host check".to_owned(),
            ));
        }
        let limits = &self.limits;
        if limits.api_body_bytes == 0
            || limits.webhook_body_bytes == 0
            || limits.mcp_body_bytes == 0
            || limits.request_timeout.is_zero()
            || limits.sse_heartbeat.is_zero()
            || limits.max_jobs_listed == 0
        {
            return Err(ServerError::Config(
                "limits must be greater than zero".to_owned(),
            ));
        }
        let sessions = &self.sessions;
        if sessions.idle_timeout.is_zero()
            || sessions.absolute_lifetime.is_zero()
            || sessions.max_sessions == 0
        {
            return Err(ServerError::Config(
                "session lifetimes and capacity must be greater than zero".to_owned(),
            ));
        }
        self.origin_policy().map(|_| ())
    }

    /// Whether the server is reached over `https` (per `public_base_url`).
    pub fn is_https(&self) -> bool {
        self.public_base_url
            .as_deref()
            .and_then(|u| PublicUrl::parse(u).ok())
            .is_some_and(|u| u.https)
    }

    /// Whether loopback panel sessions are issued without a login.
    pub fn local_sessions(&self) -> bool {
        self.role != ServerRole::Hub && self.local_user.is_some()
    }

    /// The `Host` / `Origin` allow-list: the loopback names with the listen
    /// port, the listen address itself when it is a specific non-loopback
    /// IP, the public URL's authority and the extra hosts.
    ///
    /// # Errors
    /// [`ServerError::Config`] for an invalid public URL or extra host.
    pub fn origin_policy(&self) -> Result<OriginPolicy, ServerError> {
        let invalid = |what: &str| {
            ServerError::Config(format!(
                "{what} must look like `name:port` or `[v6]:port` without wildcards"
            ))
        };
        let mut policy = OriginPolicy::loopback(self.listen.port());
        let ip = self.listen.ip();
        if !ip.is_loopback() && !ip.is_unspecified() {
            let authority = match ip {
                std::net::IpAddr::V4(v4) => format!("{v4}:{}", self.listen.port()),
                std::net::IpAddr::V6(v6) => format!("[{v6}]:{}", self.listen.port()),
            };
            policy = policy
                .with_host(&authority)
                .map_err(|_| invalid("listen address"))?;
        }
        if let Some(url) = &self.public_base_url {
            let public = PublicUrl::parse(url)?;
            policy = policy
                .with_host(&public.authority)
                .map_err(|_| invalid("public_base_url host"))?;
            if public.https {
                policy = policy.with_https();
            }
        }
        for host in &self.extra_allowed_hosts {
            policy = policy
                .with_host(host)
                .map_err(|_| invalid("extra_allowed_hosts entry"))?;
        }
        Ok(policy)
    }

    /// Default port assumed for a `Host` header without one: 443 when the
    /// public URL is `https`, else 80.
    pub(crate) fn default_host_port(&self) -> u16 {
        if self.is_https() { 443 } else { 80 }
    }
}

/// A validated public base URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublicUrl {
    pub(crate) https: bool,
    /// `host:port` with the port always explicit.
    pub(crate) authority: String,
}

impl PublicUrl {
    pub(crate) fn parse(url: &str) -> Result<Self, ServerError> {
        let invalid = || {
            ServerError::Config(
                "public_base_url must look like `https://host[:port]` (no user info, path, query or fragment)".to_owned(),
            )
        };
        let lower = url.to_ascii_lowercase();
        let (https, rest) = if let Some(rest) = lower.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = lower.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(invalid());
        };
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if rest.is_empty() || rest.contains(['/', '?', '#', '@', '\\', ' ', '*']) {
            return Err(invalid());
        }
        let default_port = if https { 443 } else { 80 };
        let authority = with_default_port(rest, default_port).ok_or_else(invalid)?;
        Ok(Self { https, authority })
    }
}

/// Returns `authority` with an explicit port, appending `default_port` when
/// it has none. `None` for a malformed authority (empty host, bad port,
/// unbalanced IPv6 brackets).
pub(crate) fn with_default_port(authority: &str, default_port: u16) -> Option<String> {
    if let Some(after_bracket) = authority.strip_prefix('[') {
        let (_, tail) = after_bracket.split_once(']')?;
        return match tail {
            "" => Some(format!("{authority}:{default_port}")),
            t => {
                let port = t.strip_prefix(':')?;
                valid_port(port).then(|| authority.to_owned())
            }
        };
    }
    match authority.rsplit_once(':') {
        None if !authority.is_empty() => Some(format!("{authority}:{default_port}")),
        None => None,
        Some((host, port)) => (!host.is_empty() && !host.contains(':') && valid_port(port))
            .then(|| authority.to_owned()),
    }
}

fn valid_port(port: &str) -> bool {
    !port.is_empty()
        && port.bytes().all(|b| b.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|p| p != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(role: ServerRole, listen: &str) -> ServerConfig {
        ServerConfig::new(role, listen.parse().unwrap(), Name::new("acme").unwrap())
    }

    #[test]
    fn loopback_rule_mirrors_knowell_config() {
        assert!(
            cfg(ServerRole::Standalone, "127.0.0.1:7420")
                .validate()
                .is_ok()
        );
        assert!(cfg(ServerRole::Edge, "[::1]:7420").validate().is_ok());
        for role in [ServerRole::Standalone, ServerRole::Edge, ServerRole::Worker] {
            let err = cfg(role, "0.0.0.0:7420").validate().unwrap_err();
            assert!(err.to_string().contains("loopback"), "{err}");
            assert!(cfg(role, "192.168.1.5:7420").validate().is_err());
        }
        // A hub on all interfaces needs a public name for the host check.
        assert!(cfg(ServerRole::Hub, "0.0.0.0:7420").validate().is_err());
        let mut hub = cfg(ServerRole::Hub, "0.0.0.0:7420");
        hub.public_base_url = Some("https://knowell.example.com".into());
        assert!(hub.validate().is_ok());
        assert!(hub.is_https());
        assert!(!hub.local_sessions());
    }

    #[test]
    fn port_zero_and_limits_rejected() {
        assert!(
            cfg(ServerRole::Standalone, "127.0.0.1:0")
                .validate()
                .is_err()
        );
        let mut c = cfg(ServerRole::Standalone, "127.0.0.1:7420");
        c.limits.api_body_bytes = 0;
        assert!(c.validate().is_err());
        let mut c = cfg(ServerRole::Standalone, "127.0.0.1:7420");
        c.sessions.max_sessions = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn public_urls() {
        let ok = |u: &str| PublicUrl::parse(u).unwrap();
        assert_eq!(
            ok("https://Knowell.Example.com").authority,
            "knowell.example.com:443"
        );
        assert_eq!(
            ok("https://knowell.example.com/").authority,
            "knowell.example.com:443"
        );
        assert_eq!(ok("http://h.example:8080").authority, "h.example:8080");
        assert_eq!(ok("https://[::1]").authority, "[::1]:443");
        assert!(!ok("http://h.example").https);
        for bad in [
            "ftp://h.example",
            "h.example",
            "https://",
            "https://user:pw@h.example",
            "https://h.example/path",
            "https://h.example?q=1",
            "https://h.example#f",
            "https://h.example:0",
            "https://h.example:99999",
            "https://h.example:port",
            "https://*.example",
            "https://[::1",
        ] {
            assert!(PublicUrl::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn default_ports() {
        assert_eq!(
            with_default_port("a.example", 80).as_deref(),
            Some("a.example:80")
        );
        assert_eq!(
            with_default_port("a.example:81", 80).as_deref(),
            Some("a.example:81")
        );
        assert_eq!(
            with_default_port("[::1]", 443).as_deref(),
            Some("[::1]:443")
        );
        assert_eq!(
            with_default_port("[::1]:9", 443).as_deref(),
            Some("[::1]:9")
        );
        for bad in ["", ":80", "a:b:80", "[::1]x", "[::1]:", "a.example:", "a:0"] {
            assert_eq!(with_default_port(bad, 80), None, "{bad}");
        }
    }

    #[test]
    fn policy_includes_public_and_extra_hosts() {
        let mut c = cfg(ServerRole::Standalone, "127.0.0.1:7420");
        c.extra_allowed_hosts = vec!["127.0.0.1:5173".into()];
        let p = c.origin_policy().unwrap();
        assert!(p.check_host(Some("127.0.0.1:5173")).is_ok());
        assert!(p.check_host(Some("localhost:7420")).is_ok());
        assert!(p.check_host(Some("evil.example:7420")).is_err());
        c.extra_allowed_hosts = vec!["*:80".into()];
        assert!(c.validate().is_err());

        let mut hub = cfg(ServerRole::Hub, "10.0.0.5:7420");
        hub.public_base_url = Some("https://kn.example".into());
        let p = hub.origin_policy().unwrap();
        assert!(p.check_host(Some("10.0.0.5:7420")).is_ok());
        assert!(p.check_host(Some("kn.example:443")).is_ok());
        assert!(p.check_origin(Some("https://kn.example:443"), true).is_ok());
        assert_eq!(hub.default_host_port(), 443);
    }

    #[test]
    fn from_engine_config() {
        let engine = knowell_config::ServerConfig::default();
        let c = ServerConfig::from_engine(&engine, Name::new("acme").unwrap());
        assert_eq!(c.role, ServerRole::Standalone);
        assert_eq!(c.listen, engine.listen);
        assert!(c.validate().is_ok());
    }
}
