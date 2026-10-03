//! The [`Engine`] facade: operations the store alone cannot answer (search,
//! graph, context packing, memory, tasks, profiles, usage, …).
//!
//! The server validates and types every request ([`EngineRequest`]), checks
//! authentication, CSRF, the agent action ceiling and the credential's scope,
//! and hands the engine an [`EngineContext`] with the caller's principal,
//! grants and visible projects. The engine enforces resource-level
//! permissions inside search, graph expansion, context packing and memory
//! (it calls `knowell_auth::authorize` on the concrete resource) and records
//! audit events for its own state changes.
//!
//! Responses are JSON values in the panel's shapes (`panel/src/lib/api/types.ts`);
//! the server checks only the top-level kind (array for lists, object
//! otherwise) and never invents data: without an engine every delegated
//! route answers `503 engine_unavailable` with the reason.

use std::sync::Arc;

use knowell_auth::{GrantSet, Principal, ProjectFilter, RequestId, TokenScopes};
use serde::Deserialize;

use crate::access::BoxFuture;
use crate::audit::AuditSink;
use crate::error::ApiError;

/// The engine behind delegated routes. Implemented by the `know` binary over
/// the query engine, knowledge store and providers.
pub trait Engine: Send + Sync + 'static {
    /// Answers one request for the caller described by `ctx`.
    fn call<'a>(
        &'a self,
        ctx: &'a EngineContext,
        request: EngineRequest,
    ) -> BoxFuture<'a, Result<serde_json::Value, EngineError>>;
}

/// Who is asking, for permission enforcement inside the engine.
#[derive(Clone)]
pub struct EngineContext {
    /// The acting principal.
    pub principal: Principal,
    /// The credential's scopes; `None` for a local panel session.
    pub scopes: Option<TokenScopes>,
    /// Grants to evaluate with `knowell_auth::authorize`.
    pub grants: Arc<GrantSet>,
    /// Projects the principal may read; filter inside search, never after.
    pub visible: ProjectFilter,
    /// Correlation id of the HTTP request (for audit events and logs).
    pub request_id: RequestId,
    /// Where the engine records audit events for its state changes.
    pub audit: Arc<dyn AuditSink>,
}

impl std::fmt::Debug for EngineContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineContext")
            .field("principal", &self.principal)
            .field("scopes", &self.scopes)
            .field("visible", &self.visible)
            .field("request_id", &self.request_id)
            .finish_non_exhaustive()
    }
}

/// Engine failures, mapped to problem responses.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    /// The engine cannot answer this request now (not configured, index not
    /// ready, provider down). → 503 `engine_unavailable` with the reason.
    #[error("engine unavailable: {reason}")]
    Unavailable {
        /// Secret-free reason shown to the caller.
        reason: String,
    },
    /// The addressed item does not exist (or is not visible). → 404.
    #[error("not found: {what}")]
    NotFound {
        /// What was not found, e.g. `memory record`.
        what: String,
    },
    /// The request is well-formed but not acceptable. → 400.
    #[error("invalid request: {message}")]
    Invalid {
        /// Secret-free explanation.
        message: String,
    },
    /// The engine's authorization check denied the action. → 403.
    #[error("forbidden: {message}")]
    Forbidden {
        /// Secret-free explanation.
        message: String,
    },
    /// The request conflicts with the current state. → 409.
    #[error("conflict: {message}")]
    Conflict {
        /// Secret-free explanation.
        message: String,
    },
    /// Anything else. → 500; the message is logged, not returned.
    #[error("engine failure: {message}")]
    Internal {
        /// Secret-free description for the server log.
        message: String,
    },
}

