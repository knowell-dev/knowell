//! `impl knowell_server::Engine for Engine`: the REST operations behind the
//! panel, answered in the shapes of `panel/src/lib/api/types.ts`.
//!
//! Every request acts as the caller of the HTTP request (principal, grants
//! and token scopes from [`EngineContext`]); the same pinning and
//! visibility rules as the MCP tools apply, so an invisible project never
//! appears in any answer. Bodies the panel does not define (trace, impact,
//! context, switch) are the MCP tool outputs; the crate README documents
//! them.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, PoisonError};
use std::time::Instant;

use knowell_auth::{Action, AuditEvent, Resource};
use knowell_config::{Origin, ResolvedWorkspace, Sourced};
use knowell_core::{Name, RepoPath};
use knowell_embed::Embedder;
use knowell_graph::{InsightCode, InsightConfig};
use knowell_index::{EmbeddingPlan, Priority, SyncOutcome};
use knowell_knowledge::{Actor, KnowledgeRecord, RecordId, RecordKind, RecordState, Rights, Scope};
use knowell_mcp::tools::{
    AnalyzeImpactInput, BuildContextInput, ChangeSubject, FlowDirection, TraceFlowInput,
};
use knowell_mcp::{SymbolRef, Target};
use knowell_query::{Reason, SourceKind};
use knowell_server::{
    EngineContext, EngineError, EngineRequest, GraphMode, GraphQuery, ImpactRequest, MemoryAction,
    MemoryDecision, SearchRequest, SwitchRequest, TraceDirection, TraceRequest,
};
use knowell_store::embeddings::{self, EmbeddingProfile};
use knowell_store::{ProfileId, audit as store_audit, identity};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::access::Access;
use crate::engine::{Engine, WorkspaceEntry};
use crate::error::tool_to_rest;
use crate::evidence::{freshness_of, place, reasons};
use crate::memory::RecordQuery;
use crate::scope::Pinned;
use crate::search::Filters;
use crate::snapshot::slice_lines;

/// Visible workspaces with the project names a `projectIds` filter selects.
type Targets = Vec<(Arc<WorkspaceEntry>, Option<BTreeSet<Name>>)>;

/// Glossary terms: domain, synonyms and code names per term.
type GlossaryTerms = BTreeMap<String, (Option<Name>, Vec<Value>, Vec<String>)>;

/// A blue/green profile switch started by this process.
#[derive(Debug, Clone)]
pub(crate) struct SwitchRecord {
    pub(crate) id: Uuid,
    pub(crate) from: Option<ProfileId>,
    pub(crate) to: ProfileId,
    pub(crate) views: Vec<knowell_store::ViewId>,
    pub(crate) started: OffsetDateTime,
}

fn iso(at: OffsetDateTime) -> Value {
    at.format(&Rfc3339).map_or(Value::Null, Value::String)
}

fn internal(message: impl Into<String>) -> EngineError {
    EngineError::Internal {
        message: message.into(),
    }
}

fn store_err(error: knowell_store::StoreError) -> EngineError {
    internal(format!("store: {error}"))
}

fn tier_label(tier: knowell_mcp::FreshnessTier) -> &'static str {
    match tier {
        knowell_mcp::FreshnessTier::T0Text => "T0",
        knowell_mcp::FreshnessTier::T1Symbols => "T1",
        knowell_mcp::FreshnessTier::T2Embeddings => "T2",
        knowell_mcp::FreshnessTier::T3Relations => "T3",
    }
}

fn evidence_label(e: knowell_query::EvidenceType) -> &'static str {
    match e {
        knowell_query::EvidenceType::SemanticallyResolved => "semantically-resolved",
        knowell_query::EvidenceType::ContractDerived => "contract-derived",
        knowell_query::EvidenceType::RuntimeObservation => "runtime-observation",
        knowell_query::EvidenceType::SyntacticObservation => "syntactic",
        knowell_query::EvidenceType::HeuristicMatch => "heuristic",
        knowell_query::EvidenceType::ModelSuggestion => "model-suggestion",
    }
}

fn query_class_label(intent: knowell_query::Intent) -> &'static str {
    match intent {
        knowell_query::Intent::ExactSymbol | knowell_query::Intent::PathOrFile => "exact-symbol",
        knowell_query::Intent::Endpoint => "endpoint",
        knowell_query::Intent::ErrorTrace => "error-trace",
        knowell_query::Intent::Behavior => "behavior",
        knowell_query::Intent::Impact => "impact",
        knowell_query::Intent::Why => "why",
    }
}

fn panel_reason(reason: &Reason, symbol: Option<&str>) -> Option<Value> {
    Some(match reason {
        Reason::ExactMatch { term, .. } => json!({
            "type": "exact-symbol",
            "symbol": symbol.unwrap_or(term),
        }),
        Reason::LexicalTerms { terms } => json!({"type": "lexical", "terms": terms}),
        Reason::SemanticSimilarity { similarity, .. } => {
            json!({"type": "semantic", "similarity": similarity})
        }
        Reason::GraphPath { seed, steps } => {
            let mut path = vec![format!("{}:{}", seed.project, seed.path)];
            path.extend(
                steps
                    .iter()
                    .map(|s| format!("{}:{}", s.to.project, s.to.path)),
            );
            let edge = steps
                .iter()
                .map(|s| s.evidence)
                .max()
                .map_or("syntactic", evidence_label);
            json!({"type": "graph", "path": path, "edge": edge})
        }
        Reason::TestReferences { subject } => json!({"type": "test-reference", "target": subject}),
        Reason::GlossaryExpansion { .. } | Reason::PersonalOverlay { .. } => return None,
    })
}

fn panel_state(state: RecordState) -> &'static str {
    match state {
        RecordState::Proposed => "proposed",
        RecordState::Accepted => "accepted",
        RecordState::Rejected => "rejected",
        RecordState::Stale => "stale",
        RecordState::Superseded => "superseded",
    }
}

fn panel_kind(kind: RecordKind) -> &'static str {
    match kind {
        RecordKind::Observed => "observed",
        RecordKind::Human => "human",
        RecordKind::ModelSuggestion => "agent-finding",
    }
}

fn panel_author(actor: &Actor) -> Value {
    match actor {
        Actor::Human(user) => json!({"type": "human", "name": format!("user:{user}")}),
        Actor::Agent { session, client } => {
            json!({"type": "agent", "name": client.to_string(), "session": session.to_string()})
        }
        Actor::System => json!({"type": "engine", "name": "knowell"}),
    }
}

fn scope_label(scope: &Scope) -> (&'static str, String) {
    match scope {
        Scope::Organization => ("organization", "organization".to_owned()),
        Scope::Workspace(w) => ("workspace", w.to_string()),
        Scope::Project { workspace, project } => ("project", format!("{workspace}/{project}")),
        Scope::Task(t) => ("task", t.to_string()),
        Scope::User(u) => ("user", u.to_string()),
    }
}

impl knowell_server::Engine for Engine {
    fn call<'a>(
        &'a self,
        ctx: &'a EngineContext,
        request: EngineRequest,
    ) -> knowell_server::BoxFuture<'a, Result<Value, EngineError>> {
        Box::pin(async move {
            let access = Access::new(ctx.principal.clone(), Arc::clone(&ctx.grants))
                .with_scopes(ctx.scopes.clone());
            self.rest(&access, ctx, request).await
        })
    }
}

