//! Routes answered by the [`Engine`](crate::Engine). Without an engine they
//! answer `503 engine_unavailable` with the configured reason.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use knowell_auth::{Action, Resource};
use knowell_config::ServerRole;
use serde::Deserialize;
use serde_json::Value;

use crate::engine::{
    ContextRequest, EngineRequest, GraphQuery, ImpactRequest, MemoryDecision, SearchRequest,
    SwitchRequest, TraceRequest, Validate,
};
use crate::error::ApiError;
use crate::extract::{ApiPath, ApiQuery, Caller, JsonBody};
use crate::state::AppState;

/// Expected top-level JSON kind of an engine answer.
#[derive(Clone, Copy)]
enum Shape {
    Array,
    Object,
}

/// Calls the engine and checks the answer's top-level shape.
async fn call(
    state: &AppState,
    caller: &Caller,
    request: EngineRequest,
    shape: Shape,
) -> Result<Json<Value>, ApiError> {
    let engine = state.engine()?;
    let name = request.name();
    let ctx = caller.engine_context(state);
    let value = engine.call(&ctx, request).await.map_err(ApiError::from)?;
    match (shape, &value) {
        (Shape::Array, Value::Array(_)) | (Shape::Object, Value::Object(_)) => Ok(Json(value)),
        _ => {
            tracing::error!(request = name, "engine answered with the wrong json shape");
            Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "engine_bad_response",
                "the engine returned a malformed answer; see the server log",
            ))
        }
    }
}

/// Reads: the action's scope check plus some visible project; the engine
/// filters by the caller's visibility.
async fn read(
    state: &AppState,
    caller: &Caller,
    action: Action,
    request: EngineRequest,
    shape: Shape,
) -> Result<Json<Value>, ApiError> {
    caller.precheck(state, action)?;
    call(state, caller, request, shape).await
}

/// `POST /api/v1/search`.
pub(super) async fn search(
    State(state): State<AppState>,
    caller: Caller,
    JsonBody(request): JsonBody<SearchRequest>,
) -> Result<Json<Value>, ApiError> {
    read(
        &state,
        &caller,
        Action::ReadCode,
        EngineRequest::Search(request),
        Shape::Object,
    )
    .await
}

/// `GET /api/v1/graph?mode=hierarchy|contracts&parent=`.
pub(super) async fn graph(
    State(state): State<AppState>,
    caller: Caller,
    ApiQuery(query): ApiQuery<GraphQuery>,
) -> Result<Json<Value>, ApiError> {
    query.validate()?;
    read(
        &state,
        &caller,
        Action::ReadCode,
        EngineRequest::Graph(query),
        Shape::Object,
    )
    .await
}

