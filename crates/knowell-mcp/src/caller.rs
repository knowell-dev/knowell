//! Who is calling: the request-context seam for authentication.
//!
//! Every tool call, prompt and resource read is resolved to a [`Caller`] by a
//! [`CallerResolver`] before the engine sees it, and the caller is passed to
//! every [`crate::KnowellTools`] method so that permissions can be enforced in
//! the engine (architecture §15). The default [`LocalOnly`] resolver accepts
//! the local user over stdio and over loopback HTTP; a hub replaces it with a
//! token- or OIDC-based resolver (a later milestone) without touching the tool
//! code.

use std::net::SocketAddr;

use axum::extract::ConnectInfo;
use axum::http::request::Parts;

use crate::error::ToolError;

/// Transport a request arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportKind {
    /// A byte stream: stdio, or an in-process duplex in tests.
    Stdio,
    /// Streamable HTTP.
    StreamableHttp,
}

/// The authenticated identity behind a request.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Principal {
    /// The local user of this machine (stdio, or loopback HTTP without
    /// authentication).
    LocalUser,
    /// A subject authenticated by a resolver (token, OIDC, …).
    Subject {
        /// Stable subject id from the identity provider or token store.
        id: String,
    },
    /// Typed identity supplied by trusted authentication middleware for
    /// this request, including the presented credential's restrictions.
    /// Never construct this from MCP arguments or client metadata.
    Authenticated {
        /// Verified user, service account or delegated agent.
        principal: knowell_auth::Principal,
        /// Credential scopes; `None` only for an unrestricted session.
        scopes: Option<knowell_auth::TokenScopes>,
    },
}

/// The MCP client, as it identified itself (self-reported, not authenticated).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientIdentity {
    /// Client name, e.g. `claude-code`.
    pub name: String,
    /// Client version.
    pub version: String,
}

/// Identity and origin of one request, passed to every tool method.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Caller {
    /// Who is calling.
    pub principal: Principal,
    /// How the request arrived.
    pub transport: TransportKind,
    /// The client application, when it identified itself.
    pub client: Option<ClientIdentity>,
}

impl Caller {
    /// The local user on the given transport, without client information.
    pub fn local(transport: TransportKind) -> Self {
        Self {
            principal: Principal::LocalUser,
            transport,
            client: None,
        }
    }
}

/// What a resolver can inspect about a request.
#[derive(Debug, Clone, Copy)]
pub struct RequestHead<'a> {
    /// Transport the request arrived on.
    pub transport: TransportKind,
    /// HTTP request head (method, URI, headers, extensions), for Streamable HTTP.
    pub http: Option<&'a Parts>,
    /// Client name and version from the MCP handshake or request metadata.
    pub client: Option<&'a ClientIdentity>,
}

/// Resolves a request to a [`Caller`] or rejects it.
///
/// Implementations must not log credentials, and their errors must not echo
/// them; return [`ToolError::PermissionDenied`] for rejected requests.
pub trait CallerResolver: Send + Sync + 'static {
    /// Identifies the caller of one request.
    fn resolve(&self, head: &RequestHead<'_>) -> Result<Caller, ToolError>;
}

/// Default resolver: the local user only.
///
/// Stdio requests are accepted. HTTP requests are accepted as the local user
/// when the peer address (axum's `ConnectInfo<SocketAddr>`, if the server
/// records it) is a loopback address; the Streamable HTTP layer additionally
/// rejects non-loopback `Host` headers and browser `Origin`s by default.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalOnly;

impl CallerResolver for LocalOnly {
    fn resolve(&self, head: &RequestHead<'_>) -> Result<Caller, ToolError> {
        if head.transport == TransportKind::StreamableHttp {
            let peer = head
                .http
                .and_then(|parts| parts.extensions.get::<ConnectInfo<SocketAddr>>())
                .map(|info| info.0);
            if let Some(addr) = peer
                && !addr.ip().is_loopback()
            {
                return Err(ToolError::permission_denied(
                    "this server only accepts local connections; remote access needs a hub with authentication",
                ));
            }
        }
        Ok(Caller {
            principal: Principal::LocalUser,
            transport: head.transport,
            client: head.client.cloned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use axum::http::Request;

    use super::*;

    fn parts_from(addr: Option<&str>) -> Parts {
        let mut request = Request::builder().uri("/mcp").body(()).unwrap();
        if let Some(addr) = addr {
            let addr: SocketAddr = addr.parse().unwrap();
            request.extensions_mut().insert(ConnectInfo(addr));
        }
        request.into_parts().0
    }

    #[test]
    fn local_only_accepts_stdio_and_loopback() {
        let client = ClientIdentity {
            name: "test-client".into(),
            version: "1.0".into(),
        };
        let stdio = RequestHead {
            transport: TransportKind::Stdio,
            http: None,
            client: Some(&client),
        };
        let caller = LocalOnly.resolve(&stdio).unwrap();
        assert_eq!(caller.principal, Principal::LocalUser);
        assert_eq!(
            caller.client.as_ref().map(|c| c.name.as_str()),
            Some("test-client")
        );

        for addr in [None, Some("127.0.0.1:5000"), Some("[::1]:5000")] {
            let parts = parts_from(addr);
            let head = RequestHead {
                transport: TransportKind::StreamableHttp,
                http: Some(&parts),
                client: None,
            };
            assert!(LocalOnly.resolve(&head).is_ok(), "{addr:?}");
        }
    }

    #[test]
    fn local_only_rejects_remote_peers() {
        let parts = parts_from(Some("192.0.2.10:5000"));
        let head = RequestHead {
            transport: TransportKind::StreamableHttp,
            http: Some(&parts),
            client: None,
        };
        let error = LocalOnly.resolve(&head).unwrap_err();
        assert_eq!(error.kind(), "permission_denied");
        assert!(!error.client_message("1").contains("192.0.2.10"));
    }
}