impl Engine {
    async fn rest(
        &self,
        access: &Access,
        ctx: &EngineContext,
        request: EngineRequest,
    ) -> Result<Value, EngineError> {
        let organization_action = match &request {
            EngineRequest::Profiles
            | EngineRequest::SwitchEstimate { .. }
            | EngineRequest::EvalReports
            | EngineRequest::Usage { .. }
            | EngineRequest::Integrations => Some(Action::ReadCode),
            EngineRequest::StartSwitch(_) => Some(Action::ManageProviders),
            EngineRequest::Admin if self.inner.settings.role == knowell_config::ServerRole::Hub => {
                Some(Action::ManageUsers)
            }
            _ => None,
        };
        // Native requests must enforce the same boundary as HTTP before a
        // selector, stored record or report can reveal organization metadata.
        if organization_action.is_some_and(|action| !access.allows(action, &Resource::Organization))
        {
            return Err(EngineError::Forbidden {
                message: "organization permission is required for this operation".to_owned(),
            });
        }
        match request {
            EngineRequest::HealthDetail => self.rest_health(access).await,
            EngineRequest::Search(request) => self.rest_search(access, request).await,
            EngineRequest::Graph(query) => self.rest_graph(access, query).await,
            EngineRequest::GraphInsights => self.rest_insights(access).await,
            EngineRequest::Trace(request) => self.rest_trace(access, request).await,
            EngineRequest::Impact(request) => self.rest_impact(access, request).await,
            EngineRequest::Context(request) => {
                let pinned = self.rest_single_workspace(access, request.project_ids.as_deref())?;
                let output = self
                    .tool_build_context(
                        access.clone(),
                        BuildContextInput {
                            target: Target::workspace(pinned, Vec::new()),
                            task: Some(request.task.chars().take(4000).collect()),
                            token_budget: Some(request.token_budget),
                            ..BuildContextInput::default()
                        },
                    )
                    .await
                    .map_err(tool_to_rest)?;
                serde_json::to_value(output).map_err(|e| internal(e.to_string()))
            }
            EngineRequest::Domains => self.rest_domains(access).await,
            EngineRequest::Glossary => Ok(self.rest_glossary()),
            EngineRequest::Memory => self.rest_memory(access).await,
            EngineRequest::DecideMemory(decision) => self.rest_decide(access, ctx, decision).await,
            EngineRequest::Tasks => self.rest_tasks(access).await,
            EngineRequest::Rules => self.rest_rules(access).await,
            EngineRequest::Profiles => self.rest_profiles(access).await,
            EngineRequest::SwitchEstimate { to_profile_id } => {
                self.rest_switch_estimate(access, &to_profile_id).await
            }
            EngineRequest::StartSwitch(request) => self.rest_start_switch(access, request).await,
            EngineRequest::EvalReports => self.rest_eval_reports(),
            EngineRequest::Usage { days } => {
                if !(1..=365).contains(&days) {
                    return Err(EngineError::Invalid {
                        message: "`days` must be between 1 and 365".to_owned(),
                    });
                }
                Ok(self.inner.usage.report(days, OffsetDateTime::now_utc()))
            }
            EngineRequest::Integrations => Ok(self.rest_integrations()),
            EngineRequest::Admin => self.rest_admin().await,
            _ => Err(EngineError::Invalid {
                message: "this request is not supported by this engine version".to_owned(),
            }),
        }
    }

    /// Workspaces the caller can see, each with the project names a
    /// `projectIds` filter selects in it (`None` = all).
    fn rest_targets(
        &self,
        access: &Access,
        project_ids: Option<&[String]>,
    ) -> Result<Targets, EngineError> {
        let visible: Vec<Arc<WorkspaceEntry>> = self
            .all_workspaces()
            .into_iter()
            .filter(|w| {
                w.projects
                    .iter()
                    .any(|p| access.reads_project(&w.name, &p.name))
            })
            .collect();
        let Some(ids) = project_ids else {
            return Ok(visible.into_iter().map(|w| (w, None)).collect());
        };
        let mut by_workspace: BTreeMap<Name, (Arc<WorkspaceEntry>, BTreeSet<Name>)> =
            BTreeMap::new();
        for id in ids {
            let uuid = Uuid::parse_str(id).map_err(|_| EngineError::NotFound {
                what: "project".to_owned(),
            })?;
            let found = visible.iter().find_map(|w| {
                w.projects
                    .iter()
                    .find(|p| p.id.0 == uuid && access.reads_project(&w.name, &p.name))
                    .map(|p| (Arc::clone(w), p.name.clone()))
            });
            let Some((workspace, project)) = found else {
                return Err(EngineError::NotFound {
                    what: "project".to_owned(),
                });
            };
            by_workspace
                .entry(workspace.name.clone())
                .or_insert_with(|| (workspace, BTreeSet::new()))
                .1
                .insert(project);
        }
        Ok(by_workspace
            .into_values()
            .map(|(w, projects)| (w, Some(projects)))
            .collect())
    }

    /// The one workspace a request without a workspace refers to.
    fn rest_single_workspace(
        &self,
        access: &Access,
        project_ids: Option<&[String]>,
    ) -> Result<Name, EngineError> {
        let targets = self.rest_targets(access, project_ids)?;
        match targets.as_slice() {
            [(workspace, _)] => Ok(workspace.name.clone()),
            [] => Err(EngineError::NotFound {
                what: "workspace".to_owned(),
            }),
            _ => Err(EngineError::Invalid {
                message: "several workspaces are visible; pass projectIds of one workspace"
                    .to_owned(),
            }),
        }
    }

    async fn rest_pin(&self, access: &Access, workspace: &Name) -> Result<Pinned, EngineError> {
        self.pin(access, Some(workspace), &[])
            .await
            .map_err(tool_to_rest)
    }

    async fn rest_health(&self, access: &Access) -> Result<Value, EngineError> {
        let mut views = 0u64;
        let mut done = [0u64; 4];
        let mut lags: Vec<u64> = Vec::new();
        let mut errors = Vec::new();
        for workspace in self.all_workspaces() {
            for project in &workspace.projects {
                if !access.reads_project(&workspace.name, &project.name) {
                    continue;
                }
                let Ok(status) = self.inner.indexer.status(project.view).await else {
                    continue;
                };
                views = views.saturating_add(1);
                for (i, state) in [
                    &status.tiers.t0,
                    &status.tiers.t1,
                    &status.tiers.t2,
                    &status.tiers.t3,
                ]
                .into_iter()
                .enumerate()
                {
                    if *state == knowell_index::TierState::Done
                        && let Some(slot) = done.get_mut(i)
                    {
                        *slot = slot.saturating_add(1);
                    }
                }
                lags.push(
                    status
                        .lag
                        .map_or(0, |l| u64::try_from(l.as_millis()).unwrap_or(u64::MAX)),
                );
                if let Some(error) = status.last_error {
                    errors.push(json!({
                        "at": iso(OffsetDateTime::now_utc()),
                        "code": "index_failed",
                        "message": error,
                        "projectId": project.id.to_string(),
                    }));
                }
            }
        }
        lags.sort_unstable();
        let median = lags.get(lags.len() / 2).copied().unwrap_or(0);
        let freshness: Vec<Value> = ["T0", "T1", "T2", "T3"]
            .iter()
            .zip(done)
            .map(|(tier, count)| {
                let coverage = if views == 0 {
                    0.0
                } else {
                    count as f64 / views as f64
                };
                json!({"tier": tier, "coverage": coverage, "medianLagMs": median})
            })
            .collect();
        Ok(json!({
            "freshness": if views == 0 { Value::Null } else { Value::Array(freshness) },
            "recentErrors": errors,
            "resources": Value::Null,
        }))
    }

