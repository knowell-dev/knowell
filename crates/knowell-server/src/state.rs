//! Shared server state and its builder.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use knowell_auth::{CsrfKey, OriginPolicy, Pepper};
use knowell_store::Store;

use crate::access::{MemoryTokenStore, TokenStore};
use crate::audit::{AuditSink, TracingAuditSink};
use crate::config::{PanelMode, ServerConfig};
use crate::engine::Engine;
use crate::error::{ApiError, ServerError};
use crate::events::EventBus;
use crate::panel::PanelSource;
use crate::session::SessionStore;
use crate::store_access::StoreTokenStore;
use crate::store_audit::StoreAuditSink;
use crate::webhooks::WebhookSecrets;

/// Reason reported when no engine is wired and none was given.
const NO_ENGINE: &str = "no query engine is wired into this server";

/// Everything request handlers share. Cheap to clone.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) config: ServerConfig,
    pub(crate) policy: OriginPolicy,
    pub(crate) secure: bool,
    pub(crate) store: Option<Store>,
    pub(crate) engine: Option<Arc<dyn Engine>>,
    pub(crate) engine_reason: String,
    pub(crate) tokens: Arc<dyn TokenStore>,
    pub(crate) pepper: Option<Pepper>,
    pub(crate) csrf_key: CsrfKey,
    pub(crate) sessions: SessionStore,
    pub(crate) audit: Arc<dyn AuditSink>,
    pub(crate) events: EventBus,
    pub(crate) webhooks: WebhookSecrets,
    pub(crate) mcp: Option<Router>,
    pub(crate) panel: PanelSource,
    pub(crate) started: Instant,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("role", &self.inner.config.role)
            .field("listen", &self.inner.config.listen)
            .field("store", &self.inner.store.is_some())
            .field("engine", &self.inner.engine.is_some())
            .field("mcp", &self.inner.mcp.is_some())
            .finish_non_exhaustive()
    }
}

impl AppState {
    /// Starts building state for `config`.
    pub fn builder(config: ServerConfig) -> AppStateBuilder {
        AppStateBuilder {
            config,
            store: None,
            engine: None,
            engine_reason: None,
            tokens: None,
            pepper: None,
            audit: None,
            events: None,
            webhooks: WebhookSecrets::default(),
            mcp: None,
        }
    }

    /// The configuration.
    pub fn config(&self) -> &ServerConfig {
        &self.inner.config
    }

    /// The progress event bus (publish indexer events here).
    pub fn events(&self) -> &EventBus {
        &self.inner.events
    }

    pub(crate) fn inner(&self) -> &Inner {
        &self.inner
    }

    /// The store, or 503 `store_unavailable`.
    pub(crate) fn store(&self) -> Result<&Store, ApiError> {
        self.inner.store.as_ref().ok_or_else(|| {
            ApiError::unavailable(
                "store_unavailable",
                "this server has no database configured",
            )
        })
    }

    /// Waits until the audit sink has written every event recorded so far
    /// (see [`AuditSink::flush`]). Call it after serving stops, before the
    /// process exits.
    pub async fn flush_audit(&self) {
        self.inner.audit.flush().await;
    }

    /// The engine, or 503 `engine_unavailable` with the configured reason.
    pub(crate) fn engine(&self) -> Result<&Arc<dyn Engine>, ApiError> {
        self.inner
            .engine
            .as_ref()
            .ok_or_else(|| ApiError::engine_unavailable(self.inner.engine_reason.clone()))
    }
}

/// Where tokens and grants come from.
enum TokenSource {
    Given(Arc<dyn TokenStore>),
    Store,
}

/// Where audit events go.
enum AuditTarget {
    Given(Arc<dyn AuditSink>),
    Store,
}

/// Builder for [`AppState`].
pub struct AppStateBuilder {
    config: ServerConfig,
    store: Option<Store>,
    engine: Option<Arc<dyn Engine>>,
    engine_reason: Option<String>,
    tokens: Option<TokenSource>,
    pepper: Option<Pepper>,
    audit: Option<AuditTarget>,
    events: Option<EventBus>,
    webhooks: WebhookSecrets,
    mcp: Option<Router>,
}