impl From<EngineError> for ApiError {
    fn from(err: EngineError) -> Self {
        match err {
            EngineError::Unavailable { reason } => ApiError::engine_unavailable(reason),
            EngineError::NotFound { what } => {
                ApiError::not_found(format!("the {what} does not exist"))
            }
            EngineError::Invalid { message } => ApiError::invalid(message),
            EngineError::Forbidden { message } => ApiError::forbidden(message),
            EngineError::Conflict { message } => ApiError::conflict("conflict", message),
            EngineError::Internal { message } => {
                tracing::error!(error = %message, "engine request failed");
                ApiError::internal()
            }
        }
    }
}

/// A validated request for the engine.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum EngineRequest {
    /// Extra health details: an object whose optional keys `freshness`,
    /// `recentErrors` and `resources` are copied into `/api/v1/health`.
    HealthDetail,
    /// Hybrid search (`SearchResponse`).
    Search(SearchRequest),
    /// A graph slice (`GraphSlice`).
    Graph(GraphQuery),
    /// Graph insights (`GraphInsight[]`).
    GraphInsights,
    /// Flow over evidenced relations (`trace_flow`).
    Trace(TraceRequest),
    /// Impact analysis of a symbol, file or patch (`analyze_impact`).
    Impact(ImpactRequest),
    /// A context pack for a task and token budget (`build_context`).
    Context(ContextRequest),
    /// Domains (`Domain[]`).
    Domains,
    /// Glossary (`GlossaryTerm[]`).
    Glossary,
    /// Memory records (`MemoryRecord[]`).
    Memory,
    /// Accept or reject a proposed memory record (`MemoryRecord`).
    DecideMemory(MemoryDecision),
    /// Tasks (`TaskRecord[]`).
    Tasks,
    /// Architecture rules (`ArchRule[]`).
    Rules,
    /// Embedding profiles (`EmbeddingProfile[]`).
    Profiles,
    /// Cost and impact of switching profiles (`SwitchEstimate`).
    SwitchEstimate {
        /// Target profile id.
        to_profile_id: String,
    },
    /// Start a blue-green profile switch (answer: any object).
    StartSwitch(SwitchRequest),
    /// Profile switches, newest first (`ProfileMigration[]`).
    Switches,
    /// Cancel a building profile switch; the old profile keeps serving
    /// (answer: the switch).
    CancelSwitch {
        /// Switch id.
        switch_id: String,
    },
    /// Roll back an active profile switch within its retention (answer: the
    /// reverse switch).
    RollbackSwitch {
        /// Switch id.
        switch_id: String,
    },
    /// Evaluation reports (`EvalReport[]`).
    EvalReports,
    /// Usage over the last `days` days (`UsageReport`).
    Usage {
        /// Period length, 1..=365.
        days: u32,
    },
    /// MCP / agent / webhook integration status (`IntegrationsStatus`).
    Integrations,
    /// Users, tokens and audit log for the hub (`AdminOverview`).
    Admin,
}

impl EngineRequest {
    /// Stable snake_case name, for logs.
    pub fn name(&self) -> &'static str {
        match self {
            Self::HealthDetail => "health_detail",
            Self::Search(_) => "search",
            Self::Graph(_) => "graph",
            Self::GraphInsights => "graph_insights",
            Self::Trace(_) => "trace",
            Self::Impact(_) => "impact",
            Self::Context(_) => "context",
            Self::Domains => "domains",
            Self::Glossary => "glossary",
            Self::Memory => "memory",
            Self::DecideMemory(_) => "decide_memory",
            Self::Tasks => "tasks",
            Self::Rules => "rules",
            Self::Profiles => "profiles",
            Self::SwitchEstimate { .. } => "switch_estimate",
            Self::StartSwitch(_) => "start_switch",
            Self::Switches => "switches",
            Self::CancelSwitch { .. } => "cancel_switch",
            Self::RollbackSwitch { .. } => "rollback_switch",
            Self::EvalReports => "eval_reports",
            Self::Usage { .. } => "usage",
            Self::Integrations => "integrations",
            Self::Admin => "admin",
        }
    }
}

/// Validation of untrusted request bodies. Messages name the field and the
/// rule, never the value.
pub(crate) trait Validate {
    fn validate(&self) -> Result<(), ApiError>;
}