    async fn rest_search(
        &self,
        access: &Access,
        request: SearchRequest,
    ) -> Result<Value, EngineError> {
        let started = Instant::now();
        let limit = usize::try_from(request.limit).unwrap_or(20);
        let languages = match &request.languages {
            Some(list) if !list.is_empty() => Some(
                list.iter()
                    .map(|l| {
                        knowell_query::Language::new(l).map_err(|_| EngineError::Invalid {
                            message: "`languages` holds an invalid name".to_owned(),
                        })
                    })
                    .collect::<Result<BTreeSet<_>, _>>()?,
            ),
            _ => None,
        };
        let mut results: Vec<(f64, Value)> = Vec::new();
        let mut skipped = Vec::new();
        let mut empty_reason = None;
        let mut query_class =
            query_class_label(knowell_query::plan(&request.query, &self.inner.glossary).intent);
        let mut tokens: u64 = 0;
        for (workspace, projects) in self.rest_targets(access, request.project_ids.as_deref())? {
            let pinned = self.rest_pin(access, &workspace.name).await?;
            for project in &pinned.not_indexed {
                skipped.push(json!({"projectName": project, "reason": "not indexed yet"}));
            }
            let filters = Filters {
                projects,
                languages: languages.clone(),
                path_prefixes: request.path_prefix.iter().cloned().collect(),
            };
            let run = self
                .run_search(
                    &pinned,
                    &filters,
                    &request.query,
                    limit,
                    request.expand_graph,
                    request.rerank,
                )
                .await
                .map_err(tool_to_rest)?;
            query_class = query_class_label(run.response.plan.intent);
            let texts = self.texts_for(&run).await.map_err(tool_to_rest)?;
            let consulted = &run.response.searched.consulted;
            for result in &run.response.results {
                let freshness = freshness_of(&result.score, result.location.range);
                let why = reasons(
                    &result.why,
                    Some(&result.score),
                    result.symbol.as_deref(),
                    &result.location,
                );
                let Some(placed) = place(
                    &run.prepared,
                    &result.location,
                    result.symbol.as_deref(),
                    why,
                    freshness,
                )
                .map_err(tool_to_rest)?
                else {
                    continue;
                };
                let Some((view, overlay)) = run
                    .prepared
                    .view_of(&result.location.project, &result.location.view)
                else {
                    continue;
                };
                let snippet = if overlay {
                    view.overlay
                        .as_ref()
                        .and_then(|o| o.overlay.file(&result.location.path))
                        .map(|f| slice_lines(&f.text, placed.evidence.lines))
                } else {
                    texts
                        .get(&result.location.content_hash)
                        .map(|t| slice_lines(t, placed.evidence.lines))
                }
                .unwrap_or_default();
                let snippet: String = snippet.lines().take(40).collect::<Vec<_>>().join("\n");
                tokens =
                    tokens.saturating_add(u64::try_from(snippet.len().div_ceil(4)).unwrap_or(0));
                let contribution = |kind: SourceKind| -> Value {
                    if !consulted.contains(&kind) {
                        return Value::Null;
                    }
                    json!(
                        result
                            .score
                            .sources
                            .iter()
                            .find(|s| s.source == kind)
                            .map_or(0.0, |s| s.contribution)
                    )
                };
                let reasons_json: Vec<Value> = result
                    .why
                    .iter()
                    .filter_map(|r| panel_reason(r, result.symbol.as_deref()))
                    .collect();
                let mut value = json!({
                    "id": placed.id.as_str(),
                    "projectId": view.pinned.entry.id.to_string(),
                    "projectName": result.location.project,
                    "path": result.location.path,
                    "lineStart": placed.evidence.lines.start(),
                    "lineEnd": placed.evidence.lines.end(),
                    "language": placed.language.clone().unwrap_or_else(|| "text".to_owned()),
                    "snippet": snippet,
                    "tier": tier_label(freshness),
                    "commit": placed.evidence.commit.as_str(),
                    "score": {
                        "fused": result.score.fused,
                        "bm25": contribution(SourceKind::Lexical),
                        "vector": contribution(SourceKind::Semantic),
                        "graph": if result.why.iter().any(|r| matches!(r, Reason::GraphPath { .. })) { json!(0.0) } else { Value::Null },
                        "rerank": result.score.rerank.as_ref().map(|r| r.score),
                    },
                    "reasons": reasons_json,
                });
                if let (Some(symbol), Some(map)) = (&result.symbol, value.as_object_mut()) {
                    map.insert("symbol".to_owned(), json!(symbol));
                }
                results.push((result.score.fused, value));
            }
            for degradation in &run.response.degraded {
                let text = degradation.to_string();
                let project = pinned
                    .projects
                    .keys()
                    .find(|p| degradation.reason.starts_with(p.as_str()))
                    .map_or_else(|| "*".to_owned(), ToString::to_string);
                skipped.push(json!({"projectName": project, "reason": text}));
            }
            if run.response.results.is_empty()
                && empty_reason.is_none()
                && let Some(empty) = &run.response.empty
            {
                let code = empty.reasons.first().map_or_else(
                    || "no_matches".to_owned(),
                    |r| {
                        serde_json::to_value(r)
                            .ok()
                            .and_then(|v| v.get("kind").and_then(Value::as_str).map(str::to_owned))
                            .unwrap_or_else(|| "no_matches".to_owned())
                    },
                );
                empty_reason = Some(json!({"code": code, "message": empty.note}));
            }
        }
        results.sort_by(|(a, va), (b, vb)| {
            b.total_cmp(a).then_with(|| {
                va.get("id")
                    .and_then(Value::as_str)
                    .cmp(&vb.get("id").and_then(Value::as_str))
            })
        });
        results.truncate(limit);
        let results: Vec<Value> = results.into_iter().map(|(_, v)| v).collect();
        let mut out = json!({
            "queryClass": query_class,
            "results": results,
            "tookMs": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "skipped": skipped,
            "tokensReturned": tokens,
        });
        if results_is_empty(&out)
            && let Some(map) = out.as_object_mut()
        {
            map.insert(
                "emptyReason".to_owned(),
                empty_reason.unwrap_or_else(
                    || json!({"code": "no_matches", "message": knowell_query::ABSENCE_NOTE}),
                ),
            );
        }
        Ok(out)
    }

    async fn rest_graph(&self, access: &Access, query: GraphQuery) -> Result<Value, EngineError> {
        const MAX_NODES: usize = 300;
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let mut truncated = false;
        let targets = self.rest_targets(access, None)?;
        if query.mode == GraphMode::Contracts {
            for (workspace, _) in &targets {
                let pinned = self.rest_pin(access, &workspace.name).await?;
                let ctx = self.code_graph(&pinned).await.map_err(tool_to_rest)?;
                for node in ctx.graph.nodes() {
                    if let Some(kind) = node.contract_kind() {
                        let kind = match kind {
                            knowell_graph::ContractKind::Endpoint => "endpoint",
                            knowell_graph::ContractKind::Topic => "event",
                            knowell_graph::ContractKind::Table => "table",
                            knowell_graph::ContractKind::EnvName => "env",
                            knowell_graph::ContractKind::I18nKey => "i18n-key",
                            _ => "package",
                        };
                        nodes.push(
                            json!({"id": node.id.as_str(), "kind": kind, "label": node.name}),
                        );
                    }
                }
            }
            return Ok(json!({"nodes": nodes, "edges": edges, "truncated": truncated}));
        }
        match query.parent.as_deref() {
            None => {
                for (workspace, _) in &targets {
                    let pinned = self.rest_pin(access, &workspace.name).await?;
                    for (name, project) in &pinned.projects {
                        let snapshot = self.snapshot_of(project).await.map_err(tool_to_rest)?;
                        let mut languages: Vec<(String, u64)> =
                            snapshot.languages().into_iter().collect();
                        languages.sort_by(|(la, a), (lb, b)| b.cmp(a).then_with(|| la.cmp(lb)));
                        let main = languages.first().map_or("text", |(l, _)| l.as_str());
                        nodes.push(json!({
                            "id": format!("project:{}", project.entry.id),
                            "kind": "service",
                            "label": name,
                            "projectId": project.entry.id.to_string(),
                            "detail": format!("{} files, mostly {main}", snapshot.files.len()),
                        }));
                    }
                }
            }
            Some(parent) => {
                let (kind, rest) = parent.split_once(':').ok_or(EngineError::NotFound {
                    what: "graph node".to_owned(),
                })?;
                let (project_id, dir) = match kind {
                    "project" => (rest, None),
                    "module" => {
                        let (id, dir) = rest.split_once(':').ok_or(EngineError::NotFound {
                            what: "graph node".to_owned(),
                        })?;
                        (id, Some(dir))
                    }
                    _ => {
                        return Err(EngineError::NotFound {
                            what: "graph node".to_owned(),
                        });
                    }
                };
                let uuid = Uuid::parse_str(project_id).map_err(|_| EngineError::NotFound {
                    what: "graph node".to_owned(),
                })?;
                let mut found = None;
                for (workspace, _) in &targets {
                    let pinned = self.rest_pin(access, &workspace.name).await?;
                    if let Some(project) = pinned.projects.values().find(|p| p.entry.id.0 == uuid) {
                        found = Some(project.clone());
                        break;
                    }
                }
                let project = found.ok_or(EngineError::NotFound {
                    what: "graph node".to_owned(),
                })?;
                let snapshot = self.snapshot_of(&project).await.map_err(tool_to_rest)?;
                let module_of = |path: &RepoPath| -> String {
                    let parts: Vec<&str> = path.components().collect();
                    if parts.len() > 1 {
                        parts
                            .first()
                            .map_or_else(|| ".".to_owned(), |p| (*p).to_owned())
                    } else {
                        ".".to_owned()
                    }
                };
                let pid = project.entry.id.to_string();
                match dir {
                    None => {
                        let mut modules: BTreeMap<String, usize> = BTreeMap::new();
                        for path in snapshot.files.keys() {
                            let count = modules.entry(module_of(path)).or_default();
                            *count = count.saturating_add(1);
                        }
                        for (module, count) in &modules {
                            nodes.push(json!({
                                "id": format!("module:{pid}:{module}"),
                                "kind": "module",
                                "label": module,
                                "projectId": pid,
                                "parentId": parent,
                                "detail": format!("{count} files"),
                            }));
                        }
                        let mut links: BTreeMap<(String, String), usize> = BTreeMap::new();
                        for import in &snapshot.imports {
                            if let crate::snapshot::ImportTarget::File(to) = &import.to {
                                let (a, b) = (module_of(&import.from), module_of(to));
                                if a != b {
                                    let count = links.entry((a, b)).or_default();
                                    *count = count.saturating_add(1);
                                }
                            }
                        }
                        for ((a, b), _) in links {
                            edges.push(json!({
                                "id": format!("imports:{pid}:{a}->{b}"),
                                "from": format!("module:{pid}:{a}"),
                                "to": format!("module:{pid}:{b}"),
                                "kind": "imports",
                                "evidence": "syntactic",
                                "status": "resolved",
                            }));
                        }
                    }
                    Some(dir) => {
                        for symbol in &snapshot.symbols {
                            if symbol.parent.is_some() || module_of(&symbol.path) != dir {
                                continue;
                            }
                            if nodes.len() >= MAX_NODES {
                                truncated = true;
                                break;
                            }
                            nodes.push(json!({
                                "id": format!("symbol:{pid}:{}", symbol.key),
                                "kind": "symbol",
                                "label": symbol.local,
                                "projectId": pid,
                                "parentId": parent,
                                "detail": format!("{} in {}", symbol.kind.as_str(), symbol.path),
                            }));
                        }
                    }
                }
            }
        }
        Ok(json!({"nodes": nodes, "edges": edges, "truncated": truncated}))
    }

