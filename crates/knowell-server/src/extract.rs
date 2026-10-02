//! Request extractors that fail with problem+json instead of axum's plain
//! text rejections, and never echo request input in their messages.

use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request};
use axum::http::HeaderMap;
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::request::Parts;
use futures::StreamExt;
use knowell_auth::{Action, AuditEvent, Decision, RequestId, Resource};
use serde::de::DeserializeOwned;
use time::OffsetDateTime;

use crate::access::Authenticated;
use crate::engine::{EngineContext, Validate};
use crate::error::ApiError;
use crate::state::AppState;

/// The request id assigned by the outermost middleware.
#[derive(Debug, Clone)]
pub(crate) struct RequestIdExt(pub(crate) RequestId);

/// Reads a body of at most `limit` bytes; 413 beyond it (checked against
/// `Content-Length` first, then while streaming).
pub(crate) async fn read_limited(
    headers: &HeaderMap,
    body: Body,
    limit: usize,
) -> Result<Bytes, ApiError> {
    if let Some(length) = headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        && length > u64::try_from(limit).unwrap_or(u64::MAX)
    {
        return Err(ApiError::payload_too_large(limit));
    }
    let mut stream = body.into_data_stream();
    let mut buffer: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ApiError::invalid("the request body could not be read"))?;
        if buffer.len().saturating_add(chunk.len()) > limit {
            return Err(ApiError::payload_too_large(limit));
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buffer))
}

/// A JSON body: `Content-Type: application/json`, at most
/// `Limits::api_body_bytes`, deserialised and validated.
#[derive(Debug)]
pub(crate) struct JsonBody<T>(pub(crate) T);

impl<T> FromRequest<AppState> for JsonBody<T>
where
    T: DeserializeOwned + Validate + Send,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let (parts, body) = req.into_parts();
        let is_json = parts
            .headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"));
        if !is_json {
            return Err(ApiError::new(
                axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                "the request body must be application/json",
            ));
        }
        let bytes =
            read_limited(&parts.headers, body, state.config().limits.api_body_bytes).await?;
        let value: T = serde_json::from_slice(&bytes).map_err(json_error)?;
        value.validate()?;
        Ok(Self(value))
    }
}

/// A JSON error message with position and category only: serde's own text
/// can quote the offending value.
pub(crate) fn json_error(err: serde_json::Error) -> ApiError {
    use serde_json::error::Category;
    let what = match err.classify() {
        Category::Syntax | Category::Eof | Category::Io => "is not valid json",
        Category::Data => "does not match the expected fields and types",
    };
    ApiError::invalid(format!(
        "the request body {what} (line {}, column {})",
        err.line(),
        err.column()
    ))
}

/// Path parameters; any rejection is a 404 (a malformed id names nothing).
#[derive(Debug)]
pub(crate) struct ApiPath<T>(pub(crate) T);

impl<T, S> FromRequestParts<S> for ApiPath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Path::<T>::from_request_parts(parts, state)
            .await
            .map(|Path(v)| Self(v))
            .map_err(|_| ApiError::not_found("the requested resource does not exist"))
    }
}

/// Query parameters; any rejection is a 400 without the parser's text.
#[derive(Debug)]
pub(crate) struct ApiQuery<T>(pub(crate) T);

impl<T, S> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(v)| Self(v))
            .map_err(|_| ApiError::invalid("the query parameters are missing or malformed"))
    }
}

/// The authenticated caller plus the request id.
#[derive(Debug, Clone)]
pub(crate) struct Caller {
    pub(crate) auth: Authenticated,
    pub(crate) request_id: RequestId,
}

impl FromRequestParts<AppState> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let auth = parts
            .extensions
            .get::<Authenticated>()
            .cloned()
            .ok_or_else(|| {
                ApiError::unauthenticated("unauthenticated", "this endpoint needs authentication")
            })?;
        let request_id = request_id(parts)?;
        Ok(Self { auth, request_id })
    }
}

pub(crate) fn request_id(parts: &Parts) -> Result<RequestId, ApiError> {
    parts
        .extensions
        .get::<RequestIdExt>()
        .map(|r| r.0.clone())
        .ok_or_else(ApiError::internal)
}