/// `GET /api/v1/graph/insights`.
pub(super) async fn graph_insights(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Value>, ApiError> {
    read(
        &state,
        &caller,
        Action::ReadCode,
        EngineRequest::GraphInsights,
        Shape::Array,
    )
    .await
}

/// `POST /api/v1/graph/trace`.
pub(super) async fn trace(
    State(state): State<AppState>,
    caller: Caller,
    JsonBody(request): JsonBody<TraceRequest>,
) -> Result<Json<Value>, ApiError> {
    read(
        &state,
        &caller,
        Action::ReadCode,
        EngineRequest::Trace(request),
        Shape::Object,
    )
    .await
}

/// `POST /api/v1/graph/impact`.
pub(super) async fn impact(
    State(state): State<AppState>,
    caller: Caller,
    JsonBody(request): JsonBody<ImpactRequest>,
) -> Result<Json<Value>, ApiError> {
    read(
        &state,
        &caller,
        Action::ReadCode,
        EngineRequest::Impact(request),
        Shape::Object,
    )
    .await
}

/// `POST /api/v1/context`.
pub(super) async fn context(
    State(state): State<AppState>,
    caller: Caller,
    JsonBody(request): JsonBody<ContextRequest>,
) -> Result<Json<Value>, ApiError> {
    read(
        &state,
        &caller,
        Action::ReadCode,
        EngineRequest::Context(request),
        Shape::Object,
    )
    .await
}

/// `GET /api/v1/domains`.
pub(super) async fn domains(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Value>, ApiError> {
    read(
        &state,
        &caller,
        Action::ReadCode,
        EngineRequest::Domains,
        Shape::Array,
    )
    .await
}

/// `GET /api/v1/glossary`.
pub(super) async fn glossary(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Value>, ApiError> {
    read(
        &state,
        &caller,
        Action::ReadCode,
        EngineRequest::Glossary,
        Shape::Array,
    )
    .await
}

/// `GET /api/v1/memory`.
pub(super) async fn memory(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Value>, ApiError> {
    read(
        &state,
        &caller,
        Action::ReadMemory,
        EngineRequest::Memory,
        Shape::Array,
    )
    .await
}

/// `POST /api/v1/memory/{id}/decision`: the engine authorizes the record's
/// concrete scope and audits the transition.
pub(super) async fn decide_memory(
    State(state): State<AppState>,
    caller: Caller,
    ApiPath(id): ApiPath<String>,
    JsonBody(mut decision): JsonBody<MemoryDecision>,
) -> Result<Json<Value>, ApiError> {
    if decision.id.is_empty() {
        decision.id = id;
    } else if decision.id != id {
        return Err(ApiError::invalid("the body `id` does not match the path"));
    }
    decision.validate()?;
    caller.precheck(&state, Action::AcceptMemory)?;
    call(
        &state,
        &caller,
        EngineRequest::DecideMemory(decision),
        Shape::Object,
    )
    .await
}

/// `GET /api/v1/tasks`.
pub(super) async fn tasks(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Value>, ApiError> {
    read(
        &state,
        &caller,
        Action::ReadMemory,
        EngineRequest::Tasks,
        Shape::Array,
    )
    .await
}

/// `GET /api/v1/rules`.
pub(super) async fn rules(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Value>, ApiError> {
    read(
        &state,
        &caller,
        Action::ReadCode,
        EngineRequest::Rules,
        Shape::Array,
    )
    .await
}

/// `GET /api/v1/profiles`.
pub(super) async fn profiles(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Value>, ApiError> {
    read(
        &state,
        &caller,
        Action::ReadCode,
        EngineRequest::Profiles,
        Shape::Array,
    )
    .await
}

/// `GET /api/v1/profiles/{id}/switch-estimate`.
pub(super) async fn switch_estimate(
    State(state): State<AppState>,
    caller: Caller,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<Value>, ApiError> {
    SwitchRequest {
        to_profile_id: id.clone(),
    }
    .validate()?;
    let request = EngineRequest::SwitchEstimate { to_profile_id: id };
    read(&state, &caller, Action::ReadCode, request, Shape::Object).await
}

/// `POST /api/v1/profiles/switch`: organization administrators only;
/// answers 202 when the engine accepted the switch.
pub(super) async fn start_switch(
    State(state): State<AppState>,
    caller: Caller,
    JsonBody(request): JsonBody<SwitchRequest>,
) -> Result<Response, ApiError> {
    caller.require(&state, Action::ManageProviders, Resource::Organization)?;
    let Json(value) = call(
        &state,
        &caller,
        EngineRequest::StartSwitch(request),
        Shape::Object,
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(value)).into_response())
}

/// `GET /api/v1/quality/reports`.
pub(super) async fn eval_reports(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Value>, ApiError> {
    read(
        &state,
        &caller,
        Action::ReadCode,
        EngineRequest::EvalReports,
        Shape::Array,
    )
    .await
}

/// `GET /api/v1/usage?days=` query.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UsageQuery {
    days: u32,
}

/// `GET /api/v1/usage?days=1..365`: organization-wide readers only.
pub(super) async fn usage(
    State(state): State<AppState>,
    caller: Caller,
    ApiQuery(query): ApiQuery<UsageQuery>,
) -> Result<Json<Value>, ApiError> {
    if !(1..=365).contains(&query.days) {
        return Err(ApiError::invalid("`days` must be between 1 and 365"));
    }
    caller.require(&state, Action::ReadCode, Resource::Organization)?;
    call(
        &state,
        &caller,
        EngineRequest::Usage { days: query.days },
        Shape::Object,
    )
    .await
}

/// `GET /api/v1/integrations`: organization-wide readers only.
pub(super) async fn integrations(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Action::ReadCode, Resource::Organization)?;
    call(&state, &caller, EngineRequest::Integrations, Shape::Object).await
}

/// `GET /api/v1/admin`: outside the hub role administration does not exist
/// (`available: false`, empty lists); on a hub, user managers only.
pub(super) async fn admin(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Value>, ApiError> {
    let role = state.config().role;
    if role != ServerRole::Hub {
        return Ok(Json(serde_json::json!({
            "available": false,
            "role": role,
            "users": [],
            "tokens": [],
            "audit": [],
        })));
    }
    caller.require(&state, Action::ManageUsers, Resource::Organization)?;
    call(&state, &caller, EngineRequest::Admin, Shape::Object).await
}