    async fn rest_insights(&self, access: &Access) -> Result<Value, EngineError> {
        let mut out = Vec::new();
        for (workspace, _) in self.rest_targets(access, None)? {
            let pinned = self.rest_pin(access, &workspace.name).await?;
            let ctx = self.code_graph(&pinned).await.map_err(tool_to_rest)?;
            for insight in ctx.graph.insights(&InsightConfig::default()) {
                let kind = match insight.code {
                    InsightCode::EndpointWithoutClient => "endpoint-without-client",
                    InsightCode::TopicWithoutConsumer => "event-without-consumer",
                    InsightCode::TableNeverRead => "table-never-read",
                    InsightCode::ContractDrift => "contract-drift",
                    InsightCode::I18nKeyUndefined | InsightCode::I18nKeyMissingLocale => {
                        "missing-i18n-key"
                    }
                    _ => continue,
                };
                let mut node_ids = vec![insight.subject.to_string()];
                node_ids.extend(insight.related.iter().map(ToString::to_string));
                out.push(json!({
                    "id": format!("{}:{}", insight.code.as_str(), insight.subject),
                    "kind": kind,
                    "title": insight.message,
                    "nodeIds": node_ids,
                    "evidence": "contract-derived",
                    "status": "resolved",
                }));
            }
        }
        Ok(Value::Array(out))
    }

    async fn rest_trace(
        &self,
        access: &Access,
        request: TraceRequest,
    ) -> Result<Value, EngineError> {
        let workspace = self.rest_single_workspace(access, request.project_ids.as_deref())?;
        let from = request.from.trim();
        let mut input = TraceFlowInput {
            target: Target::workspace(workspace, Vec::new()),
            direction: Some(match request.direction {
                TraceDirection::Callers => FlowDirection::Upstream,
                TraceDirection::Callees => FlowDirection::Downstream,
                TraceDirection::Both => FlowDirection::Both,
            }),
            max_depth: Some(request.depth),
            ..TraceFlowInput::default()
        };
        if from.starts_with("kn:") {
            input.id = knowell_mcp::ResultId::new(from).ok();
        } else if from.contains(' ') || from.starts_with('/') || from.contains(':') {
            input.contract = Some(from.to_owned());
        } else {
            input.symbol = Some(from.to_owned());
        }
        let output = self
            .tool_trace_flow(access.clone(), input)
            .await
            .map_err(tool_to_rest)?;
        serde_json::to_value(output).map_err(|e| internal(e.to_string()))
    }

    async fn rest_impact(
        &self,
        access: &Access,
        request: ImpactRequest,
    ) -> Result<Value, EngineError> {
        let workspace = self.rest_single_workspace(access, request.project_ids.as_deref())?;
        let ws = self.workspace(&workspace).ok_or(EngineError::NotFound {
            what: "workspace".to_owned(),
        })?;
        let change = match (&request.patch, &request.target) {
            (Some(patch), _) => {
                let project = self.rest_project_for(
                    access,
                    &ws,
                    request.project_ids.as_deref(),
                    request.target.as_deref(),
                )?;
                ChangeSubject::Patch {
                    project,
                    patch: patch.clone(),
                }
            }
            (None, Some(target)) => match target.split_once('/') {
                Some((project, path))
                    if Name::new(project).is_ok_and(|p| ws.project(&p).is_some()) =>
                {
                    ChangeSubject::File {
                        project: Name::new(project).map_err(|e| internal(e.to_string()))?,
                        path: RepoPath::new(path).map_err(|_| EngineError::Invalid {
                            message: "`target` is not a valid path".to_owned(),
                        })?,
                    }
                }
                _ => ChangeSubject::Symbol {
                    symbol: SymbolRef {
                        id: None,
                        symbol: Some(target.clone()),
                        project: None,
                    },
                },
            },
            (None, None) => {
                return Err(EngineError::Invalid {
                    message: "give `target`, `patch` or both".to_owned(),
                });
            }
        };
        let output = self
            .tool_analyze_impact(
                access.clone(),
                AnalyzeImpactInput {
                    target: Target::workspace(workspace, Vec::new()),
                    change: Some(change),
                    ..AnalyzeImpactInput::default()
                },
            )
            .await
            .map_err(tool_to_rest)?;
        serde_json::to_value(output).map_err(|e| internal(e.to_string()))
    }

    /// The project a patch applies to: the single `projectIds` entry, or
    /// the project named by `target` (`project` or `project/path`).
    fn rest_project_for(
        &self,
        access: &Access,
        ws: &WorkspaceEntry,
        project_ids: Option<&[String]>,
        target: Option<&str>,
    ) -> Result<Name, EngineError> {
        if let Some([id]) = project_ids
            && let Ok(uuid) = Uuid::parse_str(id)
            && let Some(project) = ws.projects.iter().find(|p| p.id.0 == uuid)
            && access.reads_project(&ws.name, &project.name)
        {
            return Ok(project.name.clone());
        }
        if let Some(target) = target {
            let first = target.split('/').next().unwrap_or(target);
            if let Ok(name) = Name::new(first)
                && ws.project(&name).is_some()
                && access.reads_project(&ws.name, &name)
            {
                return Ok(name);
            }
        }
        Err(EngineError::Invalid {
            message: "a patch needs exactly one project (projectIds or a `project/...` target)"
                .to_owned(),
        })
    }