impl Caller {
    /// Authorizes `action` on `resource`; records an audit event for denials
    /// and for allowed state changes; 403 when denied.
    pub(crate) fn require(
        &self,
        state: &AppState,
        action: Action,
        resource: Resource,
    ) -> Result<(), ApiError> {
        let decision = self.auth.decide(action, &resource);
        self.audit(state, action, resource, decision);
        if decision.allowed {
            Ok(())
        } else {
            Err(denied(decision))
        }
    }

    /// The resource-independent checks: the agent action ceiling and the
    /// credential's scope. Used by listings that filter by visibility.
    /// Denials are audited against the organization.
    pub(crate) fn require_scope(&self, state: &AppState, action: Action) -> Result<(), ApiError> {
        if let Err(reason) = self.auth.precheck(action) {
            let decision = Decision {
                allowed: false,
                reason,
            };
            self.audit(state, action, Resource::Organization, decision);
            return Err(denied(decision));
        }
        Ok(())
    }

    /// [`Self::require_scope`] plus a non-empty visibility, for actions whose
    /// concrete resource the engine resolves.
    pub(crate) fn precheck(&self, state: &AppState, action: Action) -> Result<(), ApiError> {
        self.require_scope(state, action)?;
        if self.auth.visible.is_empty() {
            let decision = Decision {
                allowed: false,
                reason: knowell_auth::DecisionReason::DeniedNoGrant,
            };
            self.audit(state, action, Resource::Organization, decision);
            return Err(denied(decision));
        }
        Ok(())
    }

    fn audit(&self, state: &AppState, action: Action, resource: Resource, decision: Decision) {
        let state_changing = action.token_scope() != knowell_auth::TokenScope::Read;
        if decision.allowed && !state_changing {
            return;
        }
        state.inner().audit.record(&AuditEvent {
            at: OffsetDateTime::now_utc(),
            actor: self.auth.principal.clone(),
            action,
            resource,
            decision,
            request_id: self.request_id.clone(),
        });
    }

    /// The context handed to the engine.
    pub(crate) fn engine_context(&self, state: &AppState) -> EngineContext {
        EngineContext {
            principal: self.auth.principal.clone(),
            scopes: self.auth.scopes.clone(),
            grants: self.auth.grants.clone(),
            visible: self.auth.visible.clone(),
            request_id: self.request_id.clone(),
            audit: state.inner().audit.clone(),
        }
    }
}

fn denied(decision: Decision) -> ApiError {
    use knowell_auth::DecisionReason as R;
    let message = match decision.reason {
        R::DeniedTokenScope => "the api token does not carry the scope this action needs",
        R::DeniedAgentNotPermitted => "agents may not perform this action",
        R::DeniedOrganizationOnly => "this action applies to the organization only",
        R::DeniedOverlayPrivate => "this working-tree overlay is private to its owner",
        R::DeniedInsufficientRole => "your role does not permit this action here",
        _ => "no grant covers this resource",
    };
    ApiError::forbidden(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn limited_reads() {
        let headers = HeaderMap::new();
        let ok = read_limited(&headers, Body::from("12345"), 5)
            .await
            .unwrap();
        assert_eq!(&ok[..], b"12345");
        let err = read_limited(&headers, Body::from("123456"), 5)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "payload_too_large");
        let mut announced = HeaderMap::new();
        announced.insert(CONTENT_LENGTH, "999".parse().unwrap());
        let err = read_limited(&announced, Body::from("1"), 5)
            .await
            .unwrap_err();
        assert_eq!(err.status().as_u16(), 413);
    }

    #[test]
    fn json_errors_do_not_quote_input() {
        #[derive(serde::Deserialize, Debug)]
        #[allow(dead_code)]
        struct T {
            n: u32,
        }
        let err = serde_json::from_str::<T>(r#"{"n": "KNOWELL_CANARY_value"}"#).unwrap_err();
        let api = json_error(err);
        assert!(!api.message().contains("CANARY"));
        assert!(api.message().contains("line 1"));
        let err = serde_json::from_str::<T>("{").unwrap_err();
        assert!(json_error(err).message().contains("not valid json"));
    }
}
