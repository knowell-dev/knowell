//! Engine configuration (`~/.knowell/config.toml`): how this Knowell
//! process runs, where its database is, and which embedding providers exist.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use knowell_core::{Name, SecretRef};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ConfigIssue;

/// Only configuration version understood by this build.
pub(crate) const SUPPORTED_VERSION: u32 = 1;

/// Engine configuration, normally stored at `~/.knowell/config.toml`.
///
/// Holds machine-level settings only. Which projects exist and how they are
/// indexed lives in the workspace file (`knowell.toml`). Secrets are never
/// written here: use references such as `env:GEMINI_API_KEY`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    /// Configuration format version. Must be `1`.
    #[schemars(range(min = 1, max = 1))]
    pub version: u32,
    /// Network role and listen address of this process.
    #[serde(default)]
    pub server: ServerConfig,
    /// Where Knowell stores its index and memory.
    #[serde(default)]
    pub database: DatabaseConfig,
    /// Embedding providers, keyed by a name that workspaces refer to.
    #[serde(default)]
    pub providers: BTreeMap<Name, ProviderConfig>,
    /// Connection to the hub. Required when `server.role = "edge"`, and only
    /// allowed then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub: Option<HubConfig>,
    /// Telemetry opt-in.
    #[serde(default)]
    pub telemetry: TelemetryConfig,
}

/// Role of this process in a deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ServerRole {
    /// One machine runs everything: panel, indexer and agent endpoint.
    #[default]
    Standalone,
    /// Central server shared by a team. The only role that may listen on a
    /// non-loopback address.
    Hub,
    /// Indexing worker that serves a hub.
    Worker,
    /// Developer machine that talks to a hub; requires the `[hub]` table.
    Edge,
}

impl ServerRole {
    /// Lowercase name as written in the configuration file.
    pub fn as_str(self) -> &'static str {
        match self {
            ServerRole::Standalone => "standalone",
            ServerRole::Hub => "hub",
            ServerRole::Worker => "worker",
            ServerRole::Edge => "edge",
        }
    }
}

/// Server role and listen address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    /// Role of this process. Defaults to `standalone`.
    pub role: ServerRole,
    /// Address to listen on, as `ip:port`. Defaults to `127.0.0.1:7420`.
    /// Every role except `hub` must use a loopback address so the local
    /// panel is never exposed to the network.
    pub listen: SocketAddr,
    /// Reference to the stable API-token pepper (at least 16 bytes), e.g.
    /// `env:KNOWELL_TOKEN_PEPPER`. Required to issue or verify bearer tokens.
    /// Changing it invalidates all existing tokens. Never put the value here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_pepper: Option<SecretRef>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            role: ServerRole::Standalone,
            listen: SocketAddr::from(([127, 0, 0, 1], 7420)),
            token_pepper: None,
        }
    }
}

/// Database ownership mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum DatabaseMode {
    /// Knowell starts and manages its own PostgreSQL instance.
    #[default]
    Managed,
    /// Knowell connects to a PostgreSQL server you operate; requires `url`.
    External,
}

/// Database settings.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct DatabaseConfig {
    /// `managed` (default) or `external`.
    pub mode: DatabaseMode,
    /// Reference to the connection URL, e.g. `env:KNOWELL_DATABASE_URL`.
    /// Required for `external`, not allowed for `managed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<SecretRef>,
    /// Directory for managed database files. Only for `managed`; when
    /// omitted Knowell uses its default data directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,
}

/// Kind of embedding provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    /// Google Gemini API (cloud).
    Gemini,
    /// Any server speaking the OpenAI embeddings API; requires `base_url`.
    OpenaiCompatible,
    /// Local Ollama server.
    Ollama,
    /// Voyage AI API (cloud).
    Voyage,
}

impl ProviderKind {
    /// Name as written in the configuration file.
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Gemini => "gemini",
            ProviderKind::OpenaiCompatible => "openai-compatible",
            ProviderKind::Ollama => "ollama",
            ProviderKind::Voyage => "voyage",
        }
    }

    /// Whether the kind is always a third-party cloud service. An
    /// `openai-compatible` endpoint may be local, so it is not counted.
    pub fn is_cloud(self) -> bool {
        matches!(self, ProviderKind::Gemini | ProviderKind::Voyage)
    }
}

/// Default Ollama endpoint, applied only when `base_url` is omitted.
pub const OLLAMA_DEFAULT_BASE_URL: &str = "http://127.0.0.1:11434";