    async fn rest_domains(&self, access: &Access) -> Result<Value, EngineError> {
        let mut out = Vec::new();
        for domain in &self.inner.settings.domains {
            let mut project_ids = Vec::new();
            let mut symbols = 0usize;
            for (workspace, _) in self.rest_targets(access, None)? {
                let pinned = self.rest_pin(access, &workspace.name).await?;
                for name in &domain.projects {
                    if let Ok(name) = Name::new(name.as_str())
                        && let Some(project) = pinned.projects.get(&name)
                    {
                        project_ids.push(project.entry.id.to_string());
                        let snapshot = self.snapshot_of(project).await.map_err(tool_to_rest)?;
                        symbols = symbols.saturating_add(snapshot.symbols.len());
                    }
                }
            }
            out.push(json!({
                "id": domain.id,
                "name": domain.name,
                "description": domain.description,
                "projectIds": project_ids,
                "symbolCount": symbols,
            }));
        }
        Ok(Value::Array(out))
    }

    fn rest_glossary(&self) -> Value {
        let mut terms: GlossaryTerms = BTreeMap::new();
        for entry in self.inner.glossary.entries() {
            let slot = terms
                .entry(entry.term.clone())
                .or_insert_with(|| (entry.domain.clone(), Vec::new(), Vec::new()));
            let approved = entry.status == knowell_query::TermStatus::Approved;
            slot.1.push(json!({
                "text": entry.expansion,
                "status": if approved { "approved" } else { "suggested" },
                "origin": if approved { "human" } else { "auto" },
            }));
            if entry.relation == knowell_query::TermRelation::CodeName {
                slot.2.push(entry.expansion.clone());
            }
        }
        Value::Array(
            terms
                .into_iter()
                .map(|(term, (domain, synonyms, code_names))| {
                    json!({
                        "id": knowell_core::ContentHash::of(term.as_bytes()).short(),
                        "term": term,
                        "definition": "",
                        "domainId": domain.map(|d| d.to_string()).unwrap_or_default(),
                        "synonyms": synonyms,
                        "codeNames": code_names,
                    })
                })
                .collect(),
        )
    }

    /// Records the caller may read in every visible workspace.
    async fn rest_records(
        &self,
        access: &Access,
        states: Vec<RecordState>,
    ) -> Result<Vec<(KnowledgeRecord, Option<Pinned>)>, EngineError> {
        let mut out: BTreeMap<RecordId, (KnowledgeRecord, Option<Pinned>)> = BTreeMap::new();
        for (workspace, _) in self.rest_targets(access, None)? {
            let pinned = self.rest_pin(access, &workspace.name).await?;
            let scopes = self.readable_scopes(access, &pinned);
            let rows = self
                .inner
                .memory
                .find_records(&RecordQuery {
                    scopes,
                    states: states.clone(),
                    kinds: Vec::new(),
                    text: None,
                    limit: 500,
                })
                .await
                .map_err(|e| internal(e.to_string()))?;
            for row in rows {
                out.entry(row.record.id)
                    .or_insert_with(|| (row.record, Some(pinned.clone())));
            }
        }
        let mut list: Vec<_> = out.into_values().collect();
        list.sort_by(|(a, _), (b, _)| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(list)
    }

    async fn panel_record(
        &self,
        record: &KnowledgeRecord,
        pinned: Option<&Pinned>,
    ) -> Result<Value, EngineError> {
        let (scope, scope_name) = scope_label(&record.scope);
        let mut evidence = Vec::new();
        for e in &record.evidence {
            let still_valid = match pinned.and_then(|p| p.projects.get(&e.project)) {
                Some(project) => {
                    let snapshot = self.snapshot_of(project).await.map_err(tool_to_rest)?;
                    snapshot
                        .file(&e.path)
                        .is_some_and(|f| f.content_hash == e.content_hash)
                }
                None => false,
            };
            evidence.push(json!({
                "path": format!("{}/{}", e.project, e.path),
                "lineStart": e.range.start(),
                "lineEnd": e.range.end(),
                "commit": e.commit.as_str(),
                "stillValid": still_valid,
            }));
        }
        let mut value = json!({
            "id": record.id.to_string(),
            "scope": scope,
            "scopeName": scope_name,
            "kind": panel_kind(record.kind),
            "state": panel_state(record.state),
            "title": record.title,
            "body": record.body,
            "author": panel_author(&record.author),
            "createdAt": record.created_at.to_datetime().map_or(Value::Null, iso),
            "version": record.version,
            "pinned": record.pinned,
            "evidence": evidence,
        });
        if let Some(map) = value.as_object_mut() {
            if let Some(by) = record.superseded_by {
                map.insert("supersededBy".to_owned(), json!(by.to_string()));
            }
            if record.state == RecordState::Stale
                && let Some(entry) = record
                    .history
                    .iter()
                    .rev()
                    .find(|h| h.to == RecordState::Stale)
            {
                map.insert("staleReason".to_owned(), json!(entry.reason));
            }
        }
        Ok(value)
    }

    async fn rest_memory(&self, access: &Access) -> Result<Value, EngineError> {
        let records = self.rest_records(access, Vec::new()).await?;
        let all: Vec<KnowledgeRecord> = records.iter().map(|(r, _)| r.clone()).collect();
        let conflicts = crate::tools::conflict_map(&all);
        let mut out = Vec::new();
        for (record, pinned) in &records {
            let mut value = self.panel_record(record, pinned.as_ref()).await?;
            if let Some(list) = conflicts.get(&record.id)
                && let Some(map) = value.as_object_mut()
            {
                map.insert(
                    "conflictsWith".to_owned(),
                    json!(list.iter().map(ToString::to_string).collect::<Vec<_>>()),
                );
            }
            out.push(value);
        }
        Ok(Value::Array(out))
    }

    /// The resource a record's scope maps to for authorization.
    async fn record_resource(&self, record: &KnowledgeRecord) -> Result<Resource, EngineError> {
        Ok(match &record.scope {
            Scope::Organization | Scope::User(_) => Resource::Organization,
            Scope::Workspace(w) => Resource::workspace(w.clone()),
            Scope::Project { workspace, project } => {
                Resource::project(workspace.clone(), project.clone())
            }
            Scope::Task(task) => match self
                .inner
                .memory
                .get_task(*task)
                .await
                .map_err(|e| internal(e.to_string()))?
                .and_then(|t| t.workspace)
            {
                Some(workspace) => Resource::workspace(workspace),
                None => Resource::Organization,
            },
        })
    }

    async fn rest_decide(
        &self,
        access: &Access,
        ctx: &EngineContext,
        decision: MemoryDecision,
    ) -> Result<Value, EngineError> {
        let not_found = || EngineError::NotFound {
            what: "memory record".to_owned(),
        };
        let id = Uuid::parse_str(&decision.id).map_err(|_| not_found())?;
        let row = self
            .inner
            .memory
            .get_record(RecordId::from_uuid(id))
            .await
            .map_err(|e| internal(e.to_string()))?
            .ok_or_else(not_found)?;
        // Invisible records are reported as missing.
        let readable = self
            .rest_records(access, vec![row.record.state])
            .await?
            .into_iter()
            .find(|(r, _)| r.id == row.record.id);
        let Some((_, pinned)) = readable else {
            return Err(not_found());
        };
        let resource = self.record_resource(&row.record).await?;
        let allowed = access.allows(Action::AcceptMemory, &resource)
            && match &row.record.scope {
                Scope::User(owner) => access
                    .acting_user()
                    .is_some_and(|u| u.to_string() == owner.as_str()),
                _ => true,
            };
        let request_id = ctx.request_id.clone();
        ctx.audit.record(&AuditEvent {
            at: OffsetDateTime::now_utc(),
            actor: access.principal().clone(),
            action: Action::AcceptMemory,
            resource: resource.clone(),
            decision: knowell_auth::authorize(
                access.principal(),
                Action::AcceptMemory,
                &resource,
                access.grants(),
            ),
            request_id,
        });
        let actor = access.actor().map_err(tool_to_rest)?;
        let rights = Rights {
            can_accept: allowed,
        };
        let now = crate::tools::now_timestamp();
        let note = decision
            .note
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| match decision.action {
                MemoryAction::Accept => "accepted in the panel".to_owned(),
                MemoryAction::Reject => "rejected in the panel".to_owned(),
            });
        let mut record = row.record.clone();
        let result = match decision.action {
            MemoryAction::Accept => {
                record.accept(&actor, rights, &self.inner.settings.acceptance, &note, now)
            }
            MemoryAction::Reject => record.reject(&actor, rights, &note, now),
        };
        result.map_err(|e| match e {
            knowell_knowledge::KnowledgeError::NotAuthorized { .. } => EngineError::Forbidden {
                message: e.to_string(),
            },
            knowell_knowledge::KnowledgeError::IllegalTransition { .. } => EngineError::Conflict {
                message: e.to_string(),
            },
            other => EngineError::Invalid {
                message: other.to_string(),
            },
        })?;
        let stored = self
            .inner
            .memory
            .update_record(&row, &record)
            .await
            .map_err(|e| match e {
                crate::memory::MemoryError::Conflict(message) => EngineError::Conflict { message },
                other => internal(other.to_string()),
            })?;
        // An accepted proposal that names a record it replaces supersedes it.
        if decision.action == MemoryAction::Accept {
            for tag in &stored.record.tags {
                let Some(old) = tag
                    .strip_prefix("supersedes:")
                    .and_then(|t| Uuid::parse_str(t).ok())
                else {
                    continue;
                };
                if let Some(old_row) = self
                    .inner
                    .memory
                    .get_record(RecordId::from_uuid(old))
                    .await
                    .map_err(|e| internal(e.to_string()))?
                {
                    let mut old_record = old_row.record.clone();
                    match old_record.supersede(&stored.record, &actor, rights, &note, now) {
                        Ok(()) => {
                            self.inner
                                .memory
                                .update_record(&old_row, &old_record)
                                .await
                                .map_err(|e| internal(e.to_string()))?;
                        }
                        Err(error) => {
                            tracing::info!(error = %error, "a proposal's superseded record was left unchanged")
                        }
                    }
                }
            }
        }
        self.panel_record(&stored.record, pinned.as_ref()).await
    }

