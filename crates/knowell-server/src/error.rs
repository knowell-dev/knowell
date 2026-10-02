//! Errors: [`ServerError`] for configuration and start-up, [`ApiError`] for
//! request handling, rendered as RFC 7807 `application/problem+json`.
//!
//! Every message is lowercase, actionable and built from fixed text: request
//! input (headers, bodies, tokens, paths) and connection strings are never
//! echoed. The request id is added by the outermost middleware so that every
//! problem, including those produced by inner layers, carries it.

use std::borrow::Cow;

use axum::http::header::{ALLOW, CONTENT_TYPE, RETRY_AFTER, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// Media type of problem responses (RFC 7807).
pub const PROBLEM_JSON: &str = "application/problem+json";

/// Errors from configuring, building or running the server.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServerError {
    /// The configuration violates a rule; the message names the setting.
    #[error("invalid server configuration: {0}")]
    Config(String),
    /// Binding the listen address failed.
    #[error("cannot listen on {address}: {source}")]
    Bind {
        /// The address that was requested.
        address: std::net::SocketAddr,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The operating system could not supply randomness for session keys.
    #[error("operating system randomness is unavailable")]
    Entropy,
    /// The HTTP server stopped with an I/O error.
    #[error("http server failed: {0}")]
    Serve(#[source] std::io::Error),
}

/// A request failure, rendered as problem+json with a stable `code`.
///
/// The body has the RFC 7807 members `type` (`urn:knowell:problem:<code>`),
/// `title`, `status` and `detail`, plus the extension members the panel reads:
/// `code`, `message` (same text as `detail`) and `requestId`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: Cow<'static, str>,
    extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Extra {
    www_authenticate: Option<&'static str>,
    retry_after_secs: Option<u32>,
    allow: Option<&'static str>,
}

/// Marker the request-id middleware uses to re-render a problem with the
/// request id.
#[derive(Debug, Clone)]
pub(crate) struct ProblemMarker {
    pub(crate) status: StatusCode,
    pub(crate) code: &'static str,
    pub(crate) message: Cow<'static, str>,
}

impl ApiError {
    /// A problem with an explicit status, stable code and safe message.
    pub fn new(
        status: StatusCode,
        code: &'static str,
        message: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            extra: Extra::default(),
        }
    }

    /// HTTP status.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// Stable machine code, e.g. `engine_unavailable`.
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// Human readable, secret-free message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// 400 `invalid_request`.
    pub fn invalid(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    /// 401 with a `WWW-Authenticate: Bearer` challenge.
    pub fn unauthenticated(code: &'static str, message: impl Into<Cow<'static, str>>) -> Self {
        let mut err = Self::new(StatusCode::UNAUTHORIZED, code, message);
        err.extra.www_authenticate = Some(match code {
            "unauthenticated" => "Bearer realm=\"knowell\"",
            _ => "Bearer realm=\"knowell\", error=\"invalid_token\"",
        });
        err
    }

    /// 403 `forbidden` (authorization denied).
    pub fn forbidden(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    /// 404 `not_found`.
    pub fn not_found(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    /// 405 `method_not_allowed`, with the `Allow` header when known.
    pub(crate) fn method_not_allowed(allow: Option<&'static str>) -> Self {
        let mut err = Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "this endpoint does not support the request method",
        );
        err.extra.allow = allow;
        err
    }

    /// 409 `conflict`.
    pub fn conflict(code: &'static str, message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    /// 413 `payload_too_large`.
    pub fn payload_too_large(limit_bytes: usize) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!("the request body exceeds the limit of {limit_bytes} bytes"),
        )
    }

    /// 503 with a `Retry-After` hint, for temporary unavailability.
    pub fn unavailable(code: &'static str, message: impl Into<Cow<'static, str>>) -> Self {
        let mut err = Self::new(StatusCode::SERVICE_UNAVAILABLE, code, message);
        err.extra.retry_after_secs = Some(5);
        err
    }

    /// 503 `engine_unavailable` with the reason the engine cannot answer.
    pub fn engine_unavailable(reason: impl Into<Cow<'static, str>>) -> Self {
        Self::unavailable("engine_unavailable", reason)
    }

    /// 500 `internal_error`; the cause is logged by the caller, never sent.
    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "the engine reported an internal error; see the server log for the request id",
        )
    }

    /// The generic problem for a bare error status produced outside the
    /// handlers (for example by the router itself).
    pub(crate) fn for_status(status: StatusCode) -> Self {
        match status {
            StatusCode::NOT_FOUND => Self::not_found("the requested resource does not exist"),
            StatusCode::METHOD_NOT_ALLOWED => Self::method_not_allowed(None),
            StatusCode::PAYLOAD_TOO_LARGE => {
                Self::new(status, "payload_too_large", "the request body is too large")
            }
            StatusCode::UNSUPPORTED_MEDIA_TYPE => Self::new(
                status,
                "unsupported_media_type",
                "the request body must be application/json",
            ),
            s if s.is_server_error() => Self::new(
                s,
                "server_error",
                "the engine reported an internal error; see the server log for the request id",
            ),
            s => Self::new(s, "http_error", "the request was rejected"),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({}): {}",
            self.code,
            self.status.as_u16(),
            self.message
        )
    }
}

