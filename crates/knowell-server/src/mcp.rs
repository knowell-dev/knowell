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

/// Passes this HTTP request's verified identity and credential scopes to MCP.
/// Missing middleware authentication is rejected, even on loopback. Client
/// metadata and tool arguments never supply or override the identity.
#[derive(Debug, Clone, Copy, Default)]
pub struct AuthenticatedCallers;

impl knowell_mcp::CallerResolver for AuthenticatedCallers {
    fn resolve(
        &self,
        head: &knowell_mcp::RequestHead<'_>,
    ) -> Result<knowell_mcp::Caller, knowell_mcp::ToolError> {
        let authenticated = head
            .http
            .and_then(|parts| parts.extensions.get::<crate::Authenticated>());
        match (head.transport, authenticated) {
            (knowell_mcp::TransportKind::StreamableHttp, Some(auth)) => Ok(knowell_mcp::Caller {
                principal: knowell_mcp::Principal::Authenticated {
                    principal: auth.principal.clone(),
                    scopes: auth.scopes.clone(),
                },
                transport: head.transport,
                client: head.client.cloned(),
            }),
            _ => Err(knowell_mcp::ToolError::permission_denied(
                "authentication is required for MCP over HTTP",
            )),
        }
    }
}

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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use knowell_auth::{
        AgentSessionId, GrantSet, Principal, ServiceAccountId, TokenScopes, UserId,
    };
    use knowell_mcp::{CallerResolver, ClientIdentity, RequestHead, TransportKind};

    use super::*;

    #[test]
    fn typed_subjects_preserve_identity_and_scopes() {
        let user = UserId::new(uuid::Uuid::from_u128(1));
        for principal in [
            Principal::User(user),
            Principal::ServiceAccount(ServiceAccountId::new(uuid::Uuid::from_u128(2))),
            Principal::Agent {
                on_behalf_of: user,
                client: knowell_core::Name::new("verified-agent").unwrap(),
                session: AgentSessionId::new(uuid::Uuid::from_u128(3)),
            },
        ] {
            let grants = GrantSet::new();
            let auth = crate::Authenticated {
                visible: knowell_auth::visible_projects(&principal, &grants),
                principal: principal.clone(),
                scopes: Some(TokenScopes::read_only()),
                grants: Arc::new(grants),
                method: crate::AuthMethod::Session {
                    fingerprint: "fake-session".into(),
                },
            };
            let mut parts = axum::http::Request::new(()).into_parts().0;
            parts.extensions.insert(auth);
            let client = ClientIdentity {
                name: "claimed-admin".into(),
                version: "1".into(),
            };
            let head = RequestHead {
                transport: TransportKind::StreamableHttp,
                http: Some(&parts),
                client: Some(&client),
            };
            let caller = AuthenticatedCallers.resolve(&head).unwrap();
            assert_eq!(
                caller.principal,
                knowell_mcp::Principal::Authenticated {
                    principal,
                    scopes: Some(TokenScopes::read_only()),
                }
            );
            assert_eq!(caller, AuthenticatedCallers.resolve(&head).unwrap());
        }
    }

    #[test]
    fn missing_authentication_never_becomes_the_local_user() {
        for transport in [TransportKind::Stdio, TransportKind::StreamableHttp] {
            let head = RequestHead {
                transport,
                http: None,
                client: None,
            };
            assert_eq!(
                AuthenticatedCallers.resolve(&head).unwrap_err().kind(),
                "permission_denied"
            );
        }
    }
}
