//! HTTP server: the versioned REST API (with SSE progress), the embedded
//! local panel, webhook receivers and the mount point for MCP over
//! Streamable HTTP.
//!
//! ```no_run
//! use std::sync::Arc;
//! use knowell_server::{AppState, MemoryTokenStore, ServerConfig, bind, build_router, serve};
//! # async fn run(config: ServerConfig) -> Result<(), knowell_server::ServerError> {
//! let listener = bind(&config).await?;
//! let state = AppState::builder(config)
//!     .with_token_store(Arc::new(MemoryTokenStore::new()))
//!     .build()?;
//! serve(listener, build_router(state), async {
//!     let _ = tokio::signal::ctrl_c().await;
//! })
//! .await
//! # }
//! ```
//!
//! # Pieces
//!
//! - [`ServerConfig`] — role, listen address (loopback unless `hub`),
//!   [`PanelMode`], public URL, [`Limits`], [`SessionSettings`].
//! - [`AppState`] — store, [`Engine`], [`TokenStore`], pepper, audit sink,
//!   [`EventBus`], [`WebhookSecrets`] and the optional MCP router.
//! - [`StoreTokenStore`] and [`StoreAuditSink`] — tokens, grants and the
//!   audit log in the database (`AppStateBuilder::with_store_tokens`,
//!   `AppStateBuilder::with_store_audit`); [`MemoryTokenStore`] and
//!   [`TracingAuditSink`] are the in-process defaults.
//! - [`build_router`] / [`serve`] — the router with its security
//!   middleware, and graceful serving.
//! - [`Engine`] — the facade for search, graph, context, memory, tasks,
//!   profiles and usage; without one those routes answer
//!   `503 engine_unavailable`.
//!
//! # Security model
//!
//! Every request: `Host` allow-list (DNS-rebinding defence) and, when an
//! `Origin` is present, origin allow-list. Protected routes: a bearer API
//! token (`kn_…`, verified with `knowell-auth`) or a panel session cookie
//! (`HttpOnly`, `SameSite=Strict`, `Secure` over https); cookie-authenticated
//! state changes also need an allowed `Origin` and the session's
//! `X-Knowell-CSRF` token. Each route authorizes its action with
//! `knowell_auth::authorize`; denials and allowed state changes are audited.
//! Errors are RFC 7807 problem+json and never echo request input, tokens or
//! connection strings. See the README for the route table and middleware
//! order.

mod access;
mod api;
mod audit;
mod config;
mod engine;
mod error;
mod events;
mod extract;
mod mcp;
mod middleware;
mod panel;
mod serve;
mod session;
mod state;
mod store_access;
mod store_audit;
mod webhooks;
mod wire;

pub use access::{AccessError, AuthMethod, Authenticated, BoxFuture, MemoryTokenStore, TokenStore};
pub use api::{JOB_KIND_VIEW_REINDEX, build_router};
pub use audit::{AuditSink, DEFAULT_MEMORY_AUDIT_CAPACITY, MemoryAuditSink, TracingAuditSink};
pub use config::{Limits, PanelMode, ServerConfig, SessionSettings};
pub use engine::{
    ContextRequest, Engine, EngineContext, EngineError, EngineRequest, GraphMode, GraphQuery,
    ImpactRequest, MemoryAction, MemoryDecision, SearchRequest, SwitchRequest, TraceDirection,
    TraceRequest,
};
pub use error::{ApiError, PROBLEM_JSON, ServerError};
pub use events::{DEFAULT_EVENT_CAPACITY, EventBus, EventScope, ProgressEvent, ScopedEvent};
pub use mcp::{AuthenticatedCallers, MCP_PATH};
pub use middleware::{CSRF_HEADER, REQUEST_ID_HEADER};
pub use serve::{DEFAULT_SHUTDOWN_GRACE, bind, serve, serve_with_grace};
pub use state::{AppState, AppStateBuilder};
pub use store_access::{DEFAULT_TOKEN_TOUCH_INTERVAL, StoreTokenStore};
pub use store_audit::{DEFAULT_AUDIT_QUEUE, StoreAuditSink};
pub use webhooks::{JOB_KIND_SOURCE_REFRESH, WebhookSecrets};
pub use wire::{
    ComponentHealth, DeadLetterView, EmbeddingSettings, EngineHealth, GenerationView, HealthStatus,
    IndexView, IndexesOverview, JobView, Liveness, ProjectDetail, ProjectSummary, ProjectViewRef,
    QueueStats, RefPolicy, ReindexRequest, ReindexResult, ReindexScope, SessionView, Setting,
    SourceView, ViewState, WebhookAck, WorkspaceDetail, WorkspaceSummary,
};

/// Re-exported: the server role used in [`ServerConfig`].
pub use knowell_config::ServerRole;
/// Re-exported so callers can build [`WebhookSecrets`] without naming the
/// `secrecy` crate.
pub use secrecy::SecretString;