impl std::error::Error for ApiError {}

#[derive(Serialize)]
struct ProblemBody<'a> {
    #[serde(rename = "type")]
    kind: String,
    title: &'a str,
    status: u16,
    detail: &'a str,
    code: &'a str,
    message: &'a str,
    #[serde(rename = "requestId", skip_serializing_if = "Option::is_none")]
    request_id: Option<&'a str>,
}

/// Serialises a problem body. Falls back to a fixed body if serialisation
/// fails (it cannot for these field types).
pub(crate) fn problem_body(
    status: StatusCode,
    code: &str,
    message: &str,
    request_id: Option<&str>,
) -> Vec<u8> {
    let body = ProblemBody {
        kind: format!("urn:knowell:problem:{code}"),
        title: status.canonical_reason().unwrap_or("Error"),
        status: status.as_u16(),
        detail: message,
        code,
        message,
        request_id,
    };
    serde_json::to_vec(&body).unwrap_or_else(|_| {
        b"{\"type\":\"urn:knowell:problem:internal_error\",\"title\":\"Internal Server Error\",\"status\":500,\"code\":\"internal_error\",\"message\":\"internal error\",\"detail\":\"internal error\"}".to_vec()
    })
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = problem_body(self.status, self.code, &self.message, None);
        let mut response = (self.status, body).into_response();
        let headers: &mut HeaderMap = response.headers_mut();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
        if let Some(challenge) = self.extra.www_authenticate {
            headers.insert(WWW_AUTHENTICATE, HeaderValue::from_static(challenge));
        }
        if let Some(secs) = self.extra.retry_after_secs {
            headers.insert(RETRY_AFTER, HeaderValue::from(secs));
        }
        if let Some(allow) = self.extra.allow {
            headers.insert(ALLOW, HeaderValue::from_static(allow));
        }
        response.extensions_mut().insert(ProblemMarker {
            status: self.status,
            code: self.code,
            message: self.message,
        });
        response
    }
}

impl From<knowell_store::StoreError> for ApiError {
    fn from(err: knowell_store::StoreError) -> Self {
        use knowell_store::StoreError as E;
        match err {
            E::NotFound { entity, .. } => Self::not_found(format!("the {entity} does not exist")),
            E::AlreadyExists { entity, .. } => {
                Self::conflict("already_exists", format!("the {entity} already exists"))
            }
            E::InvalidInput(_) => Self::invalid("the store rejected the request input"),
            E::Conflict { .. } => Self::conflict(
                "conflict",
                "the record was changed by someone else; reload it and retry",
            ),
            E::GenerationBusy { .. } | E::StaleGeneration { .. } => Self::conflict(
                "generation_conflict",
                "the view is changing generations; retry shortly",
            ),
            other => {
                // Store errors never carry the connection URL or password
                // (knowell-store scrubs them), so the cause may be logged.
                tracing::warn!(error = %other, "store request failed");
                Self::unavailable(
                    "store_unavailable",
                    "the database is unavailable or failed; see the server log",
                )
            }
        }
    }
}
