//! Route table and router assembly (see the crate README for the full table).

mod catalog;
mod delegated;
mod health;
mod indexes;
mod progress;
mod session;

use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tower_http::catch_panic::CatchPanicLayer;

use crate::error::ApiError;
use crate::middleware::{
    host_origin, panic_response, request_id, require_auth, security_headers, timeout,
};
use crate::state::AppState;
use crate::{mcp, panel, webhooks};

pub use indexes::JOB_KIND_VIEW_REINDEX;

/// Builds the complete router: `/api/v1`, `/mcp` (when an MCP router was
/// given), webhooks and the panel, wrapped in the security middleware.
///
/// Layers, outermost first: request id → security headers → panic catcher →
/// `Host`/`Origin` allow-list → (per group) timeout / MCP body limit →
/// authentication with CSRF → handler (body limits, authorization, audit).
pub fn build_router(state: AppState) -> Router {
    let mut router = Router::new().nest("/api/v1", api_v1(&state));
    if let Some(mcp_router) = state.inner().mcp.clone() {
        router = router.merge(mcp::routes(&state, mcp_router));
    }
    router
        .fallback(fallback)
        .layer(from_fn_with_state(state.clone(), host_origin))
        .layer(CatchPanicLayer::custom(panic_response))
        .layer(from_fn_with_state(state.clone(), security_headers))
        .layer(from_fn(request_id))
        .with_state(state)
}

fn api_v1(state: &AppState) -> Router<AppState> {
    let public = Router::new()
        .route("/session", get(session::bootstrap))
        .route("/session/login", post(session::login))
        .route("/health/live", get(health::live))
        .route("/webhooks/{provider}", post(webhooks::receive));
    let protected = Router::new()
        .route("/session/logout", post(session::logout))
        .route("/health", get(health::health))
        .route("/workspaces", get(catalog::list_workspaces))
        .route("/workspaces/{id}", get(catalog::get_workspace))
        .route("/projects", get(catalog::list_projects))
        .route("/projects/{id}", get(catalog::get_project))
        .route("/indexes", get(indexes::overview))
        .route("/indexes/reindex", post(indexes::reindex))
        .route("/jobs", get(indexes::list_jobs))
        .route("/jobs/{id}/retry", post(indexes::retry))
        .route("/events", get(progress::stream))
        .route("/search", post(delegated::search))
        .route("/graph", get(delegated::graph))
        .route("/graph/insights", get(delegated::graph_insights))
        .route("/graph/trace", post(delegated::trace))
        .route("/graph/impact", post(delegated::impact))
        .route("/context", post(delegated::context))
        .route("/domains", get(delegated::domains))
        .route("/glossary", get(delegated::glossary))
        .route("/memory", get(delegated::memory))
        .route("/memory/{id}/decision", post(delegated::decide_memory))
        .route("/tasks", get(delegated::tasks))
        .route("/rules", get(delegated::rules))
        .route("/profiles", get(delegated::profiles))
        .route(
            "/profiles/{id}/switch-estimate",
            get(delegated::switch_estimate),
        )
        .route("/profiles/switch", post(delegated::start_switch))
        .route("/quality/reports", get(delegated::eval_reports))
        .route("/usage", get(delegated::usage))
        .route("/integrations", get(delegated::integrations))
        .route("/admin", get(delegated::admin))
        .route_layer(from_fn_with_state(state.clone(), require_auth));
    public
        .merge(protected)
        .layer(from_fn_with_state(state.clone(), timeout))
}

/// Unknown `/api` paths are 404 problems; everything else is the panel.
async fn fallback(State(state): State<AppState>, req: Request) -> Response {
    let path = req.uri().path();
    if path == "/api" || path.starts_with("/api/") || path == "/mcp" || path.starts_with("/mcp/") {
        return ApiError::not_found("the requested resource does not exist").into_response();
    }
    panel::serve(&state.inner().panel, req.method(), path).await
}
