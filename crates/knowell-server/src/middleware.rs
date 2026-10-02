//! The security middleware. Order, outermost first (see `routes.rs`):
//!
//! 1. [`request_id`] — assigns a UUIDv7 request id, returns it in
//!    `X-Request-Id`, adds it to every problem body and turns bare error
//!    statuses (router 404/405, …) into problem+json.
//! 2. [`security_headers`] — `nosniff`, `no-referrer`, `DENY` framing, COOP/
//!    CORP `same-origin`, a CSP (the panel's own for panel files, `default-src
//!    'none'` otherwise), HSTS over https, `no-store` for API responses.
//! 3. Panic catcher — a panic anywhere below answers 500 problem+json.
//! 4. [`host_origin`] — DNS-rebinding defence: the `Host` must be on the
//!    allow-list; an `Origin`, when present, must be too (on every method).
//! 5. Per route group: [`timeout`] for `/api/v1` and webhooks, the MCP body
//!    pre-check, then [`require_auth`] / [`require_mcp_auth`]: a bearer API
//!    token or a session cookie; cookie-authenticated state-changing requests
//!    additionally need an allowed `Origin` and a valid `X-Knowell-CSRF`.
//! 6. In handlers: body size limits and validation, authorization of the
//!    concrete action (`knowell_auth::authorize`), audit events.

use std::any::Any;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE, HOST,
    ORIGIN, REFERRER_POLICY, STRICT_TRANSPORT_SECURITY, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use knowell_auth::{
    Action, AuditEvent, CsrfToken, Decision, PanelError, RequestId, Resource, TokenError,
    VerifiedToken, token_prefix, verify, visible_projects,
};
use time::OffsetDateTime;

use crate::access::{AuthMethod, Authenticated};
use crate::config::with_default_port;
use crate::error::{ApiError, ProblemMarker, problem_body};
use crate::extract::RequestIdExt;
use crate::session::session_ids;
use crate::state::AppState;

/// Header carrying the CSRF token on state-changing panel requests.
pub const CSRF_HEADER: &str = "x-knowell-csrf";
/// Response header carrying the request id.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

const API_CSP: &str =
    "default-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

/// Layer 1: request id and problem decoration.
pub(crate) async fn request_id(mut req: Request, next: Next) -> Response {
    let Ok(id) = RequestId::new(uuid::Uuid::now_v7().hyphenated().to_string()) else {
        return ApiError::internal().into_response();
    };
    req.extensions_mut().insert(RequestIdExt(id.clone()));
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let started = Instant::now();

    let mut response = next.run(req).await;
    let status = response.status();
    let marker = response.extensions().get::<ProblemMarker>().cloned();
    let replacement = match marker {
        Some(m) => Some(problem_body(
            m.status,
            m.code,
            &m.message,
            Some(id.as_str()),
        )),
        None if (status.is_client_error() || status.is_server_error())
            && !response.headers().contains_key(CONTENT_TYPE) =>
        {
            let err = ApiError::for_status(status);
            Some(problem_body(
                status,
                err.code(),
                err.message(),
                Some(id.as_str()),
            ))
        }
        None => None,
    };
    if let Some(body) = replacement {
        *response.body_mut() = Body::from(body);
        let headers = response.headers_mut();
        headers.remove(CONTENT_LENGTH);
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static(crate::error::PROBLEM_JSON),
        );
    }
    if let Ok(value) = HeaderValue::from_str(id.as_str()) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(REQUEST_ID_HEADER), value);
    }
    tracing::debug!(
        request_id = id.as_str(),
        %method,
        path = %path,
        status = response.status().as_u16(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "request"
    );
    response
}