    async fn rest_tasks(&self, access: &Access) -> Result<Value, EngineError> {
        let mut out = Vec::new();
        for (workspace, _) in self.rest_targets(access, None)? {
            let pinned = self.rest_pin(access, &workspace.name).await?;
            let rows = self
                .inner
                .memory
                .list_tasks(Some(&workspace.name), &[], 200)
                .await
                .map_err(|e| internal(e.to_string()))?;
            for row in rows {
                let own = match (&row.owner, access.acting_user()) {
                    (None, _) => true,
                    (Some(owner), Some(user)) => *owner == user.to_string(),
                    _ => false,
                };
                if !own {
                    continue;
                }
                let checkpoints = self
                    .inner
                    .memory
                    .checkpoints(row.task.id)
                    .await
                    .map_err(|e| internal(e.to_string()))?;
                let manifest = checkpoints.last().map_or_else(
                    || row.task.view_manifest.clone(),
                    |c| c.checkpoint.manifest.clone(),
                );
                let changed = manifest
                    .iter()
                    .filter(|pin| {
                        pinned
                            .projects
                            .get(&pin.project)
                            .is_some_and(|p| p.commit.as_deref() != Some(pin.commit.as_str()))
                    })
                    .count();
                let status = match row.task.status {
                    knowell_knowledge::TaskStatus::Open => "open",
                    knowell_knowledge::TaskStatus::InProgress => "in-progress",
                    knowell_knowledge::TaskStatus::Blocked => "blocked",
                    knowell_knowledge::TaskStatus::Done
                    | knowell_knowledge::TaskStatus::Abandoned => "done",
                };
                out.push(json!({
                    "id": row.task.id.to_string(),
                    "title": row.task.title,
                    "status": status,
                    "goal": row.task.goal,
                    "projectNames": manifest.iter().map(|p| p.project.to_string()).collect::<BTreeSet<_>>(),
                    "progress": row.task.notes.iter().map(|n| n.text.clone()).collect::<Vec<_>>(),
                    "openQuestions": row.task.open_questions.iter().filter(|q| q.resolved_at.is_none()).map(|q| q.text.clone()).collect::<Vec<_>>(),
                    "changedSinceCheckpoint": changed,
                    "updatedAt": row.task.updated_at.to_datetime().map_or(Value::Null, iso),
                }));
            }
        }
        Ok(Value::Array(out))
    }

    async fn rest_rules(&self, access: &Access) -> Result<Value, EngineError> {
        let records = self
            .rest_records(access, vec![RecordState::Accepted, RecordState::Proposed])
            .await?;
        let mut out = Vec::new();
        for (record, _) in records.iter().filter(|(r, _)| r.is_rule()) {
            out.push(json!({
                "id": record.id.to_string(),
                "name": record.title,
                "description": record.body,
                "state": if record.state == RecordState::Accepted { "accepted" } else { "proposed" },
                "severity": "info",
                "approvedExamples": record.evidence.iter().map(|e| json!({
                    "path": format!("{}/{}", e.project, e.path),
                    "note": format!("lines {}-{} at {}", e.range.start(), e.range.end(), e.commit.as_str()),
                })).collect::<Vec<_>>(),
                "violations": [],
            }));
        }
        Ok(Value::Array(out))
    }

    /// Store profiles of the organization.
    async fn profiles(&self) -> Result<Vec<EmbeddingProfile>, EngineError> {
        let mut conn = self.inner.store.acquire().await.map_err(store_err)?;
        embeddings::list_profiles(&mut conn, self.inner.organization)
            .await
            .map_err(store_err)
    }

    /// The profile each visible project embeds with now.
    fn active_profiles(&self, access: &Access) -> BTreeMap<ProfileId, Vec<(Name, Name)>> {
        let mut out: BTreeMap<ProfileId, Vec<(Name, Name)>> = BTreeMap::new();
        for workspace in self.all_workspaces() {
            for project in &workspace.projects {
                if !access.reads_project(&workspace.name, &project.name) {
                    continue;
                }
                if let EmbeddingPlan::Embed { profile, .. } = &project.embedding {
                    out.entry(*profile)
                        .or_default()
                        .push((workspace.name.clone(), project.name.clone()));
                }
            }
        }
        out
    }

    async fn rest_profiles(&self, access: &Access) -> Result<Value, EngineError> {
        let active = self.active_profiles(access);
        let providers = self
            .inner
            .profile_providers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let switches = self
            .inner
            .switches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut out = Vec::new();
        for profile in self.profiles().await? {
            let provider_name = providers.get(&profile.id);
            let cloud = provider_name.is_some_and(|p| self.provider_is_cloud(p));
            let mut value = json!({
                "id": profile.id.to_string(),
                "name": profile.name,
                "provider": profile.provider,
                "model": profile.model,
                "dimensions": profile.dimensions,
                "storage": "halfvec",
                "locality": if cloud { "cloud-allowed" } else { "local-only" },
                "budgets": {},
                "spentThisMonthUsdMicros": 0,
                "measured": Value::Null,
                "active": active.contains_key(&profile.id),
            });
            if let Some(map) = value.as_object_mut() {
                if let Some(key) = provider_name
                    .and_then(|p| self.inner.providers.get(p))
                    .and_then(|c| c.api_key.as_ref())
                {
                    // A reference such as `env:NAME`, never a value.
                    map.insert("apiKey".to_owned(), json!(key.to_string()));
                }
                if let Some(switch) = switches.iter().rev().find(|s| s.to == profile.id) {
                    let progress = self.switch_progress(switch).await?;
                    let state = if progress >= 1.0 {
                        "active"
                    } else {
                        "building"
                    };
                    map.insert(
                        "switch".to_owned(),
                        json!({"state": state, "progress": progress}),
                    );
                }
            }
            out.push(value);
        }
        Ok(Value::Array(out))
    }