impl std::fmt::Debug for AppStateBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppStateBuilder")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl AppStateBuilder {
    /// The database (store-backed routes answer 503 without it).
    pub fn with_store(mut self, store: Store) -> Self {
        self.store = Some(store);
        self
    }

    /// The engine behind delegated routes.
    pub fn with_engine(mut self, engine: Arc<dyn Engine>) -> Self {
        self.engine = Some(engine);
        self
    }

    /// The reason delegated routes report while no engine is wired
    /// (e.g. "role `worker` does not answer queries").
    pub fn engine_unavailable_reason(mut self, reason: impl Into<String>) -> Self {
        self.engine_reason = Some(reason.into());
        self
    }

    /// Token and grant storage (default: an empty [`MemoryTokenStore`],
    /// which denies everything). Replaces an earlier
    /// [`Self::with_store_tokens`].
    pub fn with_token_store(mut self, tokens: Arc<dyn TokenStore>) -> Self {
        self.tokens = Some(TokenSource::Given(tokens));
        self
    }

    /// Reads tokens and grants of the configured organization from the
    /// database ([`StoreTokenStore`], default throttling). Needs
    /// [`Self::with_store`]. Replaces an earlier [`Self::with_token_store`].
    pub fn with_store_tokens(mut self) -> Self {
        self.tokens = Some(TokenSource::Store);
        self
    }

    /// The pepper API token hashes were made with. Without it, bearer tokens
    /// are rejected.
    pub fn with_pepper(mut self, pepper: Pepper) -> Self {
        self.pepper = Some(pepper);
        self
    }

    /// Where audit events go (default: [`TracingAuditSink`]). Replaces an
    /// earlier [`Self::with_store_audit`].
    pub fn with_audit(mut self, audit: Arc<dyn AuditSink>) -> Self {
        self.audit = Some(AuditTarget::Given(audit));
        self
    }

    /// Writes audit events to the database's `audit_log`
    /// ([`StoreAuditSink`]; its writer task starts in [`Self::build`], which
    /// must then run inside a tokio runtime). Needs [`Self::with_store`].
    /// Replaces an earlier [`Self::with_audit`].
    pub fn with_store_audit(mut self) -> Self {
        self.audit = Some(AuditTarget::Store);
        self
    }

    /// The progress event bus shared with the indexer (default: a new bus).
    pub fn with_events(mut self, events: EventBus) -> Self {
        self.events = Some(events);
        self
    }

    /// Webhook secrets per provider; a provider without a secret has no
    /// webhook endpoint (404).
    pub fn with_webhook_secrets(mut self, secrets: WebhookSecrets) -> Self {
        self.webhooks = secrets;
        self
    }

    /// An MCP Streamable HTTP router serving `/mcp` (as
    /// `knowell_mcp::streamable_http_router` builds it). It is mounted at
    /// `/mcp` behind the same authentication as the REST API; routes it has
    /// outside `/mcp` are unreachable.
    pub fn with_mcp(mut self, router: Router) -> Self {
        self.mcp = Some(router);
        self
    }

    /// Validates the configuration and builds the state.
    ///
    /// # Errors
    /// [`ServerError::Config`] for an invalid configuration, a missing panel
    /// directory, a weak webhook secret, database tokens or audit without a
    /// store, or the database audit sink outside a tokio runtime;
    /// [`ServerError::Entropy`] when no randomness is available for the CSRF
    /// key.
    pub fn build(self) -> Result<AppState, ServerError> {
        self.config.validate()?;
        self.webhooks.validate()?;
        let policy = self.config.origin_policy()?;
        let panel = match &self.config.panel {
            PanelMode::Embedded => PanelSource::embedded(),
            PanelMode::Disabled => PanelSource::Disabled,
            PanelMode::Directory(dir) => {
                let root: PathBuf = std::fs::canonicalize(dir).map_err(|_| {
                    ServerError::Config("the panel directory does not exist".to_owned())
                })?;
                if !root.is_dir() {
                    return Err(ServerError::Config(
                        "the panel directory is not a directory".to_owned(),
                    ));
                }
                PanelSource::Directory(root)
            }
        };
        let csrf_key = CsrfKey::random().map_err(|_| ServerError::Entropy)?;
        let secure = self.config.is_https();
        let sessions = SessionStore::new(self.config.sessions.clone());
        let needs_store =
            |what: &str| ServerError::Config(format!("{what} needs a database; add `with_store`"));
        let tokens: Arc<dyn TokenStore> = match self.tokens {
            None => Arc::new(MemoryTokenStore::new()),
            Some(TokenSource::Given(tokens)) => tokens,
            Some(TokenSource::Store) => {
                let store = self
                    .store
                    .clone()
                    .ok_or_else(|| needs_store("`with_store_tokens`"))?;
                Arc::new(StoreTokenStore::new(
                    store,
                    self.config.organization.clone(),
                ))
            }
        };
        let audit: Arc<dyn AuditSink> = match self.audit {
            None => Arc::new(TracingAuditSink),
            Some(AuditTarget::Given(audit)) => audit,
            Some(AuditTarget::Store) => {
                let store = self
                    .store
                    .clone()
                    .ok_or_else(|| needs_store("`with_store_audit`"))?;
                Arc::new(StoreAuditSink::spawn(
                    store,
                    self.config.organization.clone(),
                )?)
            }
        };
        Ok(AppState {
            inner: Arc::new(Inner {
                policy,
                secure,
                store: self.store,
                engine: self.engine,
                engine_reason: self.engine_reason.unwrap_or_else(|| NO_ENGINE.to_owned()),
                tokens,
                pepper: self.pepper,
                csrf_key,
                sessions,
                audit,
                events: self.events.unwrap_or_default(),
                webhooks: self.webhooks,
                mcp: self.mcp,
                panel,
                started: Instant::now(),
                config: self.config,
            }),
        })
    }
}