/// Layer 2: security headers on every response.
pub(crate) async fn security_headers(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
    if !headers.contains_key(CONTENT_SECURITY_POLICY) {
        headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(API_CSP));
    }
    if !headers.contains_key(CACHE_CONTROL) {
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    if state.inner().secure {
        headers.insert(
            STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    response
}

/// Layer 3: the panic catcher's response (the payload may hold anything, so
/// it is not logged).
pub(crate) fn panic_response(_payload: Box<dyn Any + Send + 'static>) -> Response {
    tracing::error!("a request handler panicked");
    ApiError::internal().into_response()
}

/// Layer 4: `Host` and `Origin` allow-list (DNS-rebinding defence).
pub(crate) async fn host_origin(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let headers = req.headers();
    let default_port = state.config().default_host_port();
    let host = match single(headers, HOST.as_str()) {
        Single::One(h) => with_default_port(&h.to_ascii_lowercase(), default_port),
        Single::None => req
            .uri()
            .authority()
            .and_then(|a| with_default_port(&a.as_str().to_ascii_lowercase(), default_port)),
        Single::Many => {
            return ApiError::new(
                StatusCode::BAD_REQUEST,
                "host_invalid",
                "send exactly one host header",
            )
            .into_response();
        }
    };
    match state.inner().policy.check_host(host.as_deref()) {
        Ok(()) => {}
        Err(PanelError::MissingHost) => {
            return ApiError::new(
                StatusCode::BAD_REQUEST,
                "host_missing",
                "the host header is missing",
            )
            .into_response();
        }
        Err(_) => {
            return ApiError::new(
                StatusCode::FORBIDDEN,
                "host_not_allowed",
                "the host header is not allowed; open the panel through 127.0.0.1, localhost or a configured host name",
            )
            .into_response();
        }
    }
    let origin = match origin_header(headers) {
        Ok(origin) => origin,
        Err(err) => return err.into_response(),
    };
    if state
        .inner()
        .policy
        .check_origin(origin.as_deref(), false)
        .is_err()
    {
        return origin_rejected().into_response();
    }
    next.run(req).await
}

enum Single<'a> {
    None,
    One(&'a str),
    Many,
}

/// The value of a header that must appear at most once; a non-text value
/// counts as malformed (`Many`).
fn single<'a>(headers: &'a HeaderMap, name: &str) -> Single<'a> {
    let mut values = headers.get_all(name).iter();
    match (values.next(), values.next()) {
        (None, _) => Single::None,
        (Some(v), None) => v.to_str().map(Single::One).unwrap_or(Single::Many),
        (Some(_), Some(_)) => Single::Many,
    }
}

/// The `Origin` header, lowercased and with an explicit port; `None` when
/// absent.
fn origin_header(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    match single(headers, ORIGIN.as_str()) {
        Single::None => Ok(None),
        Single::One(origin) => Ok(Some(normalize_origin(origin))),
        Single::Many => Err(origin_rejected()),
    }
}

/// `https://Host` → `https://host:443`; anything unusual is returned
/// lowercased but otherwise untouched (the allow-list then rejects it).
pub(crate) fn normalize_origin(origin: &str) -> String {
    let lower = origin.to_ascii_lowercase();
    let (scheme, port) = if lower.starts_with("https://") {
        ("https://", 443)
    } else if lower.starts_with("http://") {
        ("http://", 80)
    } else {
        return lower;
    };
    let authority = lower.get(scheme.len()..).unwrap_or_default();
    if authority.contains(['/', '?', '#', '@']) {
        return lower;
    }
    match with_default_port(authority, port) {
        Some(a) => format!("{scheme}{a}"),
        None => lower,
    }
}

fn origin_rejected() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "origin_not_allowed",
        "the request origin is not allowed",
    )
}

/// Layer 5a: request timeout (time to response headers).
pub(crate) async fn timeout(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let limit = state.config().limits.request_timeout;
    match tokio::time::timeout(limit, next.run(req)).await {
        Ok(response) => response,
        Err(_) => ApiError::unavailable(
            "timeout",
            format!("the request did not finish within {} ms", limit.as_millis()),
        )
        .into_response(),
    }
}

/// Layer 5b: MCP request body limit. A body announced (by `Content-Length`
/// or its known size) beyond the limit is rejected with 413 up front; any
/// other body is cut off with a read error once it exceeds the limit, which
/// the MCP transport reports as a failed request.
pub(crate) async fn mcp_body_limit(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let limit = state.config().limits.mcp_body_bytes;
    let limit_u64 = u64::try_from(limit).unwrap_or(u64::MAX);
    let announced = req
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let known = axum::body::HttpBody::size_hint(req.body()).lower();
    if announced.is_some_and(|n| n > limit_u64) || known > limit_u64 {
        return ApiError::payload_too_large(limit).into_response();
    }
    let (parts, body) = req.into_parts();
    let mut seen: usize = 0;
    let limited = futures::StreamExt::map(body.into_data_stream(), move |chunk| {
        let chunk = chunk?;
        seen = seen.saturating_add(chunk.len());
        if seen > limit {
            Err(axum::Error::new(BodyTooLarge))
        } else {
            Ok(chunk)
        }
    });
    next.run(Request::from_parts(parts, Body::from_stream(limited)))
        .await
}