    /// Share of a switch's views whose active generation is covered by the
    /// target profile's vectors.
    async fn switch_progress(&self, switch: &SwitchRecord) -> Result<f64, EngineError> {
        if switch.views.is_empty() {
            return Ok(1.0);
        }
        let mut conn = self.inner.store.acquire().await.map_err(store_err)?;
        let mut done = 0usize;
        for view in &switch.views {
            let active = knowell_store::views::get_view(&mut conn, *view)
                .await
                .map_err(store_err)?
                .and_then(|v| v.active_generation);
            let covered = embeddings::active_index_generation(&mut conn, *view, switch.to)
                .await
                .map_err(store_err)?
                .is_some_and(|g| Some(g.view_generation) == active);
            if covered {
                done = done.saturating_add(1);
            }
        }
        Ok(done as f64 / switch.views.len() as f64)
    }

    /// The engine provider whose embedder produces `profile`'s vectors.
    fn provider_for(&self, profile: &EmbeddingProfile) -> Option<Name> {
        self.inner.embedders.iter().find_map(|(name, embedder)| {
            let p = embedder.profile();
            (p.provider_kind.as_str() == profile.provider
                && p.model == profile.model
                && p.dimensions == profile.dimensions)
                .then(|| name.clone())
        })
    }

    async fn rest_switch_estimate(&self, access: &Access, to: &str) -> Result<Value, EngineError> {
        let not_found = || EngineError::NotFound {
            what: "embedding profile".to_owned(),
        };
        let to_id = Uuid::parse_str(to).map_err(|_| not_found())?;
        let profiles = self.profiles().await?;
        let target = profiles
            .iter()
            .find(|p| p.id.0 == to_id)
            .ok_or_else(not_found)?;
        let active = self.active_profiles(access);
        let from = active
            .iter()
            .filter(|(id, _)| **id != target.id)
            .max_by_key(|(_, projects)| projects.len())
            .map(|(id, _)| *id);
        let from_profile = from.and_then(|id| profiles.iter().find(|p| p.id == id));
        let mut affected = Vec::new();
        let mut chunks: u64 = 0;
        let mut bytes: u64 = 0;
        let mut warnings = Vec::new();
        for workspace in self.all_workspaces() {
            let pinned = self.rest_pin(access, &workspace.name).await?;
            for project in pinned.projects.values() {
                let uses_target = matches!(&project.entry.embedding, EmbeddingPlan::Embed { profile, .. } if *profile == target.id);
                if uses_target {
                    continue;
                }
                affected.push(project.entry.name.to_string());
                let snapshot = self.snapshot_of(project).await.map_err(tool_to_rest)?;
                for list in snapshot.chunks.values() {
                    chunks = chunks.saturating_add(u64::try_from(list.len()).unwrap_or(0));
                    bytes = bytes.saturating_add(list.iter().map(|c| c.bytes).sum::<u64>());
                }
                if project.entry.data_policy == knowell_config::DataPolicy::LocalOnly
                    && self
                        .provider_for(target)
                        .is_some_and(|p| self.provider_is_cloud(&p))
                {
                    warnings.push(format!(
                        "{} is local-only; a cloud profile will not embed it (nothing is sent)",
                        project.entry.name
                    ));
                }
            }
        }
        let needs_reembedding = match from_profile {
            Some(from) => {
                !(from.provider == target.provider
                    && from.model == target.model
                    && target.dimensions <= from.dimensions)
            }
            None => true,
        };
        let tokens = bytes.div_ceil(4);
        let price = self
            .inner
            .settings
            .prices_usd_per_million_tokens
            .get(&target.provider)
            .copied();
        if price.is_none() && needs_reembedding {
            warnings.push(format!(
                "no price is configured for provider `{}`; the cost is shown as 0",
                target.provider
            ));
        }
        if self.provider_for(target).is_none() {
            warnings.push(
                "no embedder for this profile is configured in the engine; the switch cannot start"
                    .to_owned(),
            );
        }
        warnings.push("duration is not measured yet; shown as 0".to_owned());
        let cost = if needs_reembedding {
            (tokens as f64 * price.unwrap_or(0.0)).round()
        } else {
            0.0
        };
        Ok(json!({
            "fromProfileId": from.map_or_else(String::new, |id| id.to_string()),
            "toProfileId": target.id.to_string(),
            "affectedProjects": affected,
            "chunksToRegenerate": chunks,
            "needsReembedding": needs_reembedding,
            "estimatedTokens": if needs_reembedding { tokens } else { 0 },
            "estimatedCostUsdMicros": cost,
            "estimatedDiskBytes": chunks.saturating_mul(u64::from(target.dimensions)).saturating_mul(2),
            "estimatedDurationMs": 0,
            "warnings": warnings,
        }))
    }

    async fn rest_start_switch(
        &self,
        access: &Access,
        request: SwitchRequest,
    ) -> Result<Value, EngineError> {
        let not_found = || EngineError::NotFound {
            what: "embedding profile".to_owned(),
        };
        let to_id = Uuid::parse_str(&request.to_profile_id).map_err(|_| not_found())?;
        let profiles = self.profiles().await?;
        let target = profiles
            .iter()
            .find(|p| p.id.0 == to_id)
            .ok_or_else(not_found)?
            .clone();
        let provider = self
            .provider_for(&target)
            .ok_or_else(|| EngineError::Invalid {
                message: "no embedder for this profile is configured in the engine".to_owned(),
            })?;
        let from = self
            .active_profiles(access)
            .into_iter()
            .filter(|(id, _)| *id != target.id)
            .max_by_key(|(_, p)| p.len())
            .map(|(id, _)| id);
        let mut views = Vec::new();
        let mut jobs = Vec::new();
        for workspace in self.all_workspaces() {
            let mut resolved: ResolvedWorkspace = workspace.resolved.clone();
            let mut changed = false;
            for project in &mut resolved.projects {
                if !access.reads_project(&workspace.name, &project.name) {
                    continue;
                }
                let current = workspace.project(&project.name).map(|p| &p.embedding);
                if matches!(current, Some(EmbeddingPlan::Embed { profile, .. }) if *profile == target.id)
                {
                    continue;
                }
                project.embedding.provider = Some(Sourced {
                    value: provider.clone(),
                    origin: Origin::Project,
                });
                project.embedding.model = Some(Sourced {
                    value: target.model.clone(),
                    origin: Origin::Project,
                });
                project.embedding.preset = Sourced {
                    value: knowell_config::EmbeddingPreset::Custom,
                    origin: Origin::Project,
                };
                project.embedding.dimensions = Sourced {
                    value: target.dimensions,
                    origin: Origin::Project,
                };
                changed = true;
            }
            if !changed {
                continue;
            }
            // Re-registering points new builds at the target profile; the
            // active generation keeps serving with the old profile's vectors
            // until a rebuild with complete new vectors activates.
            let registration = self
                .add_workspace(&resolved)
                .await
                .map_err(EngineError::from)?;
            for view in &registration.views {
                if !matches!(&view.embedding, EmbeddingPlan::Embed { profile, .. } if *profile == target.id)
                {
                    continue;
                }
                views.push(view.view);
                match self
                    .inner
                    .indexer
                    .rebuild_view(view.view, Priority::Background)
                    .await
                    .map_err(|e| internal(e.to_string()))?
                {
                    SyncOutcome::Queued { job, .. } => jobs.push(job.to_string()),
                    SyncOutcome::UpToDate { .. } => {}
                    SyncOutcome::Failed { reason, .. } => {
                        tracing::warn!(%reason, "a view could not be queued for the profile switch");
                    }
                }
            }
        }
        let record = SwitchRecord {
            id: Uuid::now_v7(),
            from,
            to: target.id,
            views,
            started: OffsetDateTime::now_utc(),
        };
        self.inner
            .switches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(record.clone());
        Ok(json!({
            "switchId": record.id.to_string(),
            "fromProfileId": record.from.map(|p| p.to_string()),
            "toProfileId": record.to.to_string(),
            "views": record.views.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "jobs": jobs,
            "startedAt": iso(record.started),
            "state": "building",
        }))
    }