/// One embedding provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Provider kind.
    pub kind: ProviderKind,
    /// Reference to the API key, e.g. `env:GEMINI_API_KEY`. Required for
    /// `gemini` and `voyage`, optional for `openai-compatible`, not allowed
    /// for `ollama`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<SecretRef>,
    /// Endpoint base URL (`http://` or `https://`). Required for
    /// `openai-compatible`; optional for `gemini` and `voyage`; for `ollama`
    /// it defaults to `http://127.0.0.1:11434` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Default embedding model for workspaces that do not name one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl ProviderConfig {
    /// The endpoint to use: the configured `base_url`, or Ollama's default
    /// for `ollama` providers that omit it. Never guesses for other kinds.
    pub fn effective_base_url(&self) -> Option<&str> {
        match (&self.base_url, self.kind) {
            (Some(url), _) => Some(url.as_str()),
            (None, ProviderKind::Ollama) => Some(OLLAMA_DEFAULT_BASE_URL),
            (None, _) => None,
        }
    }
}

/// Connection to a hub (for `role = "edge"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HubConfig {
    /// Hub base URL, `http://` or `https://`.
    pub url: String,
    /// Reference to the access token, e.g. `env:KNOWELL_HUB_TOKEN`.
    pub token: SecretRef,
}

/// Telemetry settings.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct TelemetryConfig {
    /// Whether anonymous usage telemetry may be sent. Defaults to `false`;
    /// Knowell never sends telemetry without this opt-in.
    pub enabled: bool,
}

impl EngineConfig {
    /// Checks every rule and returns all violations (empty when valid).
    pub fn validate(&self) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();
        if self.version != SUPPORTED_VERSION {
            issues.push(ConfigIssue::new(
                "version",
                format!(
                    "unsupported version {}; this build understands version {SUPPORTED_VERSION}",
                    self.version
                ),
            ));
        }
        self.validate_server(&mut issues);
        self.validate_database(&mut issues);
        for (name, provider) in &self.providers {
            validate_provider(name, provider, &mut issues);
        }
        self.validate_hub(&mut issues);
        issues
    }

    fn validate_server(&self, issues: &mut Vec<ConfigIssue>) {
        let server = &self.server;
        if server.role != ServerRole::Hub && !server.listen.ip().is_loopback() {
            issues.push(ConfigIssue::new(
                "server.listen",
                format!(
                    "role `{}` must listen on a loopback address (127.0.0.1 or ::1) so the local panel is not exposed to the network; only role `hub` may bind other interfaces",
                    server.role.as_str()
                ),
            ));
        }
    }

    fn validate_database(&self, issues: &mut Vec<ConfigIssue>) {
        let db = &self.database;
        match db.mode {
            DatabaseMode::External => {
                if db.url.is_none() {
                    issues.push(ConfigIssue::new(
                        "database.url",
                        "mode `external` requires `url`, a reference such as `env:KNOWELL_DATABASE_URL`",
                    ));
                }
                if db.data_dir.is_some() {
                    issues.push(ConfigIssue::new(
                        "database.data_dir",
                        "`data_dir` only applies to mode `managed`",
                    ));
                }
            }
            DatabaseMode::Managed => {
                if db.url.is_some() {
                    issues.push(ConfigIssue::new(
                        "database.url",
                        "`url` is not allowed for mode `managed`; use mode `external` to connect to your own server",
                    ));
                }
            }
        }
        if let Some(dir) = &db.data_dir
            && dir.as_os_str().is_empty()
        {
            issues.push(ConfigIssue::new(
                "database.data_dir",
                "`data_dir` must not be empty",
            ));
        }
    }

    fn validate_hub(&self, issues: &mut Vec<ConfigIssue>) {
        match (&self.hub, self.server.role) {
            (None, ServerRole::Edge) => issues.push(ConfigIssue::new(
                "hub",
                "role `edge` requires a `[hub]` table with `url` and `token`",
            )),
            (Some(_), role) if role != ServerRole::Edge => issues.push(ConfigIssue::new(
                "hub",
                format!(
                    "`[hub]` only applies to role `edge`, but role is `{}`",
                    role.as_str()
                ),
            )),
            _ => {}
        }
        if let Some(hub) = &self.hub
            && !is_http_url(&hub.url)
        {
            issues.push(ConfigIssue::new(
                "hub.url",
                "`url` must be an http:// or https:// URL",
            ));
        }
    }
}