/// Read error raised when a streamed MCP body passes the limit.
#[derive(Debug, thiserror::Error)]
#[error("the request body exceeds the configured limit")]
struct BodyTooLarge;

/// Layer 5c: authentication for `/api/v1` routes.
pub(crate) async fn require_auth(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let (mut parts, body) = req.into_parts();
    match authenticate(&state, &parts.method, &parts.headers).await {
        Ok(auth) => {
            parts.extensions.insert(auth);
            next.run(Request::from_parts(parts, body)).await
        }
        Err(err) => err.into_response(),
    }
}

/// Layer 5c for `/mcp`: authentication plus the `use_mcp` pre-check. The
/// [`Authenticated`] caller is left in the request extensions for the MCP
/// server's caller resolver.
pub(crate) async fn require_mcp_auth(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let (mut parts, body) = req.into_parts();
    let auth = match authenticate(&state, &parts.method, &parts.headers).await {
        Ok(auth) => auth,
        Err(err) => return err.into_response(),
    };
    let denial = match auth.precheck(Action::UseMcp) {
        Err(reason) => Some(reason),
        Ok(()) if auth.visible.is_empty() => Some(knowell_auth::DecisionReason::DeniedNoGrant),
        Ok(()) => None,
    };
    if let Some(reason) = denial {
        if let Some(RequestIdExt(id)) = parts.extensions.get::<RequestIdExt>() {
            state.inner().audit.record(&AuditEvent {
                at: OffsetDateTime::now_utc(),
                actor: auth.principal.clone(),
                action: Action::UseMcp,
                resource: Resource::Organization,
                decision: Decision {
                    allowed: false,
                    reason,
                },
                request_id: id.clone(),
            });
        }
        return ApiError::forbidden("this principal may not use mcp").into_response();
    }
    parts.extensions.insert(auth);
    next.run(Request::from_parts(parts, body)).await
}

fn is_state_changing(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Authenticates a request by bearer token or session cookie.
pub(crate) async fn authenticate(
    state: &AppState,
    method: &Method,
    headers: &HeaderMap,
) -> Result<Authenticated, ApiError> {
    let inner = state.inner();
    let (principal, scopes, method) = match single(headers, AUTHORIZATION.as_str()) {
        Single::One(value) => {
            let token = parse_bearer(value).ok_or_else(|| {
                ApiError::unauthenticated(
                    "invalid_token",
                    "send the api token as `Authorization: Bearer kn_…`",
                )
            })?;
            let verified = verify_token(state, token).await?;
            (
                verified.principal,
                Some(verified.scopes),
                AuthMethod::Token {
                    token_id: verified.token_id,
                },
            )
        }
        Single::Many => {
            return Err(ApiError::unauthenticated(
                "invalid_token",
                "send exactly one authorization header",
            ));
        }
        Single::None => {
            let found = session_ids(headers, inner.secure)
                .into_iter()
                .find_map(|id| inner.sessions.get(&id).map(|info| (id, info)));
            let Some((id, info)) = found else {
                return Err(ApiError::unauthenticated(
                    "unauthenticated",
                    "no valid session; reload the panel or send an api token",
                ));
            };
            if is_state_changing(method) {
                check_browser_mutation(state, headers, &id)?;
            }
            (
                info.principal,
                info.scopes,
                AuthMethod::Session {
                    fingerprint: id.fingerprint(),
                },
            )
        }
    };
    let grants = inner.tokens.grants_for(&principal).await.map_err(|err| {
        tracing::warn!(error = %err, "grant lookup failed");
        ApiError::unavailable(
            "access_unavailable",
            "permissions cannot be loaded right now",
        )
    })?;
    let visible = visible_projects(&principal, &grants);
    Ok(Authenticated {
        principal,
        scopes,
        grants: std::sync::Arc::new(grants),
        visible,
        method,
    })
}

/// A cookie-authenticated state change must come from an allowed origin and
/// carry the session's CSRF token.
pub(crate) fn check_browser_mutation(
    state: &AppState,
    headers: &HeaderMap,
    session: &knowell_auth::PanelSessionId,
) -> Result<(), ApiError> {
    let inner = state.inner();
    let origin = origin_header(headers)?;
    match inner.policy.check_origin(origin.as_deref(), true) {
        Ok(()) => {}
        Err(PanelError::MissingOrigin) => {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "origin_required",
                "state-changing panel requests must carry an origin header",
            ));
        }
        Err(_) => return Err(origin_rejected()),
    }
    let presented = match single(headers, CSRF_HEADER) {
        Single::None => None,
        Single::One(v) => Some(v),
        Single::Many => Some(""),
    };
    match CsrfToken::validate(&inner.csrf_key, session, presented) {
        Ok(()) => Ok(()),
        Err(PanelError::CsrfMissing) if presented.is_none() => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "csrf_missing",
            "state-changing requests must carry the x-knowell-csrf header from /api/v1/session",
        )),
        Err(_) => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "csrf_invalid",
            "the csrf token does not match this session; reload the panel",
        )),
    }
}