    fn rest_eval_reports(&self) -> Result<Value, EngineError> {
        let Some(dir) = &self.inner.settings.eval_reports_dir else {
            return Ok(Value::Array(Vec::new()));
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(Value::Array(Vec::new()));
        };
        let mut files: Vec<std::path::PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        files.sort();
        let mut out = Vec::new();
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(report) = knowell_eval::Report::from_json(&text) else {
                tracing::warn!("skipping a file that is not an evaluation report");
                continue;
            };
            let ran_at = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .map(OffsetDateTime::from)
                .map_or(Value::Null, iso);
            let metrics = |m: &knowell_eval::MetricSummary| {
                json!({
                    "recallAt5": m.recall_at_5.unwrap_or(0.0),
                    "recallAt10": m.recall_at_10.unwrap_or(0.0),
                    "mrr": m.mrr_at_10.unwrap_or(0.0),
                    "ndcgAt10": m.ndcg_at_10.unwrap_or(0.0),
                })
            };
            let mut slices = Vec::new();
            let mut bad = Vec::new();
            for retriever in &report.retrievers {
                slices.push(json!({"dimension": "retriever", "value": retriever.name, "queries": retriever.overall.ranked_queries, "metrics": metrics(&retriever.overall)}));
                for (kind, m) in &retriever.by_kind {
                    slices.push(json!({"dimension": "kind", "value": format!("{}:{kind}", retriever.name), "queries": m.ranked_queries, "metrics": metrics(m)}));
                }
                for (lang, m) in &retriever.by_lang {
                    slices.push(json!({"dimension": "lang", "value": format!("{}:{lang}", retriever.name), "queries": m.ranked_queries, "metrics": metrics(m)}));
                }
                for q in &retriever.queries {
                    if q.metrics
                        .as_ref()
                        .is_some_and(|m| m.first_relevant_rank.is_none())
                    {
                        bad.push(json!({
                            "query": q.id,
                            "expected": "",
                            "got": q.top.first().cloned().unwrap_or_default(),
                            "queryClass": "behavior",
                        }));
                    }
                }
            }
            let first = report.retrievers.first();
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push(json!({
                "id": name,
                "querySet": report.query_set.fixture,
                "profileId": "",
                "ranAt": ran_at,
                "hardware": "not recorded",
                "dataset": report.fixture.as_ref().map_or_else(String::new, |f| format!("{} seed {} {}", f.name, f.seed, f.scale.as_str())),
                "queryCount": report.query_set.queries,
                "overall": first.map_or_else(|| json!({"recallAt5": 0.0, "recallAt10": 0.0, "mrr": 0.0, "ndcgAt10": 0.0}), |r| metrics(&r.overall)),
                "slices": slices,
                "badResults": bad.into_iter().take(20).collect::<Vec<_>>(),
            }));
        }
        Ok(Value::Array(out))
    }

    fn rest_integrations(&self) -> Value {
        let last = self.inner.usage.last_call();
        let recent =
            last.is_some_and(|t| OffsetDateTime::now_utc() - t < time::Duration::minutes(5));
        let hub = self.inner.settings.role == knowell_config::ServerRole::Hub;
        let mut mcp = json!({
            "transport": if hub { "streamable-http" } else { "stdio" },
            "endpoint": if hub { "/mcp" } else { "stdio" },
            "status": if recent { "connected" } else { "idle" },
        });
        if let (Some(at), Some(map)) = (last, mcp.as_object_mut()) {
            map.insert("lastCallAt".to_owned(), iso(at));
        }
        json!({"mcp": mcp, "agents": [], "webhooks": []})
    }

    async fn rest_admin(&self) -> Result<Value, EngineError> {
        let hub = self.inner.settings.role == knowell_config::ServerRole::Hub;
        let role = self.inner.settings.role.as_str();
        if !hub {
            return Ok(
                json!({"available": false, "role": role, "users": [], "tokens": [], "audit": []}),
            );
        }
        let mut conn = self.inner.store.acquire().await.map_err(store_err)?;
        let org = self.inner.organization;
        let principals = identity::list_principals(&mut conn, org)
            .await
            .map_err(store_err)?;
        let grants = identity::list_grants(&mut conn, org, None)
            .await
            .map_err(store_err)?;
        let users: Vec<Value> = principals
            .iter()
            .map(|p| {
                let role = grants
                    .iter()
                    .filter(|g| {
                        g.principal == p.id && g.scope == identity::GrantScope::Organization
                    })
                    .map(|g| g.role)
                    .max()
                    .map_or("reader", |r| match r {
                        knowell_store::GrantRole::Admin => "admin",
                        knowell_store::GrantRole::Maintainer => "maintainer",
                        _ => "reader",
                    });
                json!({
                    "id": p.id.to_string(),
                    "name": p.display_name.clone().unwrap_or_else(|| p.name.to_string()),
                    "email": "",
                    "role": role,
                    "disabled": p.disabled_at.is_some(),
                })
            })
            .collect();
        let tokens: Vec<Value> = identity::list_api_tokens(&mut conn, org, None)
            .await
            .map_err(store_err)?
            .iter()
            .map(|t| {
                let mut value = json!({
                    "id": t.id.to_string(),
                    "name": t.label.clone().unwrap_or_default(),
                    "prefix": t.prefix,
                    "scopes": t.scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    "createdAt": iso(t.created_at),
                });
                if let (Some(expires), Some(map)) = (t.expires_at, value.as_object_mut()) {
                    map.insert("expiresAt".to_owned(), iso(expires));
                }
                value
            })
            .collect();
        let mut filter = store_audit::AuditFilter::new(100);
        filter.organization = Some(org);
        let audit: Vec<Value> = store_audit::list_audit(&mut conn, &filter)
            .await
            .map_err(store_err)?
            .iter()
            .map(|e| {
                json!({
                    "id": e.id.to_string(),
                    "at": iso(e.at),
                    "actor": e.actor,
                    "action": e.action,
                    "target": e.resource,
                    "outcome": if e.allowed { "success" } else { "denied" },
                })
            })
            .collect();
        Ok(
            json!({"available": true, "role": role, "users": users, "tokens": tokens, "audit": audit}),
        )
    }
}

fn results_is_empty(value: &Value) -> bool {
    value
        .get("results")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
}

#[cfg(test)]
mod tests {
    use knowell_query::{ExactTarget, Intent};

    use super::*;

    #[test]
    fn panel_reasons_use_the_panel_vocabulary() {
        let exact = panel_reason(
            &Reason::ExactMatch {
                term: "cancel".into(),
                target: ExactTarget::Symbol,
            },
            Some("Svc.cancel"),
        )
        .unwrap();
        assert_eq!(
            exact,
            json!({"type": "exact-symbol", "symbol": "Svc.cancel"})
        );
        let lexical = panel_reason(
            &Reason::LexicalTerms {
                terms: vec!["cancel".into()],
            },
            None,
        )
        .unwrap();
        assert_eq!(lexical["type"], "lexical");
        let semantic = panel_reason(
            &Reason::SemanticSimilarity {
                similarity: 0.5,
                profile: "p".into(),
            },
            None,
        )
        .unwrap();
        assert_eq!(semantic, json!({"type": "semantic", "similarity": 0.5}));
        assert_eq!(query_class_label(Intent::Why), "why");
        assert_eq!(query_class_label(Intent::PathOrFile), "exact-symbol");
        assert_eq!(
            evidence_label(knowell_query::EvidenceType::HeuristicMatch),
            "heuristic"
        );
    }

    #[test]
    fn record_labels_follow_the_panel() {
        assert_eq!(panel_state(RecordState::Proposed), "proposed");
        assert_eq!(panel_kind(RecordKind::ModelSuggestion), "agent-finding");
        assert_eq!(panel_author(&Actor::System)["type"], "engine");
        assert_eq!(scope_label(&Scope::Organization).0, "organization");
        assert!(results_is_empty(&json!({"results": []})));
        assert!(!results_is_empty(&json!({"results": [1]})));
    }
}