const MAX_ID_LEN: usize = 128;
const MAX_IDS: usize = 200;

fn check_text(field: &str, value: &str, max_chars: usize, required: bool) -> Result<(), ApiError> {
    if required && value.trim().is_empty() {
        return Err(ApiError::invalid(format!("`{field}` must not be empty")));
    }
    if value.chars().count() > max_chars {
        return Err(ApiError::invalid(format!(
            "`{field}` must be at most {max_chars} characters"
        )));
    }
    if value
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t' && c != '\r')
    {
        return Err(ApiError::invalid(format!(
            "`{field}` must not contain control characters"
        )));
    }
    Ok(())
}

fn check_ids(field: &str, ids: Option<&[String]>) -> Result<(), ApiError> {
    let Some(ids) = ids else { return Ok(()) };
    if ids.len() > MAX_IDS {
        return Err(ApiError::invalid(format!(
            "`{field}` may list at most {MAX_IDS} entries"
        )));
    }
    for id in ids {
        check_text(field, id, MAX_ID_LEN, true)?;
    }
    Ok(())
}

/// `POST /api/v1/search` body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchRequest {
    /// The query, 1..=4096 characters.
    pub query: String,
    /// Restrict to these project ids.
    #[serde(default)]
    pub project_ids: Option<Vec<String>>,
    /// Restrict to these languages.
    #[serde(default)]
    pub languages: Option<Vec<String>>,
    /// Restrict to paths under this prefix.
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// Include graph expansion (callers, tests, contracts).
    #[serde(default)]
    pub expand_graph: bool,
    /// Rerank the short list (when a reranker is configured).
    #[serde(default)]
    pub rerank: bool,
    /// Results wanted, 1..=200 (default 20).
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_limit() -> u32 {
    20
}

impl Validate for SearchRequest {
    fn validate(&self) -> Result<(), ApiError> {
        check_text("query", &self.query, 4096, true)?;
        check_ids("projectIds", self.project_ids.as_deref())?;
        if let Some(languages) = &self.languages {
            if languages.len() > 64 {
                return Err(ApiError::invalid("`languages` may list at most 64 entries"));
            }
            for language in languages {
                check_text("languages", language, 64, true)?;
            }
        }
        if let Some(prefix) = &self.path_prefix {
            check_text("pathPrefix", prefix, 1024, false)?;
        }
        if !(1..=200).contains(&self.limit) {
            return Err(ApiError::invalid("`limit` must be between 1 and 200"));
        }
        Ok(())
    }
}

/// Graph display mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GraphMode {
    /// Service → module → symbol drill-down.
    Hierarchy,
    /// Contract map.
    Contracts,
}

/// `GET /api/v1/graph?mode=&parent=` query.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphQuery {
    /// Display mode.
    pub mode: GraphMode,
    /// Node to expand; absent for the top-level service map.
    #[serde(default)]
    pub parent: Option<String>,
}

impl Validate for GraphQuery {
    fn validate(&self) -> Result<(), ApiError> {
        if let Some(parent) = &self.parent {
            check_text("parent", parent, 512, true)?;
        }
        Ok(())
    }
}

/// Direction of a trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraceDirection {
    /// Who reaches the start node.
    Callers,
    /// What the start node reaches.
    Callees,
    /// Both.
    Both,
}

/// `POST /api/v1/graph/trace` body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TraceRequest {
    /// Start: a symbol, endpoint, topic or graph node id.
    pub from: String,
    /// Direction (default both).
    #[serde(default = "default_direction")]
    pub direction: TraceDirection,
    /// Hops, 1..=5 (default 3).
    #[serde(default = "default_depth")]
    pub depth: u8,
    /// Restrict to these project ids.
    #[serde(default)]
    pub project_ids: Option<Vec<String>>,
}

fn default_direction() -> TraceDirection {
    TraceDirection::Both
}