fn validate_provider(name: &Name, p: &ProviderConfig, issues: &mut Vec<ConfigIssue>) {
    let at = |field: &str| format!("providers.{name}.{field}");
    match p.kind {
        ProviderKind::Gemini | ProviderKind::Voyage if p.api_key.is_none() => {
            issues.push(ConfigIssue::new(
                at("api_key"),
                format!(
                    "kind `{}` requires `api_key`, a reference such as `env:PROVIDER_API_KEY`",
                    p.kind.as_str()
                ),
            ));
        }
        ProviderKind::Ollama if p.api_key.is_some() => {
            issues.push(ConfigIssue::new(
                at("api_key"),
                "kind `ollama` does not use an API key",
            ));
        }
        ProviderKind::OpenaiCompatible if p.base_url.is_none() => {
            issues.push(ConfigIssue::new(
                at("base_url"),
                "kind `openai-compatible` requires `base_url`",
            ));
        }
        _ => {}
    }
    if let Some(url) = &p.base_url
        && !is_http_url(url)
    {
        issues.push(ConfigIssue::new(
            at("base_url"),
            "`base_url` must be an http:// or https:// URL",
        ));
    }
    if let Some(model) = &p.model
        && model.trim().is_empty()
    {
        issues.push(ConfigIssue::new(at("model"), "`model` must not be empty"));
    }
}

