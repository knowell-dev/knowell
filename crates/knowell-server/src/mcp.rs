//! The MCP mount: an MCP Streamable HTTP router (from `knowell-mcp`) served
//! at `/mcp` behind the server's authentication.
//!
//! Requests reach the MCP router unchanged (path included), so it must serve
//! `/mcp` itself, as `knowell_mcp::streamable_http_router` does. Before that,
//! the request passes the global `Host`/`Origin` checks, a `Content-Length`
//! limit, and authentication (bearer token, or session cookie with CSRF for
//! state-changing methods) plus the `use_mcp` pre-check. The
//! [`Authenticated`](crate::Authenticated) caller is in the request
//! extensions for the MCP caller resolver.

use std::convert::Infallible;
use std::task::{Context, Poll};

use axum::Router;
use axum::extract::Request;
use axum::middleware::from_fn_with_state;
use axum::response::Response;
use axum::routing::future::RouteFuture;

use crate::middleware::{mcp_body_limit, require_mcp_auth};
use crate::state::AppState;

/// Path the MCP router is mounted at.
pub const MCP_PATH: &str = "/mcp";

/// Wraps the MCP router as a plain service so it can be mounted on exact
/// paths without the prefix being stripped.
#[derive(Clone)]
struct McpService(Router);

impl tower::Service<Request> for McpService {
    type Response = Response;
    type Error = Infallible;
    type Future = RouteFuture<Infallible>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request) -> Self::Future {
        tower::Service::call(&mut self.0, req)
    }
}

/// `/mcp` and everything below it, behind authentication.
pub(crate) fn routes(state: &AppState, mcp: Router) -> Router<AppState> {
    let service = McpService(mcp);
    Router::new()
        .route_service(MCP_PATH, service.clone())
        .route_service("/mcp/{*rest}", service)
        .layer(from_fn_with_state(state.clone(), require_mcp_auth))
        .layer(from_fn_with_state(state.clone(), mcp_body_limit))
}
