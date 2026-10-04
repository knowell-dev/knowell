//! Transports: stdio for edge/standalone, Streamable HTTP for the hub.
//!
//! Stdio uses stdout for protocol messages, so the binary must send logs to
//! stderr. The Streamable HTTP service is local-only by default: it accepts
//! only loopback `Host` headers (DNS-rebinding protection), rejects requests
//! that carry a browser `Origin`, and resolves callers with
//! [`crate::LocalOnly`]. Remote access needs a [`crate::CallerResolver`] that
//! authenticates (a later milestone) plus explicit host and origin lists.

use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::caller::TransportKind;
use crate::engine::KnowellTools;
use crate::error::ServeError;
use crate::server::KnowellServer;

/// Path the router serves MCP on. Nest the router to mount it elsewhere.
pub const MCP_HTTP_PATH: &str = "/mcp";

/// Default largest accepted POST body, in bytes. It leaves room for a
/// maximal `analyze_impact` patch plus JSON escaping.
pub const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Serves `tools` over stdin/stdout to the local user until the client
/// disconnects.
pub async fn serve_stdio<T: KnowellTools>(tools: Arc<T>) -> Result<(), ServeError> {
    serve_stdio_with(KnowellServer::new(tools)).await
}

/// Serves a configured server (e.g. with a custom caller resolver) over
/// stdin/stdout until the client disconnects.
pub async fn serve_stdio_with<T: KnowellTools>(server: KnowellServer<T>) -> Result<(), ServeError> {
    serve_stdio_with_io(server, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Serves a configured stdio server over newline-delimited JSON-RPC byte streams.
/// Uses the same protocol negotiation as [`serve_stdio_with`], including a
/// discovery probe followed by an `initialize` handshake on the same stream.
/// The reader and writer are owned until the client disconnects.
pub async fn serve_stdio_with_io<T, R, W>(
    server: KnowellServer<T>,
    reader: R,
    writer: W,
) -> Result<(), ServeError>
where
    T: KnowellTools,
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let server = server.with_transport(TransportKind::Stdio);
    let transport = rmcp::transport::async_rw::AsyncRwTransport::new_server(reader, writer);
    let Some(transport) = crate::stdio::prepare_stdio(&server, transport)
        .await
        .map_err(|error| ServeError::Initialize(error.to_string()))?
    else {
        return Ok(());
    };
    let running = server
        .serve(transport)
        .await
        .map_err(|error| ServeError::Initialize(error.to_string()))?;
    running
        .waiting()
        .await
        .map_err(|error| ServeError::Task(error.to_string()))?;
    Ok(())
}

/// Options of the Streamable HTTP transport. The defaults are local-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpServerOptions {
    /// Accepted `Host` values (`name` or `name:port`). Default: loopback
    /// names only. An empty list disables the check (never do this without
    /// authentication).
    pub allowed_hosts: Vec<String>,
    /// Accepted browser origins (`scheme://host:port`, `:*` for any port).
    /// Default: empty, which rejects every request that carries an `Origin`
    /// header; native MCP clients send none.
    pub allowed_origins: Vec<String>,
    /// Keep a session per client for protocol versions before 2026-07-28
    /// (default true; newer clients are always served statelessly).
    pub stateful_sessions: bool,
    /// Prefer plain JSON responses over SSE for simple calls (default false).
    pub json_response: bool,
    /// Largest accepted POST body, in bytes.
    pub max_request_body_bytes: usize,
}

impl Default for HttpServerOptions {
    fn default() -> Self {
        Self {
            allowed_hosts: vec!["localhost".into(), "127.0.0.1".into(), "::1".into()],
            allowed_origins: Vec::new(),
            stateful_sessions: true,
            json_response: false,
            max_request_body_bytes: DEFAULT_MAX_REQUEST_BODY_BYTES,
        }
    }
}

impl HttpServerOptions {
    /// The rmcp configuration for these options. Origin validation is
    /// always enforced, so an empty `allowed_origins` rejects every origin.
    pub fn to_rmcp_config(&self) -> StreamableHttpServerConfig {
        let config = StreamableHttpServerConfig::default()
            .with_allowed_origins(self.allowed_origins.iter().cloned())
            .enforce_origin_validation()
            .with_legacy_session_mode(self.stateful_sessions)
            .with_json_response(self.json_response)
            .with_max_request_body_bytes(self.max_request_body_bytes);
        if self.allowed_hosts.is_empty() {
            config.disable_allowed_hosts()
        } else {
            config.with_allowed_hosts(self.allowed_hosts.iter().cloned())
        }
    }
}

/// The Streamable HTTP service (a tower service) for a server. Mount it on
/// any path; its `config.cancellation_token` stops every session on
/// shutdown.
pub fn streamable_http_service<T: KnowellTools>(
    server: KnowellServer<T>,
    options: &HttpServerOptions,
) -> StreamableHttpService<KnowellServer<T>, LocalSessionManager> {
    let server = server.with_transport(TransportKind::StreamableHttp);
    StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        options.to_rmcp_config(),
    )
}

/// An axum router serving MCP at [`MCP_HTTP_PATH`]. Serve it with
/// `into_make_service_with_connect_info::<SocketAddr>()` so that
/// [`crate::LocalOnly`] can also check the peer address, and bind it to a
/// loopback address unless an authenticating resolver is configured.
pub fn streamable_http_router<T: KnowellTools>(
    server: KnowellServer<T>,
    options: &HttpServerOptions,
) -> axum::Router {
    axum::Router::new().route_service(MCP_HTTP_PATH, streamable_http_service(server, options))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_local_only() {
        let options = HttpServerOptions::default();
        let config = options.to_rmcp_config();
        assert_eq!(config.allowed_hosts, ["localhost", "127.0.0.1", "::1"]);
        assert!(config.allowed_origins.is_empty());
        assert_eq!(
            config.max_request_body_bytes,
            DEFAULT_MAX_REQUEST_BODY_BYTES
        );
    }
}