/// `Bearer <token>` (scheme case-insensitive).
fn parse_bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.trim().split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() && !token.contains(' '))
        .then_some(token)
}

/// Verifies a presented token against the stored candidates.
pub(crate) async fn verify_token(state: &AppState, token: &str) -> Result<VerifiedToken, ApiError> {
    let inner = state.inner();
    let Some(pepper) = inner.pepper.as_ref() else {
        return Err(ApiError::unauthenticated(
            "invalid_token",
            "api token authentication is not configured on this server",
        ));
    };
    let Some(prefix) = token_prefix(token) else {
        return Err(ApiError::unauthenticated(
            "invalid_token",
            "the api token is malformed",
        ));
    };
    let candidates = inner
        .tokens
        .tokens_with_prefix(&prefix)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "token lookup failed");
            ApiError::unavailable("access_unavailable", "tokens cannot be checked right now")
        })?;
    let now = OffsetDateTime::now_utc();
    let mut outcome = TokenError::Invalid;
    for stored in &candidates {
        match verify(token, stored, pepper, now) {
            Ok(verified) => {
                inner.tokens.token_used(verified.token_id, now).await;
                return Ok(verified);
            }
            // Revoked / expired are only reported for the matching record.
            Err(err @ (TokenError::Revoked | TokenError::Expired)) => outcome = err,
            Err(_) => {}
        }
    }
    Err(match outcome {
        TokenError::Revoked => {
            ApiError::unauthenticated("token_revoked", "the api token has been revoked")
        }
        TokenError::Expired => {
            ApiError::unauthenticated("token_expired", "the api token has expired")
        }
        _ => ApiError::unauthenticated("invalid_token", "the api token is not valid"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_parsing() {
        assert_eq!(parse_bearer("Bearer kn_abc"), Some("kn_abc"));
        assert_eq!(parse_bearer("bearer   kn_abc "), Some("kn_abc"));
        for bad in [
            "kn_abc",
            "Basic kn_abc",
            "Bearer",
            "Bearer ",
            "Bearer a b",
            "",
        ] {
            assert_eq!(parse_bearer(bad), None, "{bad}");
        }
    }

    #[test]
    fn origin_normalization() {
        assert_eq!(
            normalize_origin("http://LOCALHOST:7420"),
            "http://localhost:7420"
        );
        assert_eq!(
            normalize_origin("https://kn.example"),
            "https://kn.example:443"
        );
        assert_eq!(
            normalize_origin("http://kn.example"),
            "http://kn.example:80"
        );
        assert_eq!(normalize_origin("http://[::1]"), "http://[::1]:80");
        assert_eq!(normalize_origin("null"), "null");
        assert_eq!(normalize_origin("http://a@b:1"), "http://a@b:1");
        assert_eq!(normalize_origin("http://b:1/x"), "http://b:1/x");
    }
}