fn default_depth() -> u8 {
    3
}

impl Validate for TraceRequest {
    fn validate(&self) -> Result<(), ApiError> {
        check_text("from", &self.from, 1024, true)?;
        if !(1..=5).contains(&self.depth) {
            return Err(ApiError::invalid("`depth` must be between 1 and 5"));
        }
        check_ids("projectIds", self.project_ids.as_deref())
    }
}

/// `POST /api/v1/graph/impact` body: a target, an unapplied patch, or both.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImpactRequest {
    /// A symbol, file path or contract.
    #[serde(default)]
    pub target: Option<String>,
    /// A unified diff that is not applied anywhere.
    #[serde(default)]
    pub patch: Option<String>,
    /// Restrict to these project ids.
    #[serde(default)]
    pub project_ids: Option<Vec<String>>,
}

impl Validate for ImpactRequest {
    fn validate(&self) -> Result<(), ApiError> {
        if self.target.is_none() && self.patch.is_none() {
            return Err(ApiError::invalid("give `target`, `patch` or both"));
        }
        if let Some(target) = &self.target {
            check_text("target", target, 1024, true)?;
        }
        if let Some(patch) = &self.patch
            && patch.trim().is_empty()
        {
            return Err(ApiError::invalid("`patch` must not be empty"));
        }
        check_ids("projectIds", self.project_ids.as_deref())
    }
}

/// `POST /api/v1/context` body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextRequest {
    /// The task to gather context for, 1..=8192 characters.
    pub task: String,
    /// Token budget of the pack, 256..=200000.
    pub token_budget: u32,
    /// Restrict to these project ids.
    #[serde(default)]
    pub project_ids: Option<Vec<String>>,
}

impl Validate for ContextRequest {
    fn validate(&self) -> Result<(), ApiError> {
        check_text("task", &self.task, 8192, true)?;
        if !(256..=200_000).contains(&self.token_budget) {
            return Err(ApiError::invalid(
                "`tokenBudget` must be between 256 and 200000",
            ));
        }
        check_ids("projectIds", self.project_ids.as_deref())
    }
}

/// Memory review decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryAction {
    /// Accept the proposal.
    Accept,
    /// Reject the proposal.
    Reject,
}

/// `POST /api/v1/memory/{id}/decision` body (the id comes from the path; a
/// body `id`, if present, must match it).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryDecision {
    /// Memory record id.
    #[serde(default)]
    pub id: String,
    /// Accept or reject.
    pub action: MemoryAction,
    /// Optional reviewer note, at most 4000 characters.
    #[serde(default)]
    pub note: Option<String>,
}

impl Validate for MemoryDecision {
    /// An empty `id` passes here: the route fills it from the path.
    fn validate(&self) -> Result<(), ApiError> {
        if !self.id.is_empty() {
            check_text("id", &self.id, MAX_ID_LEN, true)?;
        }
        if let Some(note) = &self.note {
            check_text("note", note, 4000, false)?;
        }
        Ok(())
    }
}

/// `POST /api/v1/profiles/switch` body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SwitchRequest {
    /// Target profile id.
    pub to_profile_id: String,
}

impl Validate for SwitchRequest {
    fn validate(&self) -> Result<(), ApiError> {
        check_text("toProfileId", &self.to_profile_id, MAX_ID_LEN, true)
    }
}

