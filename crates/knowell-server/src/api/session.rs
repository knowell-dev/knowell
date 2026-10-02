//! `/api/v1/session`: panel session bootstrap, token login and logout.

use axum::Json;
use axum::extract::State;
use axum::http::header::{ORIGIN, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use knowell_auth::{CsrfToken, PanelSessionId, Principal};
use serde::Deserialize;

use crate::engine::Validate;
use crate::error::ApiError;
use crate::extract::JsonBody;
use crate::middleware::{normalize_origin, verify_token};
use crate::session::{SessionInfo, clear_cookie, session_ids, set_cookie};
use crate::state::AppState;
use crate::wire::SessionView;

/// `GET /api/v1/session`: returns the current session with a fresh CSRF
/// token. Without a valid session cookie, a loopback (non-hub) server with
/// a configured local user starts a session for that user; otherwise 401.
pub(super) async fn bootstrap(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let inner = state.inner();
    let existing = session_ids(&headers, inner.secure)
        .into_iter()
        .find_map(|id| inner.sessions.get(&id).map(|info| (id, info)));
    if let Some((id, info)) = existing {
        return session_response(&state, &id, &info, false);
    }
    let local_user = state
        .config()
        .local_user
        .filter(|_| state.config().local_sessions());
    let Some(user) = local_user else {
        return Err(ApiError::unauthenticated(
            "unauthenticated",
            "sign in with an api token: POST /api/v1/session/login",
        ));
    };
    let (id, info) = inner
        .sessions
        .create(Principal::User(user), None)
        .ok_or_else(|| {
            ApiError::unavailable(
                "session_unavailable",
                "a session cannot be created right now",
            )
        })?;
    session_response(&state, &id, &info, true)
}

/// `POST /api/v1/session/login` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LoginRequest {
    token: String,
}

impl Validate for LoginRequest {
    fn validate(&self) -> Result<(), ApiError> {
        if self.token.is_empty() || self.token.len() > 128 {
            return Err(ApiError::invalid("`token` must be an api token"));
        }
        Ok(())
    }
}

/// `POST /api/v1/session/login`: exchanges a user's API token for a panel
/// session bounded by the token's scopes. Must come from an allowed origin
/// (login-CSRF defence). Existing sessions presented in the request end.
pub(super) async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    JsonBody(request): JsonBody<LoginRequest>,
) -> Result<Response, ApiError> {
    let inner = state.inner();
    let origin = headers
        .get(ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(normalize_origin);
    if inner.policy.check_origin(origin.as_deref(), true).is_err() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "origin_required",
            "log in from the panel: the request must carry an allowed origin",
        ));
    }
    let verified = verify_token(&state, &request.token).await?;
    if !matches!(verified.principal, Principal::User(_)) {
        return Err(ApiError::forbidden(
            "only a user's api token can open a panel session",
        ));
    }
    for id in session_ids(&headers, inner.secure) {
        inner.sessions.remove(&id);
    }
    let (id, info) = inner
        .sessions
        .create(verified.principal, Some(verified.scopes))
        .ok_or_else(|| {
            ApiError::unavailable(
                "session_unavailable",
                "a session cannot be created right now",
            )
        })?;
    tracing::info!(token = %verified.token_id, "panel session opened with an api token");
    session_response(&state, &id, &info, true)
}

/// `POST /api/v1/session/logout`: ends the presented session (CSRF
/// protected like every cookie-authenticated mutation) and clears the cookie.
pub(super) async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let inner = state.inner();
    for id in session_ids(&headers, inner.secure) {
        inner.sessions.remove(&id);
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    if let Ok(value) = HeaderValue::from_str(&clear_cookie(inner.secure)) {
        response.headers_mut().append(SET_COOKIE, value);
    }
    Ok(response)
}

fn session_response(
    state: &AppState,
    id: &PanelSessionId,
    info: &SessionInfo,
    new_cookie: bool,
) -> Result<Response, ApiError> {
    let inner = state.inner();
    let csrf = CsrfToken::generate(&inner.csrf_key, id).map_err(|_| ApiError::internal())?;
    let body = SessionView {
        user: info.principal.to_string(),
        role: state.config().role,
        csrf_token: csrf.expose().to_owned(),
        expires_at: info.expires_at,
    };
    let mut response = Json(body).into_response();
    if new_cookie {
        let cookie = set_cookie(id, inner.secure, state.config().sessions.absolute_lifetime);
        let value = HeaderValue::from_str(&cookie).map_err(|_| ApiError::internal())?;
        response.headers_mut().append(SET_COOKIE, value);
    }
    Ok(response)
}