/// Light syntactic check: `http://` or `https://`, a non-empty host part, no
/// whitespace or control characters. Deliberately does not echo the value.
pub(crate) fn is_http_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    match rest {
        Some(rest) => {
            !rest.is_empty()
                && !rest.starts_with('/')
                && !url.chars().any(|c| c.is_whitespace() || c.is_control())
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_engine;

    fn issues_of(text: &str) -> Vec<ConfigIssue> {
        match parse_engine(text) {
            Ok(_) => Vec::new(),
            Err(crate::ConfigError::Invalid { issues, .. }) => issues.0,
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    fn has(issues: &[ConfigIssue], path: &str) -> bool {
        issues.iter().any(|i| i.path == path)
    }

    #[test]
    fn minimal_config_uses_defaults() {
        let cfg = parse_engine("version = 1").unwrap();
        assert_eq!(cfg.server.role, ServerRole::Standalone);
        assert_eq!(cfg.server.listen.to_string(), "127.0.0.1:7420");
        assert_eq!(cfg.database.mode, DatabaseMode::Managed);
        assert!(!cfg.telemetry.enabled);
        assert!(cfg.providers.is_empty());
        assert!(cfg.hub.is_none());
        assert!(cfg.server.token_pepper.is_none());
    }

    #[test]
    fn token_pepper_is_only_a_secret_reference() {
        let config =
            parse_engine("version = 1\n[server]\ntoken_pepper = 'env:KNOWELL_TOKEN_PEPPER'")
                .unwrap();
        assert!(config.server.token_pepper.is_some());
        let canary = "KNOWELL_CANARY_pasted_pepper";
        let error =
            parse_engine(&format!("version = 1\n[server]\ntoken_pepper = '{canary}'")).unwrap_err();
        assert!(!format!("{error:?} {error}").contains(canary));
    }

    #[test]
    fn version_is_required_and_must_be_one() {
        assert!(matches!(
            parse_engine(""),
            Err(crate::ConfigError::Parse { .. })
        ));
        assert!(has(&issues_of("version = 2"), "version"));
        assert!(has(&issues_of("version = 0"), "version"));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        for text in [
            "version = 1\nservr = 1",
            "version = 1\n[server]\nrol = \"hub\"",
            "version = 1\n[providers.a]\nkind = \"ollama\"\nmodle = \"x\"",
        ] {
            let err = parse_engine(text).unwrap_err();
            assert!(err.to_string().contains("unknown field"), "{err}");
        }
    }

    #[test]
    fn non_loopback_is_only_allowed_for_hub() {
        for role in ["standalone", "worker"] {
            let text =
                format!("version = 1\n[server]\nrole = \"{role}\"\nlisten = \"0.0.0.0:7420\"");
            assert!(has(&issues_of(&text), "server.listen"), "{role}");
        }
        let edge = "version = 1\n[server]\nrole = \"edge\"\nlisten = \"192.168.1.5:7420\"\n[hub]\nurl = \"https://h.example\"\ntoken = \"env:T\"";
        assert!(has(&issues_of(edge), "server.listen"));
        let hub = "version = 1\n[server]\nrole = \"hub\"\nlisten = \"0.0.0.0:7420\"";
        assert!(issues_of(hub).is_empty());
        let v6 = "version = 1\n[server]\nlisten = \"[::1]:7420\"";
        assert!(issues_of(v6).is_empty());
    }

    #[test]
    fn invalid_listen_address_is_a_parse_error() {
        let err = parse_engine("version = 1\n[server]\nlisten = \"localhost\"").unwrap_err();
        assert!(matches!(
            err,
            crate::ConfigError::Parse { line: Some(3), .. }
        ));
    }

    #[test]
    fn database_rules() {
        let ext = "version = 1\n[database]\nmode = \"external\"";
        assert!(has(&issues_of(ext), "database.url"));
        let managed = "version = 1\n[database]\nurl = \"env:X\"";
        assert!(has(&issues_of(managed), "database.url"));
        let ext_dir =
            "version = 1\n[database]\nmode = \"external\"\nurl = \"env:X\"\ndata_dir = \"/d\"";
        assert!(has(&issues_of(ext_dir), "database.data_dir"));
        let ok = "version = 1\n[database]\nmode = \"external\"\nurl = \"env:KNOWELL_DATABASE_URL\"";
        assert!(issues_of(ok).is_empty());
        let dir = "version = 1\n[database]\ndata_dir = \"/var/lib/knowell\"";
        assert!(issues_of(dir).is_empty());
        let empty = "version = 1\n[database]\ndata_dir = \"\"";
        assert!(has(&issues_of(empty), "database.data_dir"));
    }

    #[test]
    fn provider_rules() {
        let text = r#"
version = 1
[providers.g]
kind = "gemini"
[providers.v]
kind = "voyage"
[providers.o]
kind = "ollama"
api_key = "env:X"
[providers.c]
kind = "openai-compatible"
[providers.bad-url]
kind = "ollama"
base_url = "ftp://x"
model = " "
"#;
        let issues = issues_of(text);
        for path in [
            "providers.g.api_key",
            "providers.v.api_key",
            "providers.o.api_key",
            "providers.c.base_url",
            "providers.bad-url.base_url",
            "providers.bad-url.model",
        ] {
            assert!(has(&issues, path), "missing {path}: {issues:?}");
        }
        assert_eq!(issues.len(), 6, "{issues:?}");
    }

    #[test]
    fn valid_providers_and_defaults() {
        let text = r#"
version = 1
[providers.g]
kind = "gemini"
api_key = "env:GEMINI_API_KEY"
model = "gemini-embedding-2"
[providers.local]
kind = "ollama"
[providers.compat]
kind = "openai-compatible"
base_url = "http://localhost:8080/v1"
"#;
        let cfg = parse_engine(text).unwrap();
        let get = |n: &str| cfg.providers.get(&Name::new(n).unwrap()).unwrap();
        assert_eq!(
            get("local").effective_base_url(),
            Some(OLLAMA_DEFAULT_BASE_URL)
        );
        assert_eq!(get("g").effective_base_url(), None);
        assert_eq!(
            get("compat").effective_base_url(),
            Some("http://localhost:8080/v1")
        );
        assert!(get("g").kind.is_cloud());
        assert!(!get("compat").kind.is_cloud());
    }

    #[test]
    fn invalid_provider_name_is_rejected() {
        let err = parse_engine("version = 1\n[providers.Bad]\nkind = \"ollama\"").unwrap_err();
        assert!(matches!(err, crate::ConfigError::Parse { .. }));
    }

    #[test]
    fn hub_rules() {
        let edge = "version = 1\n[server]\nrole = \"edge\"";
        assert!(has(&issues_of(edge), "hub"));
        let stray = "version = 1\n[hub]\nurl = \"https://h.example\"\ntoken = \"env:T\"";
        assert!(has(&issues_of(stray), "hub"));
        let bad_url =
            "version = 1\n[server]\nrole = \"edge\"\n[hub]\nurl = \"h.example\"\ntoken = \"env:T\"";
        assert!(has(&issues_of(bad_url), "hub.url"));
        let ok = "version = 1\n[server]\nrole = \"edge\"\n[hub]\nurl = \"https://h.example\"\ntoken = \"env:KNOWELL_HUB_TOKEN\"";
        assert!(issues_of(ok).is_empty());
    }

    #[test]
    fn all_issues_are_collected() {
        let text = "version = 3\n[server]\nlisten = \"0.0.0.0:1\"\n[database]\nmode = \"external\"\n[providers.g]\nkind = \"gemini\"";
        let issues = issues_of(text);
        assert_eq!(issues.len(), 4, "{issues:?}");
    }

    #[test]
    fn url_check() {
        assert!(is_http_url("https://a.example/x"));
        assert!(is_http_url("http://127.0.0.1:11434"));
        for bad in [
            "",
            "https://",
            "http:///x",
            "https://a b",
            "a.example",
            "ftp://x",
        ] {
            assert!(!is_http_url(bad), "{bad}");
        }
    }
}