/// Checks a switch id taken from a request path.
pub(crate) fn validate_switch_id(id: &str) -> Result<(), ApiError> {
    check_text("switch id", id, MAX_ID_LEN, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search(json: serde_json::Value) -> Result<SearchRequest, serde_json::Error> {
        serde_json::from_value(json)
    }

    #[test]
    fn search_validation() {
        let ok = search(serde_json::json!({"query": "where is auth", "expandGraph": true, "rerank": false, "limit": 10})).unwrap();
        assert!(ok.validate().is_ok());
        let defaults = search(serde_json::json!({"query": "x"})).unwrap();
        assert_eq!(defaults.limit, 20);
        for bad in [
            serde_json::json!({"query": "  "}),
            serde_json::json!({"query": "x", "limit": 0}),
            serde_json::json!({"query": "x", "limit": 201}),
            serde_json::json!({"query": "a\u{0000}b"}),
            serde_json::json!({"query": "x".repeat(4097)}),
            serde_json::json!({"query": "x", "projectIds": vec!["p"; 201]}),
            serde_json::json!({"query": "x", "languages": [""]}),
        ] {
            let req = search(bad.clone()).unwrap();
            let err = req.validate().unwrap_err();
            assert_eq!(err.code(), "invalid_request", "{bad}");
        }
        assert!(search(serde_json::json!({"query": "x", "unknown": 1})).is_err());
    }

    #[test]
    fn validation_messages_never_echo_values() {
        let secret = "KNOWELL_CANARY_secret_value";
        let req = SearchRequest {
            query: format!("{secret}\u{0007}"),
            project_ids: None,
            languages: None,
            path_prefix: None,
            expand_graph: false,
            rerank: false,
            limit: 1,
        };
        let err = req.validate().unwrap_err();
        assert!(!err.message().contains(secret));
    }

    #[test]
    fn other_requests() {
        let trace: TraceRequest =
            serde_json::from_value(serde_json::json!({"from": "svc.Pay"})).unwrap();
        assert_eq!((trace.depth, trace.direction), (3, TraceDirection::Both));
        assert!(trace.validate().is_ok());
        let deep: TraceRequest =
            serde_json::from_value(serde_json::json!({"from": "a", "depth": 9})).unwrap();
        assert!(deep.validate().is_err());

        let impact: ImpactRequest = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(impact.validate().is_err());
        let impact: ImpactRequest =
            serde_json::from_value(serde_json::json!({"patch": "--- a\n+++ b\n"})).unwrap();
        assert!(impact.validate().is_ok());

        let ctx: ContextRequest =
            serde_json::from_value(serde_json::json!({"task": "fix", "tokenBudget": 100})).unwrap();
        assert!(ctx.validate().is_err());

        let decision: MemoryDecision =
            serde_json::from_value(serde_json::json!({"id": "m1", "action": "accept"})).unwrap();
        assert!(decision.validate().is_ok());
        assert!(
            serde_json::from_value::<MemoryDecision>(
                serde_json::json!({"id": "m1", "action": "maybe"})
            )
            .is_err()
        );

        let switch: SwitchRequest =
            serde_json::from_value(serde_json::json!({"toProfileId": ""})).unwrap();
        assert!(switch.validate().is_err());

        let graph: GraphQuery =
            serde_json::from_value(serde_json::json!({"mode": "contracts"})).unwrap();
        assert!(graph.validate().is_ok());
    }

    #[test]
    fn engine_errors_map_to_problems() {
        let cases = [
            (
                EngineError::Unavailable {
                    reason: "index not ready".into(),
                },
                503,
                "engine_unavailable",
            ),
            (
                EngineError::NotFound {
                    what: "memory record".into(),
                },
                404,
                "not_found",
            ),
            (
                EngineError::Invalid {
                    message: "bad".into(),
                },
                400,
                "invalid_request",
            ),
            (
                EngineError::Forbidden {
                    message: "no".into(),
                },
                403,
                "forbidden",
            ),
            (
                EngineError::Conflict {
                    message: "busy".into(),
                },
                409,
                "conflict",
            ),
            (
                EngineError::Internal {
                    message: "boom".into(),
                },
                500,
                "internal_error",
            ),
        ];
        for (err, status, code) in cases {
            let api = ApiError::from(err);
            assert_eq!((api.status().as_u16(), api.code()), (status, code));
        }
        let internal = ApiError::from(EngineError::Internal {
            message: "detail-only-in-log".into(),
        });
        assert!(!internal.message().contains("detail-only-in-log"));
    }
}
