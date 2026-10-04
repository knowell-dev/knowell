//! `trace_flow`, `analyze_impact` and `contracts` over the code graph of
//! the pinned manifest.
//!
//! The pinned graph combines file and symbol structure, syntactic imports,
//! and facts from the configured relation stage (by default `knowell-link`).
//! Missing relation stages and unsupported reference resolution remain
//! explicit gaps; file-import impact is not presented as call resolution.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use knowell_core::{LineRange, Name, RepoPath, TrackTarget};
use knowell_graph::{
    CodeGraph, ContractKind as GContractKind, Direction, EdgeFilter, EdgeKind, ImpactSpec, NodeId,
    NodeKind as GNodeKind, WalkSpec,
};
use knowell_index::GitConfigMode;
use knowell_mcp::tools::{
    AnalyzeImpactInput, AnalyzeImpactOutput, ChangeSubject, ContractInfo, ContractKind,
    ContractParticipant, ContractRole, ContractsInput, ContractsOutput, DriftCode, DriftFinding,
    FlowDirection, FlowEdge, FlowNode, ImpactItem, ImpactKind, NodeKind, Risk, RiskCode,
    RiskFactor, RiskLevel, TraceFlowInput, TraceFlowOutput,
};
use knowell_mcp::{
    Evidence, EvidenceType, FreshnessTier, Gap, GapReason, GraphHop, MatchReason, RelationKind,
    Resolution, ToolError, ViewLayer,
};
use knowell_parse::parse;
use knowell_secrets::ExclusionPolicy;
use knowell_source::git::{Change, GitError, GitRepo};
use knowell_source::{FileRead, SkipReason, WalkOptions};
use knowell_store::views::GenerationPin;

use super::code::dedupe;
use crate::access::Access;
use crate::engine::Engine;
use crate::error::store_tool;
use crate::graph::{self, file_node, symbol_node};
use crate::ids::{contract_id, source_id};
use crate::patch;
use crate::scope::{Pinned, PinnedProject};
use crate::search::is_test_path;
use crate::snapshot::{Snapshot, slice_lines, whole_file};

/// The graph of a pinned manifest and the views it was built from.
pub(crate) struct GraphCtx {
    pub(crate) graph: Arc<CodeGraph>,
    pub(crate) views: BTreeMap<Name, (PinnedProject, Arc<Snapshot>)>,
}

impl GraphCtx {
    fn flow_node(&self, id: &NodeId, local: String) -> Option<FlowNode> {
        let node = self.graph.node(id)?;
        let placed = self.evidence(node, Vec::new());
        let label = match node.kind {
            GNodeKind::File => node
                .source
                .as_ref()
                .map_or_else(|| node.name.clone(), |source| source.path.to_string()),
            _ => node.name.clone(),
        };
        Some(FlowNode {
            node: local,
            kind: node_kind(node),
            label,
            project: node.project.clone(),
            id: placed.as_ref().map(|(id, _)| id.clone()),
            evidence: placed.map(|(_, evidence)| evidence),
        })
    }

    /// Evidence of a graph node (its source reference) at the pinned view.
    fn evidence(
        &self,
        node: &knowell_graph::Node,
        why: Vec<MatchReason>,
    ) -> Option<(knowell_mcp::ResultId, Evidence)> {
        let source = node.source.as_ref()?;
        let (project, snapshot) = self.views.get(&source.project)?;
        let file = snapshot.file(&source.path)?;
        let lines = source.range.or_else(|| whole_file(file.line_count))?;
        let commit = project.commit_id()?;
        let id = source_id(
            &source.project,
            Some(commit.as_str()),
            &source.content_hash,
            &source.path,
            lines,
        )
        .ok()?;
        let symbol = match node.kind {
            GNodeKind::Symbol => Some(node.name.clone()),
            _ => None,
        };
        Some((
            id,
            Evidence {
                project: source.project.clone(),
                view: project.target.clone(),
                layer: ViewLayer::Shared,
                commit,
                path: source.path.clone(),
                lines,
                content_hash: source.content_hash,
                symbol,
                why,
                freshness: FreshnessTier::T1Symbols,
                index_state: project.index_state(),
            },
        ))
    }

    /// Evidence of an edge's first reference.
    fn edge_evidence(&self, edge: &knowell_graph::Edge) -> Vec<Evidence> {
        edge.evidence_refs
            .iter()
            .filter_map(|r| {
                let (project, snapshot) = self.views.get(&r.project)?;
                let file = snapshot.file(&r.path)?;
                Some(Evidence {
                    project: r.project.clone(),
                    view: project.target.clone(),
                    layer: ViewLayer::Shared,
                    commit: project.commit_id()?,
                    path: r.path.clone(),
                    lines: r.range.or_else(|| whole_file(file.line_count))?,
                    content_hash: r.content_hash,
                    symbol: None,
                    why: Vec::new(),
                    freshness: FreshnessTier::T1Symbols,
                    index_state: project.index_state(),
                })
            })
            .take(1)
            .collect()
    }
}

fn mcp_evidence(e: knowell_graph::EvidenceType) -> EvidenceType {
    match e {
        knowell_graph::EvidenceType::SemanticResolved => EvidenceType::SemanticallyResolved,
        knowell_graph::EvidenceType::RuntimeObserved => EvidenceType::RuntimeObservation,
        knowell_graph::EvidenceType::ContractDerived => EvidenceType::ContractDerived,
        knowell_graph::EvidenceType::Syntactic => EvidenceType::SyntacticObservation,
        knowell_graph::EvidenceType::Heuristic => EvidenceType::HeuristicMatch,
        knowell_graph::EvidenceType::ModelSuggestion => EvidenceType::ModelSuggestion,
    }
}

fn mcp_resolution(r: knowell_graph::Resolution) -> Resolution {
    match r {
        knowell_graph::Resolution::Resolved => Resolution::Resolved,
        knowell_graph::Resolution::Ambiguous => Resolution::Ambiguous,
        knowell_graph::Resolution::Unresolved => Resolution::Unresolved,
    }
}

/// Graph edge kinds a requested relation maps to.
fn graph_kinds(relation: RelationKind) -> &'static [EdgeKind] {
    match relation {
        RelationKind::Calls => &[EdgeKind::Calls],
        RelationKind::References | RelationKind::UsesI18nKey => &[EdgeKind::References],
        RelationKind::Implements => &[EdgeKind::Implements],
        RelationKind::Imports => &[EdgeKind::Imports],
        RelationKind::HttpCall | RelationKind::Consumes | RelationKind::RpcCall => {
            &[EdgeKind::Consumes]
        }
        RelationKind::HttpRoute | RelationKind::RpcServes => &[EdgeKind::Exposes],
        RelationKind::Publishes => &[EdgeKind::Produces],
        RelationKind::ReadsTable | RelationKind::ReadsEnv => &[EdgeKind::Reads],
        RelationKind::WritesTable => &[EdgeKind::Writes],
        RelationKind::DependsOnPackage => &[EdgeKind::DependsOn],
        RelationKind::Tests => &[EdgeKind::Tests],
        RelationKind::Documents => &[EdgeKind::Documents, EdgeKind::Decides],
    }
}

/// The MCP relation of a graph edge (by kind and target contract kind);
/// `None` for structural edges.
fn relation_of(kind: EdgeKind, target: Option<GContractKind>) -> Option<RelationKind> {
    Some(match kind {
        EdgeKind::Calls => RelationKind::Calls,
        EdgeKind::References => match target {
            Some(GContractKind::I18nKey) => RelationKind::UsesI18nKey,
            _ => RelationKind::References,
        },
        EdgeKind::Implements => RelationKind::Implements,
        EdgeKind::Imports => RelationKind::Imports,
        EdgeKind::Tests => RelationKind::Tests,
        EdgeKind::Documents | EdgeKind::Decides => RelationKind::Documents,
        EdgeKind::Produces => RelationKind::Publishes,
        EdgeKind::Consumes => match target {
            Some(GContractKind::Endpoint) => RelationKind::HttpCall,
            Some(GContractKind::Rpc) => RelationKind::RpcCall,
            _ => RelationKind::Consumes,
        },
        EdgeKind::Reads => match target {
            Some(GContractKind::EnvName) => RelationKind::ReadsEnv,
            Some(GContractKind::I18nKey) => RelationKind::UsesI18nKey,
            _ => RelationKind::ReadsTable,
        },
        EdgeKind::Writes => RelationKind::WritesTable,
        EdgeKind::Exposes => match target {
            Some(GContractKind::Rpc) => RelationKind::RpcServes,
            _ => RelationKind::HttpRoute,
        },
        EdgeKind::DependsOn => RelationKind::DependsOnPackage,
        EdgeKind::Defines | EdgeKind::Contains => return None,
    })
}

fn node_kind(node: &knowell_graph::Node) -> NodeKind {
    match node.contract_kind() {
        Some(GContractKind::Endpoint) => NodeKind::Endpoint,
        Some(GContractKind::Topic) => NodeKind::Topic,
        Some(GContractKind::Rpc) => NodeKind::Rpc,
        Some(GContractKind::Table) => NodeKind::Table,
        Some(GContractKind::EnvName) => NodeKind::EnvName,
        Some(GContractKind::I18nKey) => NodeKind::I18nKey,
        Some(GContractKind::Package | GContractKind::Infra) => NodeKind::Package,
        None if node.is_unresolved_placeholder() => NodeKind::Package,
        None => NodeKind::Symbol,
    }
}

fn mcp_contract_kind(kind: knowell_store::ContractKind) -> ContractKind {
    match kind {
        knowell_store::ContractKind::Endpoint => ContractKind::Endpoint,
        knowell_store::ContractKind::Topic => ContractKind::Topic,
        knowell_store::ContractKind::Rpc => ContractKind::Rpc,
        knowell_store::ContractKind::Table => ContractKind::Table,
        knowell_store::ContractKind::EnvName => ContractKind::EnvName,
        knowell_store::ContractKind::I18nKey => ContractKind::I18nKey,
        knowell_store::ContractKind::Package => ContractKind::Package,
    }
}

fn store_contract_kind(kind: ContractKind) -> knowell_store::ContractKind {
    match kind {
        ContractKind::Endpoint => knowell_store::ContractKind::Endpoint,
        ContractKind::Topic => knowell_store::ContractKind::Topic,
        ContractKind::Rpc => knowell_store::ContractKind::Rpc,
        ContractKind::Table => knowell_store::ContractKind::Table,
        ContractKind::EnvName => knowell_store::ContractKind::EnvName,
        ContractKind::I18nKey => knowell_store::ContractKind::I18nKey,
        ContractKind::Package => knowell_store::ContractKind::Package,
    }
}

/// Whether a stored contract key matches a (lowercased) user query. Keys
/// are stored normalised by knowell-link (`POST /v1/items/{}`), while agents
/// type `/v1/items/{id}`, `/v1/items/:id` or `${id}`: the query is matched
/// both as typed and in the same normalised form.
fn contract_matches(kind: knowell_store::ContractKind, key: &str, query: &str) -> bool {
    let key = key.to_lowercase();
    if key.contains(query) {
        return true;
    }
    let graph_kind = match kind {
        knowell_store::ContractKind::Endpoint => knowell_graph::ContractKind::Endpoint,
        knowell_store::ContractKind::Topic => knowell_graph::ContractKind::Topic,
        knowell_store::ContractKind::Rpc => knowell_graph::ContractKind::Rpc,
        knowell_store::ContractKind::Table => knowell_graph::ContractKind::Table,
        knowell_store::ContractKind::EnvName => knowell_graph::ContractKind::EnvName,
        knowell_store::ContractKind::I18nKey => knowell_graph::ContractKind::I18nKey,
        knowell_store::ContractKind::Package => knowell_graph::ContractKind::Package,
    };
    knowell_link::normalize_key(graph_kind, query)
        .is_some_and(|normalized| key.contains(&normalized.to_lowercase()))
}

fn relations_gap(engine: &Engine) -> Option<Gap> {
    engine.inner.settings.relation_stage.is_none().then(|| {
        Gap::new(
            GapReason::RelationsNotReady,
            "no contract relation stage ran; HTTP, event, RPC and table relations may be missing even when source imports, references or calls are indexed",
        )
    })
}

fn graph_overlay_gaps(pinned: &Pinned) -> Vec<Gap> {
    pinned
        .projects
        .iter()
        .filter(|(_, project)| {
            project.overlay.as_ref().is_some_and(|overlay| !overlay.overlay.is_empty())
        })
        .map(|(name, _)| {
            Gap::for_project(
                GapReason::RelationsNotReady,
                name.clone(),
                "graph relations describe the shared indexed generation; changed or deleted working-tree overlay sources are not included in this graph",
            )
        })
        .collect()
}

fn graph_context_filter(pinned: &Pinned, filter: EdgeFilter, gaps: &mut Vec<Gap>) -> EdgeFilter {
    if pinned
        .projects
        .values()
        .any(|project| project.overlay.is_some())
    {
        gaps.push(Gap::new(
            GapReason::NoReferenceResolutionForLanguage,
            "compiler-resolved graph edges were excluded because this personal context has no matching compiler-input analysis; source and dependency inputs may differ from the shared index",
        ));
        filter.without_evidence(knowell_graph::EvidenceType::SemanticResolved)
    } else {
        filter
    }
}

fn call_coverage_message(coverage: &[knowell_store::analysis::AnalysisCoverage]) -> Option<String> {
    let mut summaries = Vec::new();
    let mut partial = false;
    for provider in ["syntax", "scip"] {
        for record in coverage.iter().filter(|record| record.provider == provider) {
            let details = &record.details;
            partial |= details
                .get("calls_complete")
                .and_then(serde_json::Value::as_bool)
                != Some(true);
            let mut facts = Vec::new();
            for (key, label) in [
                ("call_sites", "callee spans observed"),
                ("calls_written", "call observations written"),
                ("calls_ambiguous", "ambiguous call targets"),
                ("calls_unresolved", "unresolved call targets"),
            ] {
                if let Some(count) = details.get(key).and_then(serde_json::Value::as_u64) {
                    facts.push(format!("{count} {label}"));
                }
            }
            for (key, label) in [
                ("parse_unavailable", "parser unavailable"),
                ("source_unavailable", "source unavailable"),
                ("encoding_unknown", "position encoding unknown"),
                ("truncated", "analysis truncated"),
                ("candidate_overflow", "candidate budget reached"),
            ] {
                let present = details.get(key).is_some_and(|value| {
                    value.as_bool() == Some(true) || value.as_u64().is_some_and(|count| count > 0)
                });
                if present {
                    partial = true;
                    facts.push(label.to_owned());
                }
            }
            if facts.is_empty() {
                partial = true;
                facts.push("call-site counters unavailable".to_owned());
            }
            summaries.push(format!("{provider}: {}", facts.join(", ")));
        }
    }
    if summaries.is_empty() {
        Some("call-site analysis coverage was not recorded for this exact pinned file version; reindex to record coverage, and verify callers or callees in source".to_owned())
    } else if partial {
        Some(format!(
            "source-level call coverage is partial ({}); these are whole-file observations, not a complete caller/callee or runtime-dispatch inventory",
            summaries.join("; ")
        ))
    } else {
        None
    }
}

struct TraceAssembly {
    nodes: Vec<FlowNode>,
    edges: Vec<FlowEdge>,
    truncated: bool,
    candidates_returned: usize,
    candidates_omitted: usize,
}

fn trace_paths(
    walks: &[knowell_graph::WalkResult],
    navigation: bool,
) -> Vec<(bool, &[knowell_graph::Hop])> {
    let mut paths = Vec::new();
    if navigation {
        // Interleave starts so a broad import file cannot fill the result
        // before the symbol's own neighborhood gets a slot.
        let mut positions = vec![0usize; walks.len()];
        loop {
            let mut advanced = false;
            for (walk, position) in walks.iter().zip(&mut positions) {
                if let Some(visit) = walk.visits.get(*position) {
                    paths.push((false, visit.path.as_slice()));
                    *position = position.saturating_add(1);
                    advanced = true;
                }
            }
            if !advanced {
                break;
            }
        }
        for walk in walks {
            for visit in &walk.candidate_visits {
                paths.push((true, visit.path.as_slice()));
            }
        }
    } else {
        for walk in walks {
            for visit in &walk.visits {
                paths.push((false, visit.path.as_slice()));
            }
        }
    }
    paths
}

fn assemble_trace(
    ctx: &GraphCtx,
    starts: &[NodeId],
    walks: &[knowell_graph::WalkResult],
    navigation: bool,
    limit: usize,
    candidate_limit: usize,
) -> TraceAssembly {
    let mut output = TraceAssembly {
        nodes: Vec::new(),
        edges: Vec::new(),
        truncated: false,
        candidates_returned: 0,
        candidates_omitted: 0,
    };
    let mut local = BTreeMap::new();
    let add_node =
        |id: &NodeId, local: &mut BTreeMap<NodeId, String>, output: &mut TraceAssembly| {
            if let Some(existing) = local.get(id) {
                return Some(existing.clone());
            }
            if local.len() >= limit {
                output.truncated = true;
                return None;
            }
            let name = format!("n{}", local.len());
            let node = ctx.flow_node(id, name.clone())?;
            local.insert(id.clone(), name.clone());
            output.nodes.push(node);
            Some(name)
        };
    for start in starts {
        add_node(start, &mut local, &mut output);
    }
    let mut seen_edges = BTreeSet::new();
    for (candidate_path, path) in trace_paths(walks, navigation) {
        for hop in path {
            if !seen_edges.insert(hop.edge.clone()) {
                continue;
            }
            let Some(edge) = ctx.graph.edge(&hop.edge) else {
                continue;
            };
            let target_kind = ctx
                .graph
                .node(&hop.edge.to)
                .and_then(knowell_graph::Node::contract_kind);
            let Some(relation) = relation_of(edge.kind, target_kind) else {
                continue;
            };
            let candidate = candidate_path
                && (edge.resolution != knowell_graph::Resolution::Resolved
                    || !edge
                        .evidence
                        .at_least(knowell_graph::EvidenceType::Syntactic));
            if candidate && output.candidates_returned >= candidate_limit {
                output.candidates_omitted = output.candidates_omitted.saturating_add(1);
                output.truncated = true;
                continue;
            }
            // Reserve both endpoints together so a clipped edge cannot
            // consume a slot with an orphaned endpoint.
            let additional = BTreeSet::from([&hop.edge.from, &hop.edge.to])
                .into_iter()
                .filter(|id| !local.contains_key(*id))
                .count();
            if local
                .len()
                .checked_add(additional)
                .is_none_or(|total| total > limit)
            {
                output.truncated = true;
                if candidate {
                    output.candidates_omitted = output.candidates_omitted.saturating_add(1);
                }
                continue;
            }
            let (Some(from), Some(to)) = (
                add_node(&hop.edge.from, &mut local, &mut output),
                add_node(&hop.edge.to, &mut local, &mut output),
            ) else {
                continue;
            };
            output.edges.push(FlowEdge {
                from,
                to,
                relation,
                evidence_type: mcp_evidence(edge.evidence),
                resolution: mcp_resolution(edge.resolution),
                evidence: ctx.edge_evidence(edge),
            });
            if candidate {
                output.candidates_returned = output.candidates_returned.saturating_add(1);
            }
        }
    }
    output
}

/// The `contracts_not_extracted` gap (no relation stage installed).
pub(crate) fn contracts_gap() -> Gap {
    Gap::new(
        GapReason::NoRulePackForFramework,
        "contracts_not_extracted: no contract rule packs (knowell-link) ran for this workspace, so endpoints, topics, RPCs, tables, env names and i18n keys are not extracted yet",
    )
}

/// One changed item of an impact analysis.
struct Changed {
    node: Option<NodeId>,
    item: Option<ImpactItem>,
}

fn graph_project_available(pinned: &Pinned, project: &Name) -> Result<bool, ToolError> {
    if pinned.projects.contains_key(project) {
        return Ok(true);
    }
    if pinned.not_indexed.contains(project)
        || pinned.gaps.iter().any(|gap| {
            gap.project.as_ref() == Some(project) && gap.reason == GapReason::RefNotFound
        })
    {
        return Ok(false);
    }
    // Only authorized pins and gaps establish existence, so invisible and
    // nonexistent projects have the same response.
    Err(ToolError::not_found(format!(
        "project {project} does not exist"
    )))
}

fn change_description(change: &ChangeSubject) -> String {
    match change {
        ChangeSubject::Symbol { symbol } => format!(
            "symbol {}",
            symbol
                .symbol
                .clone()
                .or_else(|| symbol.id.as_ref().map(ToString::to_string))
                .unwrap_or_default()
        ),
        ChangeSubject::File { project, path } => format!("file {project}/{path}"),
        ChangeSubject::Diff {
            project,
            base,
            head,
        } => format!(
            "diff of {project} from {base} to {}",
            head.as_ref()
                .map_or_else(|| "the pinned view".to_owned(), ToString::to_string),
        ),
        ChangeSubject::Patch { project, .. } => format!("unapplied patch to {project}"),
    }
}

/// Opens local Git using the indexer's configuration isolation convention.
pub(crate) fn open_git_repo(path: &Path, mode: GitConfigMode) -> Result<GitRepo, GitError> {
    match mode {
        GitConfigMode::User => GitRepo::open(path),
        GitConfigMode::Isolated => GitRepo::open_isolated(path),
    }
}

/// Whether a source failure specifically means a requested ref is unavailable.
pub(crate) fn is_missing_git_ref(error: &GitError) -> bool {
    matches!(
        error,
        GitError::RefNotFound { .. }
            | GitError::CommitNotFound { .. }
            | GitError::UnbornHead { .. }
            | GitError::NotACommit { .. }
            | GitError::InvalidObjectId { .. }
    )
}

fn project_relative(path: &RepoPath, root: Option<&RepoPath>) -> Option<RepoPath> {
    match root {
        None => Some(path.clone()),
        Some(root) => path
            .as_str()
            .strip_prefix(root.as_str())
            .and_then(|suffix| suffix.strip_prefix('/'))
            .and_then(|suffix| RepoPath::new(suffix).ok()),
    }
}

impl Engine {
    async fn trace_call_coverage_gaps(
        &self,
        ctx: &GraphCtx,
        starts: &[NodeId],
    ) -> Result<Vec<Gap>, ToolError> {
        let mut checked = BTreeSet::new();
        let mut gaps = Vec::new();
        let mut connection = self.inner.store.acquire().await.map_err(store_tool)?;
        for start in starts {
            let Some(source) = ctx.graph.node(start).and_then(|node| node.source.as_ref()) else {
                continue;
            };
            if !checked.insert((
                source.project.clone(),
                source.path.clone(),
                source.content_hash,
            )) {
                continue;
            }
            let Some((project, snapshot)) = ctx.views.get(&source.project) else {
                continue;
            };
            let Some(file) = snapshot.file(&source.path) else {
                continue;
            };
            let coverage = knowell_store::analysis::coverage_at(
                &mut connection,
                self.inner.organization,
                project.pin(),
                &source.path,
                &file.content_hash,
            )
            .await
            .map_err(store_tool)?;
            if let Some(message) = call_coverage_message(&coverage) {
                gaps.push(Gap::for_project(
                    GapReason::NoReferenceResolutionForLanguage,
                    source.project.clone(),
                    format!("{}: {message}", source.path),
                ));
            }
        }
        Ok(gaps)
    }

    /// The configured path policy, checked on project-relative paths before reads.
    pub(crate) fn project_exclusion_policy(
        &self,
        pinned: &Pinned,
        project: &Name,
    ) -> Result<ExclusionPolicy, ToolError> {
        let resolved = pinned
            .workspace
            .resolved
            .projects
            .iter()
            .find(|entry| &entry.name == project)
            .ok_or_else(|| ToolError::internal("a pinned project's configuration is missing"))?;
        let mut patterns = self.inner.indexer.config().content.excluded_dirs.clone();
        patterns.extend(resolved.exclude.iter().map(|pattern| pattern.value.clone()));
        ExclusionPolicy::with_patterns(&patterns)
            .map_err(|error| ToolError::internal(format!("project exclusion policy: {error}")))
    }

    /// The code graph of the pinned manifest (cached per pin set).
    pub(crate) async fn code_graph(&self, pinned: &Pinned) -> Result<GraphCtx, ToolError> {
        let mut views = BTreeMap::new();
        let mut snapshots = Vec::new();
        for (name, project) in &pinned.projects {
            let snapshot = self.snapshot_of(project).await?;
            snapshots.push(Arc::clone(&snapshot));
            views.insert(name.clone(), (project.clone(), snapshot));
        }
        let mut pins: Vec<GenerationPin> =
            pinned.projects.values().map(PinnedProject::pin).collect();
        pins.sort();
        let graph = match self.inner.graphs.get(&pins) {
            Some(graph) => graph,
            None => {
                let built =
                    Arc::new(graph::build(&snapshots).map_err(|e| {
                        ToolError::internal(format!("building the code graph: {e}"))
                    })?);
                self.inner.graphs.put(pins, Arc::clone(&built));
                built
            }
        };
        Ok(GraphCtx { graph, views })
    }

    pub(crate) async fn tool_trace_flow(
        &self,
        access: Access,
        input: TraceFlowInput,
    ) -> Result<TraceFlowOutput, ToolError> {
        let pinned = self.resolve_target(&access, &input.target).await?;
        if let Some(job) = &input.job_id {
            return Err(ToolError::not_found(format!(
                "trace job {job} does not exist; traces are computed synchronously"
            )));
        }
        let mut gaps = pinned.gaps.clone();
        gaps.extend(pinned.not_indexed_gaps(&[]));
        if let Some(project) = &input.project {
            let available = match graph_project_available(&pinned, project) {
                Err(ToolError::NotFound(_)) if input.target.context_id.is_some() => {
                    // Contexts have already been re-filtered against current
                    // grants. Preserve their structured empty-trace response
                    // without probing a removed or invisible project's source.
                    gaps.push(Gap::new(
                        GapReason::NotFound,
                        "the start symbol, id or contract does not exist in the pinned views",
                    ));
                    false
                }
                result => result?,
            };
            if !available {
                dedupe(&mut gaps);
                return Ok(TraceFlowOutput {
                    gaps,
                    ..TraceFlowOutput::default()
                });
            }
        }
        let ctx = self.code_graph(&pinned).await?;
        gaps.extend(graph_overlay_gaps(&pinned));
        // The start: a symbol (and the file that carries its imports), a
        // file, or a contract.
        let mut starts: Vec<NodeId> = Vec::new();
        if input.id.is_some() || input.symbol.is_some() {
            let found = self
                .find_symbols(
                    &pinned,
                    input.id.as_ref(),
                    input.symbol.as_deref(),
                    input.project.as_ref(),
                )
                .await?;
            let best = found.first().map(|f| f.3);
            let top: Vec<_> = found.iter().filter(|f| Some(f.3) == best).collect();
            if top.len() > 1 {
                if input.navigation.unwrap_or(false) {
                    let limit = usize::try_from(input.limit.unwrap_or(50)).unwrap_or(50);
                    let nodes = top
                        .iter()
                        .take(limit)
                        .enumerate()
                        .filter_map(|(position, (project, _, symbol, _))| {
                            let id = symbol_node(&project.entry.name, &symbol.key);
                            ctx.flow_node(&id, format!("n{position}"))
                        })
                        .collect();
                    gaps.push(Gap::new(
                        GapReason::LimitReached,
                        format!(
                            "{} equally ranked start definitions match; navigation did not choose one or expand their relations; choose a qualified symbol and project; a shared source id cannot distinguish same-line definitions",
                            top.len()
                        ),
                    ));
                    dedupe(&mut gaps);
                    return Ok(TraceFlowOutput {
                        nodes,
                        truncated: top.len() > limit,
                        gaps,
                        ..TraceFlowOutput::default()
                    });
                }
                gaps.push(Gap::new(
                    GapReason::LimitReached,
                    format!(
                        "{} symbols match; tracing the first ({}:{}); pass `id` or `project` to choose",
                        top.len(),
                        top.first().map_or_else(String::new, |f| f.0.entry.name.to_string()),
                        top.first().map_or_else(String::new, |f| f.2.local.clone())
                    ),
                ));
            }
            if let Some((project, _, symbol, _)) = top.first() {
                starts.push(symbol_node(&project.entry.name, &symbol.key));
                starts.push(file_node(&project.entry.name, &symbol.path));
            } else if let Some(id) = &input.id
                && let Some(source) = crate::ids::parse_source_id(id)
                && let Some((project, snapshot)) = ctx.views.get(&source.project)
                && let Some(path) = self
                    .source_path_in_snapshot(project, snapshot, &source)
                    .await?
            {
                starts.push(file_node(&source.project, &path));
            }
        } else if let Some(contract) = &input.contract {
            let (kind_text, key) = match contract.split_once(':') {
                Some((k, rest))
                    if matches!(
                        k,
                        "endpoint"
                            | "topic"
                            | "rpc"
                            | "table"
                            | "env_name"
                            | "i18n_key"
                            | "package"
                    ) =>
                {
                    (Some(k), rest)
                }
                _ => (None, contract.as_str()),
            };
            for kind in [
                GContractKind::Endpoint,
                GContractKind::Topic,
                GContractKind::Rpc,
                GContractKind::Table,
                GContractKind::EnvName,
                GContractKind::I18nKey,
                GContractKind::Package,
            ] {
                if kind_text.is_some_and(|k| k != kind.as_str()) {
                    continue;
                }
                let id = NodeId::contract(kind, key);
                if ctx.graph.node(&id).is_some() {
                    starts.push(id);
                }
            }
            if starts.is_empty() && self.inner.settings.relation_stage.is_none() {
                gaps.push(contracts_gap());
            }
        }
        if let Some(gap) = relations_gap(self) {
            gaps.push(gap);
        }
        let Some(_) = starts.first() else {
            gaps.push(Gap::new(
                GapReason::NotFound,
                "the start symbol, id or contract does not exist in the pinned views",
            ));
            dedupe(&mut gaps);
            return Ok(TraceFlowOutput {
                gaps,
                ..TraceFlowOutput::default()
            });
        };
        let direction = match input.direction.unwrap_or(FlowDirection::Downstream) {
            FlowDirection::Downstream => Direction::Outgoing,
            FlowDirection::Upstream => Direction::Incoming,
            FlowDirection::Both => Direction::Both,
        };
        let mut kinds: BTreeSet<EdgeKind> = BTreeSet::new();
        if input.relations.is_empty() {
            for relation in [
                RelationKind::Calls,
                RelationKind::References,
                RelationKind::Implements,
                RelationKind::Imports,
                RelationKind::HttpCall,
                RelationKind::HttpRoute,
                RelationKind::Publishes,
                RelationKind::ReadsTable,
                RelationKind::WritesTable,
                RelationKind::DependsOnPackage,
                RelationKind::Tests,
                RelationKind::Documents,
            ] {
                kinds.extend(graph_kinds(relation));
            }
        } else {
            for relation in &input.relations {
                kinds.extend(graph_kinds(*relation));
            }
        }
        let limit = usize::try_from(input.limit.unwrap_or(50)).unwrap_or(50);
        let filter = graph_context_filter(&pinned, EdgeFilter::any().with_kinds(kinds), &mut gaps);
        let spec = WalkSpec::new(input.max_depth.unwrap_or(3), direction)
            .with_filter(filter)
            .with_node_budget(limit.max(1));
        let mut truncated = false;
        let navigation = input.navigation.unwrap_or(false);
        if navigation || input.relations.contains(&RelationKind::Calls) {
            gaps.extend(self.trace_call_coverage_gaps(&ctx, &starts).await?);
        }
        let neighbor_limit = usize::try_from(input.neighbor_limit.unwrap_or(12)).unwrap_or(12);
        let candidate_limit = usize::try_from(input.candidate_limit.unwrap_or(8)).unwrap_or(8);
        let mut walks = Vec::new();
        let mut fanout_omitted = 0usize;
        let mut candidate_omitted = 0usize;
        let mut depth_omitted = 0usize;
        for start in &starts {
            let walk = if navigation {
                ctx.graph
                    .walk_navigation(start, &spec, neighbor_limit, candidate_limit)
            } else {
                ctx.graph.walk(start, &spec)
            }
            .map_err(|e| ToolError::internal(format!("walking the graph: {e}")))?;
            truncated |= walk.truncated;
            fanout_omitted = fanout_omitted.saturating_add(walk.fanout_omitted);
            candidate_omitted = candidate_omitted.saturating_add(walk.candidate_omitted);
            depth_omitted = depth_omitted.saturating_add(walk.depth_omitted);
            walks.push(walk);
        }
        let assembly = assemble_trace(&ctx, &starts, &walks, navigation, limit, candidate_limit);
        truncated |= assembly.truncated;
        candidate_omitted = candidate_omitted.saturating_add(assembly.candidates_omitted);
        let nodes = assembly.nodes;
        let edges = assembly.edges;
        let candidates_returned = assembly.candidates_returned;
        if edges.is_empty() && !truncated {
            gaps.push(Gap::new(
                GapReason::NoCandidatesInSelectedRef,
                "no indexed relation matched the selected kinds and direction at this source pin; this does not establish that calls or dependencies are absent",
            ));
        }
        if navigation && candidates_returned > 0 {
            gaps.push(Gap::new(
                GapReason::RelationsNotReady,
                format!(
                    "{candidates_returned} weak, ambiguous or unresolved candidate edges are shown for inspection only; these edges were not used to carry trace expansion"
                ),
            ));
        }
        if navigation && (fanout_omitted > 0 || candidate_omitted > 0 || depth_omitted > 0) {
            gaps.push(Gap::new(
                GapReason::LimitReached,
                format!(
                    "navigation omitted inspected adjacencies: {fanout_omitted} by per-node fanout, {candidate_omitted} by candidate or node budget, {depth_omitted} beyond depth; these counts do not enumerate the unseen neighborhood"
                ),
            ));
        }
        if truncated {
            gaps.push(Gap::new(
                GapReason::LimitReached,
                "the trace reached its node or walk budget",
            ));
        }
        dedupe(&mut gaps);
        Ok(TraceFlowOutput {
            nodes,
            edges,
            truncated,
            job: None,
            gaps,
        })
    }

    /// An impact item for a graph node.
    fn impact_item(
        ctx: &GraphCtx,
        node: &knowell_graph::Node,
        distance: u8,
        why: Vec<MatchReason>,
    ) -> Option<ImpactItem> {
        let (id, evidence) = ctx.evidence(node, why)?;
        let kind = match &node.kind {
            GNodeKind::Contract { .. } => ImpactKind::Contract,
            GNodeKind::Test => ImpactKind::Test,
            GNodeKind::File if is_test_path(&evidence.path) => ImpactKind::Test,
            GNodeKind::File => match evidence.path.extension() {
                Some("yaml" | "yml" | "json" | "toml" | "ini" | "env") => ImpactKind::Config,
                _ => ImpactKind::File,
            },
            _ => ImpactKind::Symbol,
        };
        let name = match node.kind {
            GNodeKind::File => evidence.path.to_string(),
            _ => node.name.clone(),
        };
        Some(ImpactItem {
            id,
            kind,
            name,
            distance,
            evidence,
        })
    }

    pub(crate) async fn tool_analyze_impact(
        &self,
        access: Access,
        input: AnalyzeImpactInput,
    ) -> Result<AnalyzeImpactOutput, ToolError> {
        let pinned = self.resolve_target(&access, &input.target).await?;
        if let Some(job) = &input.job_id {
            return Err(ToolError::not_found(format!(
                "impact job {job} does not exist; impact is computed synchronously"
            )));
        }
        let Some(change) = &input.change else {
            return Err(ToolError::invalid_input("pass `change`"));
        };
        let mut gaps = pinned.gaps.clone();
        gaps.extend(pinned.not_indexed_gaps(&[]));
        let project = match change {
            ChangeSubject::Symbol { symbol } => symbol.project.as_ref(),
            ChangeSubject::File { project, .. }
            | ChangeSubject::Diff { project, .. }
            | ChangeSubject::Patch { project, .. } => Some(project),
        };
        if let Some(project) = project
            && !graph_project_available(&pinned, project)?
        {
            dedupe(&mut gaps);
            return Ok(AnalyzeImpactOutput {
                subject: change_description(change),
                gaps,
                ..AnalyzeImpactOutput::default()
            });
        }
        let ctx = self.code_graph(&pinned).await?;
        gaps.extend(graph_overlay_gaps(&pinned));
        let (subject, changed) = match change {
            ChangeSubject::Symbol { symbol } => {
                let found = self
                    .find_symbols(
                        &pinned,
                        symbol.id.as_ref(),
                        symbol.symbol.as_deref(),
                        symbol.project.as_ref(),
                    )
                    .await?;
                let best = found.first().map(|f| f.3);
                let mut changed = Vec::new();
                for (project, _, entry, strength) in &found {
                    if Some(*strength) != best {
                        continue;
                    }
                    let node_id = symbol_node(&project.entry.name, &entry.key);
                    if let Some(node) = ctx.graph.node(&node_id)
                        && let Some(item) = Self::impact_item(&ctx, node, 0, Vec::new())
                    {
                        changed.push(Changed {
                            node: Some(node_id),
                            item: Some(item),
                        });
                        // The file carries the symbol's imports; it is a
                        // propagation target, not a changed item.
                        changed.push(Changed {
                            node: Some(file_node(&project.entry.name, &entry.path)),
                            item: None,
                        });
                    }
                }
                let name = symbol
                    .symbol
                    .clone()
                    .or_else(|| symbol.id.as_ref().map(|id| id.to_string()))
                    .unwrap_or_default();
                (format!("symbol {name}"), changed)
            }
            ChangeSubject::File { project, path } => {
                let mut changed = Vec::new();
                if let Some((_, snapshot)) = ctx.views.get(project)
                    && snapshot.file(path).is_some()
                {
                    let node_id = file_node(project, path);
                    if let Some(node) = ctx.graph.node(&node_id)
                        && let Some(item) = Self::impact_item(&ctx, node, 0, Vec::new())
                    {
                        changed.push(Changed {
                            node: Some(node_id),
                            item: Some(item),
                        });
                    }
                } else if !pinned.projects.contains_key(project)
                    && !pinned.not_indexed.contains(project)
                {
                    return Err(ToolError::not_found(format!(
                        "project {project} does not exist"
                    )));
                }
                (format!("file {project}/{path}"), changed)
            }
            ChangeSubject::Diff {
                project,
                base,
                head,
            } => {
                let (changed, diff_gaps) = self
                    .diff_changes(&ctx, &pinned, project, base, head.as_ref())
                    .await?;
                gaps.extend(diff_gaps);
                (
                    format!(
                        "diff of {project} from {base} to {}",
                        head.as_ref()
                            .map_or_else(|| "the pinned view".to_owned(), ToString::to_string)
                    ),
                    changed,
                )
            }
            ChangeSubject::Patch { project, patch } => {
                let (changed, patch_gaps) =
                    self.patch_changes(&ctx, &pinned, project, patch).await?;
                gaps.extend(patch_gaps);
                (format!("unapplied patch to {project}"), changed)
            }
        };
        if changed.is_empty()
            && gaps.iter().any(|gap| {
                matches!(
                    gap.reason,
                    GapReason::ProjectNotIndexed | GapReason::RefNotFound
                ) && project.is_none_or(|project| gap.project.as_ref() == Some(project))
            })
        {
            dedupe(&mut gaps);
            return Ok(AnalyzeImpactOutput {
                subject,
                gaps,
                ..AnalyzeImpactOutput::default()
            });
        }
        if let Some(gap) = relations_gap(self) {
            gaps.push(gap);
        }
        let targets: Vec<NodeId> = changed
            .iter()
            .filter_map(|c| c.node.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let limit = usize::try_from(input.limit.unwrap_or(50)).unwrap_or(50);
        let include_tests = input.include_tests.unwrap_or(true);
        let mut impacted = Vec::new();
        let mut tests = Vec::new();
        let mut truncated = false;
        let mut unresolved = false;
        if !targets.is_empty() {
            let filter = graph_context_filter(&pinned, EdgeFilter::any(), &mut gaps);
            let spec = ImpactSpec::new(targets.clone())
                .with_filter(filter)
                .with_max_depth(input.max_depth.unwrap_or(3))
                .with_node_budget(limit.saturating_mul(4).max(1));
            let report = ctx
                .graph
                .impact(&spec)
                .map_err(|e| ToolError::internal(format!("impact: {e}")))?;
            truncated = report.truncated;
            unresolved = !report.unknown.is_empty();
            for affected in report.affected.iter().chain(report.tests.iter()) {
                let Some(node) = ctx.graph.node(&affected.node) else {
                    continue;
                };
                if node.is_unresolved_placeholder() || targets.contains(&affected.node) {
                    continue;
                }
                let hops: Vec<GraphHop> = affected
                    .risk
                    .carried_by
                    .iter()
                    .filter_map(|hop| {
                        let edge = ctx.graph.edge(&hop.edge)?;
                        let target_kind = ctx
                            .graph
                            .node(&hop.edge.to)
                            .and_then(knowell_graph::Node::contract_kind);
                        Some(GraphHop {
                            from: ctx
                                .graph
                                .node(&hop.from)
                                .map_or_else(|| hop.from.to_string(), |n| n.name.clone()),
                            relation: relation_of(edge.kind, target_kind)
                                .unwrap_or(RelationKind::References),
                            to: ctx
                                .graph
                                .node(&hop.to)
                                .map_or_else(|| hop.to.to_string(), |n| n.name.clone()),
                            evidence_type: mcp_evidence(hop.evidence),
                            resolution: mcp_resolution(hop.resolution),
                        })
                    })
                    .collect();
                let distance = u8::try_from(affected.depth).unwrap_or(u8::MAX);
                let Some(item) =
                    Self::impact_item(&ctx, node, distance, vec![MatchReason::GraphPath { hops }])
                else {
                    continue;
                };
                if item.kind == ImpactKind::Test {
                    tests.push(item);
                } else {
                    impacted.push(item);
                }
            }
        }
        impacted.sort_by(|a, b| {
            a.distance
                .cmp(&b.distance)
                .then_with(|| a.evidence.project.cmp(&b.evidence.project))
                .then_with(|| a.name.cmp(&b.name))
        });
        tests.sort_by(|a, b| {
            a.distance
                .cmp(&b.distance)
                .then_with(|| a.name.cmp(&b.name))
        });
        // Omitting a result list must not erase its evidence from risk or
        // report that the omitted list exceeded the requested output limit.
        let has_test_evidence = !tests.is_empty();
        if impacted.len() > limit || (include_tests && tests.len() > limit) {
            truncated = true;
        }
        impacted.truncate(limit);
        let changed_items: Vec<ImpactItem> = changed.into_iter().filter_map(|c| c.item).collect();
        let changed_projects: BTreeSet<Name> = changed_items
            .iter()
            .map(|i| i.evidence.project.clone())
            .collect();
        let mut factors = Vec::new();
        if impacted
            .iter()
            .any(|i| !changed_projects.contains(&i.evidence.project))
        {
            factors.push(RiskFactor {
                code: RiskCode::CrossProjectConsumers,
                message: "other projects depend on the changed code".to_owned(),
                evidence: impacted
                    .iter()
                    .filter(|i| !changed_projects.contains(&i.evidence.project))
                    .take(3)
                    .map(|i| i.evidence.clone())
                    .collect(),
            });
        }
        if impacted.iter().any(|i| i.kind == ImpactKind::Contract) {
            factors.push(RiskFactor {
                code: RiskCode::PublicContractChanged,
                message: "a cross-project contract is reached".to_owned(),
                evidence: Vec::new(),
            });
        }
        if impacted.len() >= 10 {
            factors.push(RiskFactor {
                code: RiskCode::ManyDependents,
                message: format!(
                    "{} dependents within {} hops",
                    impacted.len(),
                    input.max_depth.unwrap_or(3)
                ),
                evidence: Vec::new(),
            });
        }
        if !changed_items.is_empty() && !has_test_evidence {
            factors.push(RiskFactor {
                code: RiskCode::UntestedCode,
                message: "no test file imports the changed code (file-level evidence only)"
                    .to_owned(),
                evidence: Vec::new(),
            });
        }
        if unresolved {
            factors.push(RiskFactor {
                code: RiskCode::UnresolvedReferences,
                message: "some imports are unresolved; impact may be larger".to_owned(),
                evidence: Vec::new(),
            });
        }
        let level = if factors.iter().any(|f| {
            matches!(
                f.code,
                RiskCode::CrossProjectConsumers
                    | RiskCode::PublicContractChanged
                    | RiskCode::ManyDependents
            )
        }) {
            RiskLevel::High
        } else if !impacted.is_empty() || !factors.is_empty() {
            RiskLevel::Medium
        } else {
            RiskLevel::Low
        };
        if changed_items.is_empty() {
            gaps.push(Gap::new(
                GapReason::NotFound,
                "nothing in the pinned views matches the change",
            ));
        }
        let mut languages = BTreeSet::new();
        for item in &changed_items {
            if let Some((_, snapshot)) = ctx.views.get(&item.evidence.project)
                && let Some(language) = snapshot
                    .file(&item.evidence.path)
                    .and_then(|f| f.language.clone())
            {
                languages.insert((item.evidence.project.clone(), language));
            }
        }
        for (project, language) in languages {
            gaps.push(Gap::for_project(
                GapReason::NoReferenceResolutionForLanguage,
                project,
                format!("{language}: dependents are files that import the changed files; call-level impact needs reference resolution"),
            ));
        }
        dedupe(&mut gaps);
        if include_tests {
            tests.truncate(limit);
        } else {
            tests.clear();
        }
        Ok(AnalyzeImpactOutput {
            subject,
            changed: changed_items,
            impacted,
            tests,
            risk: Some(Risk { level, factors }),
            truncated,
            job: None,
            gaps,
        })
    }

    /// Changed files and symbols of a committed diff between two refs.
    async fn diff_changes(
        &self,
        ctx: &GraphCtx,
        pinned: &Pinned,
        project: &Name,
        base: &TrackTarget,
        head: Option<&TrackTarget>,
    ) -> Result<(Vec<Changed>, Vec<Gap>), ToolError> {
        let Some((pinned_project, snapshot)) = ctx.views.get(project) else {
            if pinned.not_indexed.contains(project) {
                return Ok((
                    Vec::new(),
                    vec![Gap::for_project(
                        GapReason::ProjectNotIndexed,
                        project.clone(),
                        format!("{project} has no index yet"),
                    )],
                ));
            }
            return Err(ToolError::not_found(format!(
                "project {project} does not exist"
            )));
        };
        let path = pinned_project.entry.path.clone();
        let root = pinned_project.entry.root.clone();
        let base = base.clone();
        let head = head.cloned();
        let head_commit = pinned_project.commit.clone();
        let policy = self.project_exclusion_policy(pinned, project)?;
        let config = self.inner.indexer.config();
        let mode = config.git_config;
        let options = WalkOptions {
            max_file_bytes: config.limits.max_file_bytes,
            ..WalkOptions::default()
        };
        if head.is_none() && head_commit.is_none() {
            return Ok((
                Vec::new(),
                vec![Gap::for_project(
                    GapReason::RefNotFound,
                    project.clone(),
                    "the pinned view has no git commit",
                )],
            ));
        }
        let outcome = tokio::task::spawn_blocking(move || -> Result<DiffOutcome, DiffFailure> {
            let repo = open_git_repo(&path, mode).map_err(DiffFailure::Source)?;
            let resolve = |target: &TrackTarget| {
                repo.resolve(target)
                    .map(|resolved| resolved.commit)
                    .map_err(|error| {
                        if is_missing_git_ref(&error) {
                            DiffFailure::MissingRef(target.clone())
                        } else {
                            DiffFailure::Source(error)
                        }
                    })
            };
            let base_commit = resolve(&base)?;
            let head_commit = match head.as_ref() {
                Some(target) => resolve(target)?,
                None => head_commit.ok_or_else(|| {
                    DiffFailure::Source(GitError::Git {
                        operation: "committed diff",
                        message: "the pinned view has no git commit".to_owned(),
                    })
                })?,
            };
            let changes = repo
                .diff_scoped(&base_commit, &head_commit, root.as_ref(), &policy, &options)
                .map_err(DiffFailure::Source)?;
            let mut files = Vec::new();
            let mut skipped = Vec::new();
            for change in changes {
                let (old, new) = match &change {
                    Change::Added(p) => (None, Some(p.clone())),
                    Change::Modified(p) => (Some(p.clone()), Some(p.clone())),
                    Change::Deleted(p) => (Some(p.clone()), None),
                    Change::Renamed { from, to, .. } => (Some(from.clone()), Some(to.clone())),
                };
                // The scoped diff has already filtered both endpoints before
                // any similarity read. Recheck that boundary before reading
                // texts; configured patterns use project-relative paths.
                let scoped_path = |path: &Option<RepoPath>| {
                    path.as_ref()
                        .map(|path| {
                            project_relative(path, root.as_ref())
                                .filter(|relative| policy.check(relative).is_none())
                                .ok_or_else(|| {
                                    DiffFailure::Source(GitError::Git {
                                        operation: "scoped diff validation",
                                        message: "a change escaped the selected project policy"
                                            .to_owned(),
                                    })
                                })
                        })
                        .transpose()
                };
                let old_path = scoped_path(&old)?;
                let new_path = scoped_path(&new)?;
                let (old_text, old_skip) =
                    read_diff_text(&repo, &base_commit, old.as_ref(), &options)
                        .map_err(DiffFailure::Source)?;
                let (new_text, new_skip) =
                    read_diff_text(&repo, &head_commit, new.as_ref(), &options)
                        .map_err(DiffFailure::Source)?;
                let contents_complete = old_skip.is_none() && new_skip.is_none();
                if let Some((path, reason)) = old_path.as_ref().zip(old_skip) {
                    skipped.push((path.clone(), reason));
                }
                if let Some((path, reason)) = new_path.as_ref().zip(new_skip) {
                    skipped.push((path.clone(), reason));
                }
                files.push(DiffFile {
                    old: old_path,
                    new: new_path,
                    old_text,
                    new_text,
                    contents_complete,
                });
            }
            Ok(DiffOutcome { files, skipped })
        })
        .await
        .map_err(|e| ToolError::internal(format!("git diff task: {e}")))?;
        let outcome = match outcome {
            Ok(o) => o,
            Err(DiffFailure::MissingRef(target)) => {
                return Ok((
                    Vec::new(),
                    vec![Gap::for_project(
                        GapReason::RefNotFound,
                        project.clone(),
                        format!("ref {target} does not exist; no other ref was used in its place"),
                    )],
                ));
            }
            Err(DiffFailure::Source(error)) => {
                return Err(ToolError::internal(format!(
                    "reading committed changes: {error}"
                )));
            }
        };
        let gaps = outcome.skipped.into_iter().map(|(path, reason)| Gap::for_project(
            GapReason::ExcludedByPolicy, project.clone(),
            format!("{path}: changed content is unavailable ({reason}); symbol changes were not inferred"),
        )).collect();
        let mut changed = Vec::new();
        for file in outcome.files {
            let DiffFile {
                old,
                new,
                old_text,
                new_text,
                contents_complete,
            } = file;
            let Some(path) = new.clone().or(old.clone()) else {
                continue;
            };
            let symbols = if contents_complete {
                changed_symbols(
                    old.as_ref().zip(old_text.as_deref()),
                    new.as_ref().zip(new_text.as_deref()),
                )
            } else {
                // Missing text is not evidence that its symbols were removed.
                BTreeMap::new()
            };
            changed.extend(self.changed_items(
                ctx,
                project,
                snapshot,
                &path,
                old.as_ref(),
                &symbols,
                None,
            )?);
        }
        Ok((changed, gaps))
    }

    /// Changed files and symbols of an unapplied unified diff against the
    /// pinned view.
    async fn patch_changes(
        &self,
        ctx: &GraphCtx,
        pinned: &Pinned,
        project: &Name,
        text: &str,
    ) -> Result<(Vec<Changed>, Vec<Gap>), ToolError> {
        let Some((_, snapshot)) = ctx.views.get(project) else {
            if pinned.not_indexed.contains(project) {
                return Ok((
                    Vec::new(),
                    vec![Gap::for_project(
                        GapReason::ProjectNotIndexed,
                        project.clone(),
                        format!("{project} has no index yet"),
                    )],
                ));
            }
            return Err(ToolError::not_found(format!(
                "project {project} does not exist"
            )));
        };
        let files =
            patch::parse(text).map_err(|e| ToolError::invalid_input(format!("patch: {e}")))?;
        let mut gaps = Vec::new();
        let mut changed = Vec::new();
        for file in &files {
            let Some(path) = file.path().cloned() else {
                continue;
            };
            let base_text = match file.old_path.as_ref().and_then(|p| snapshot.file(p)) {
                Some(info) => self
                    .inner
                    .texts
                    .load(
                        &self.inner.store,
                        self.inner.organization,
                        &info.content_hash,
                    )
                    .await
                    .map_err(ToolError::from)?,
                None => None,
            };
            if file.old_path.is_some() && base_text.is_none() {
                gaps.push(Gap::for_project(
                    GapReason::NotFound,
                    project.clone(),
                    format!("{path} is not in the pinned view; the patch was made against another version"),
                ));
                continue;
            }
            let new_text = match &base_text {
                Some(base) => patch::apply(base, file),
                None => patch::apply("", file),
            };
            if new_text.is_none() {
                gaps.push(Gap::for_project(
                    GapReason::NotFound,
                    project.clone(),
                    format!("the patch does not apply cleanly to {path} at the pinned commit; changed lines were mapped by the hunk headers"),
                ));
            }
            let mut symbols = match (&base_text, &new_text) {
                (Some(base), Some(new)) => changed_symbols(
                    file.old_path.as_ref().map(|p| (p, base.as_ref())),
                    file.new_path.as_ref().map(|p| (p, new.as_str())),
                ),
                (None, Some(new)) => {
                    changed_symbols(None, file.new_path.as_ref().map(|p| (p, new.as_str())))
                }
                _ => BTreeMap::new(),
            };
            if new_text.is_none()
                && let (Some(base), Some(old)) = (&base_text, &file.old_path)
            {
                let parsed = parse(old, base);
                for hunk in &file.hunks {
                    let Some(touched) = hunk.old_touched() else {
                        continue;
                    };
                    for symbol in &parsed.symbols {
                        if symbol.range.overlaps(&touched) && !symbol.qualified_name.is_empty() {
                            symbols
                                .entry(symbol.qualified_name.clone())
                                .or_insert(SymbolChange::Modified);
                        }
                    }
                }
            }
            let hunk_lines = file.hunks.iter().find_map(patch::Hunk::old_touched);
            changed.extend(self.changed_items(
                ctx,
                project,
                snapshot,
                &path,
                file.old_path.as_ref(),
                &symbols,
                hunk_lines,
            )?);
        }
        Ok((changed, gaps))
    }

    /// Items (file + leaf-most changed symbols) for one changed file.
    #[allow(clippy::too_many_arguments)]
    fn changed_items(
        &self,
        ctx: &GraphCtx,
        project: &Name,
        snapshot: &Snapshot,
        path: &RepoPath,
        old_path: Option<&RepoPath>,
        symbols: &BTreeMap<String, SymbolChange>,
        hint: Option<LineRange>,
    ) -> Result<Vec<Changed>, ToolError> {
        let mut out = Vec::new();
        let base_path = old_path
            .filter(|p| snapshot.file(p).is_some())
            .or_else(|| snapshot.file(path).map(|_| path));
        let Some(base_path) = base_path else {
            // A new file: nothing indexed depends on it yet.
            return Ok(out);
        };
        let file_id = file_node(project, base_path);
        if let Some(node) = ctx.graph.node(&file_id)
            && let Some(item) = Self::impact_item(ctx, node, 0, Vec::new())
        {
            out.push(Changed {
                node: Some(file_id),
                item: Some(item),
            });
        }
        let Some(file_item) = out.first().and_then(|c| c.item.clone()) else {
            return Ok(out);
        };
        for (local, change) in symbols {
            match snapshot.symbol_by_local(base_path, local) {
                Some(symbol) => {
                    let node_id = symbol_node(project, &symbol.key);
                    if let Some(node) = ctx.graph.node(&node_id)
                        && let Some(mut item) = Self::impact_item(ctx, node, 0, Vec::new())
                    {
                        if *change == SymbolChange::Removed {
                            item.name = format!("{} (removed)", item.name);
                        }
                        out.push(Changed {
                            node: Some(node_id),
                            item: Some(item),
                        });
                    }
                }
                None => {
                    // Added by the change: cite the base file where it lands.
                    let mut item = file_item.clone();
                    item.kind = ImpactKind::Symbol;
                    item.name = format!("{local} (added)");
                    if let Some(lines) = hint {
                        item.evidence.lines = lines;
                    }
                    item.evidence.symbol = Some(local.clone());
                    out.push(Changed {
                        node: None,
                        item: Some(item),
                    });
                }
            }
        }
        Ok(out)
    }

    pub(crate) async fn tool_contracts(
        &self,
        access: Access,
        input: ContractsInput,
    ) -> Result<ContractsOutput, ToolError> {
        let pinned = self.resolve_target(&access, &input.target).await?;
        let mut gaps = pinned.gaps.clone();
        gaps.extend(graph_overlay_gaps(&pinned));
        if let Some(project) = &input.project
            && !pinned.projects.contains_key(project)
            && !pinned.not_indexed.contains(project)
        {
            return Err(ToolError::not_found(format!(
                "project {project} does not exist"
            )));
        }
        gaps.extend(pinned.not_indexed_gaps(&[]));
        if self.inner.settings.relation_stage.is_none() {
            gaps.push(contracts_gap());
        }
        let limit = usize::try_from(input.limit.unwrap_or(20)).unwrap_or(20);
        let query = input
            .query
            .as_deref()
            .map(|q| q.trim().to_lowercase())
            .filter(|q| !q.is_empty());
        let kinds: Vec<knowell_store::ContractKind> = input
            .kinds
            .iter()
            .copied()
            .map(store_contract_kind)
            .collect();
        // Every participation of every pinned (visible) project, grouped by
        // contract.
        let mut grouped: BTreeMap<(knowell_store::ContractKind, String), Vec<ContractParticipant>> =
            BTreeMap::new();
        for (name, project) in &pinned.projects {
            let snapshot = self.snapshot_of(project).await?;
            for party in &snapshot.contracts {
                let contract = &party.contract;
                if !kinds.is_empty() && !kinds.contains(&contract.kind) {
                    continue;
                }
                if let Some(q) = &query
                    && !contract_matches(contract.kind, &contract.key, q)
                {
                    continue;
                }
                let Some(path) = crate::snapshot::origin_path(&contract.origin) else {
                    continue;
                };
                let Some(file) = snapshot.file(&path) else {
                    continue;
                };
                let Some(commit) = project.commit_id() else {
                    continue;
                };
                let lines = contract
                    .evidence
                    .get("lines")
                    .and_then(|l| l.as_array())
                    .and_then(|l| {
                        let a = u32::try_from(l.first()?.as_u64()?).ok()?;
                        let b = u32::try_from(l.get(1)?.as_u64()?).ok()?;
                        LineRange::new(a, b).ok()
                    })
                    .or_else(|| whole_file(file.line_count));
                let Some(lines) = lines else { continue };
                grouped
                    .entry((contract.kind, contract.key.clone()))
                    .or_default()
                    .push(ContractParticipant {
                        project: name.clone(),
                        role: match contract.role {
                            knowell_store::ContractRole::Producer => ContractRole::Producer,
                            knowell_store::ContractRole::Consumer => ContractRole::Consumer,
                        },
                        evidence_type: match contract.evidence_type {
                            knowell_store::EvidenceType::SemanticResolved => {
                                EvidenceType::SemanticallyResolved
                            }
                            knowell_store::EvidenceType::ContractDerived => {
                                EvidenceType::ContractDerived
                            }
                            knowell_store::EvidenceType::Syntactic => {
                                EvidenceType::SyntacticObservation
                            }
                            knowell_store::EvidenceType::Heuristic => EvidenceType::HeuristicMatch,
                            knowell_store::EvidenceType::ModelSuggestion => {
                                EvidenceType::ModelSuggestion
                            }
                            knowell_store::EvidenceType::RuntimeObserved => {
                                EvidenceType::RuntimeObservation
                            }
                        },
                        resolution: Resolution::Resolved,
                        evidence: Evidence {
                            project: name.clone(),
                            view: project.target.clone(),
                            layer: ViewLayer::Shared,
                            commit,
                            path: path.clone(),
                            lines,
                            content_hash: file.content_hash,
                            symbol: contract
                                .symbol
                                .and_then(|id| snapshot.symbol_by_id(id))
                                .map(|s| s.local.clone()),
                            why: vec![MatchReason::Contract {
                                contract: contract.key.clone(),
                            }],
                            freshness: FreshnessTier::T3Relations,
                            index_state: project.index_state(),
                        },
                    });
            }
        }
        let mut contracts = Vec::new();
        let mut more_available = false;
        for ((kind, key), mut participants) in grouped {
            if input
                .project
                .as_ref()
                .is_some_and(|p| !participants.iter().any(|x| &x.project == p))
            {
                continue;
            }
            if contracts.len() >= limit {
                more_available = true;
                break;
            }
            participants.sort_by(|a, b| {
                (
                    a.project.as_str(),
                    a.evidence.path.as_str(),
                    a.evidence.lines.start(),
                )
                    .cmp(&(
                        b.project.as_str(),
                        b.evidence.path.as_str(),
                        b.evidence.lines.start(),
                    ))
            });
            let producers = participants
                .iter()
                .filter(|p| p.role == ContractRole::Producer)
                .count();
            let consumers = participants.len().saturating_sub(producers);
            let mut drift = Vec::new();
            if kind == knowell_store::ContractKind::Endpoint && producers > 0 && consumers == 0 {
                drift.push(DriftFinding {
                    code: DriftCode::EndpointWithoutClient,
                    message: "no analysed project calls this endpoint".to_owned(),
                    evidence: Vec::new(),
                });
            }
            if kind == knowell_store::ContractKind::Topic && producers > 0 && consumers == 0 {
                drift.push(DriftFinding {
                    code: DriftCode::EventWithoutConsumer,
                    message: "no analysed project consumes this event".to_owned(),
                    evidence: Vec::new(),
                });
            }
            if input.only_drift.unwrap_or(false) && drift.is_empty() {
                continue;
            }
            contracts.push(ContractInfo {
                id: contract_id(kind.as_str(), &key)?,
                kind: mcp_contract_kind(kind),
                key,
                participants,
                drift,
            });
        }
        if more_available {
            gaps.push(Gap::new(
                GapReason::LimitReached,
                "more contracts exist; raise `limit`",
            ));
        }
        if contracts.is_empty()
            && !gaps
                .iter()
                .any(|g| g.reason == GapReason::NoRulePackForFramework)
        {
            gaps.push(Gap::new(
                GapReason::NoCandidatesInSelectedRef,
                "no contract in the pinned views matches",
            ));
        }
        dedupe(&mut gaps);
        Ok(ContractsOutput {
            contracts,
            more_available,
            gaps,
        })
    }
}

/// One changed file of a git diff: both paths and both texts.
struct DiffFile {
    old: Option<RepoPath>,
    new: Option<RepoPath>,
    old_text: Option<String>,
    new_text: Option<String>,
    contents_complete: bool,
}

struct DiffOutcome {
    files: Vec<DiffFile>,
    skipped: Vec<(RepoPath, &'static str)>,
}

enum DiffFailure {
    MissingRef(TrackTarget),
    Source(GitError),
}

/// A missing endpoint describes an addition/deletion, while an unreadable
/// endpoint is an operational failure and skipped text leaves an explicit gap.
fn read_diff_text(
    repo: &GitRepo,
    commit: &str,
    path: Option<&RepoPath>,
    options: &WalkOptions,
) -> Result<(Option<String>, Option<&'static str>), GitError> {
    let Some(path) = path else {
        return Ok((None, None));
    };
    match repo.read_commit_file(commit, path, &ExclusionPolicy::builtin(), options)? {
        FileRead::File(file) => Ok((Some(file.text), None)),
        FileRead::Missing => Err(GitError::PathNotFound {
            commit: commit.to_owned(),
            path: path.clone(),
        }),
        FileRead::Skipped(SkipReason::Unreadable { error_kind }) => Err(GitError::Git {
            operation: "reading changed content",
            message: error_kind,
        }),
        FileRead::Skipped(reason) => Ok((None, Some(reason.as_str()))),
    }
}

/// How a symbol changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SymbolChange {
    Added,
    Removed,
    Modified,
}

/// Symbols (by in-file qualified name) that were added, removed or whose
/// text changed between two versions of a file; a container is left out
/// when one of its members explains the change.
pub(crate) fn changed_symbols(
    old: Option<(&RepoPath, &str)>,
    new: Option<(&RepoPath, &str)>,
) -> BTreeMap<String, SymbolChange> {
    let texts = |side: Option<(&RepoPath, &str)>| -> BTreeMap<String, String> {
        let Some((path, text)) = side else {
            return BTreeMap::new();
        };
        let parsed = parse(path, text);
        parsed
            .symbols
            .iter()
            .filter(|s| !s.qualified_name.is_empty())
            .map(|s| (s.qualified_name.clone(), slice_lines(text, s.range)))
            .collect()
    };
    let old_symbols = texts(old);
    let new_symbols = texts(new);
    let mut out = BTreeMap::new();
    for (name, text) in &old_symbols {
        match new_symbols.get(name) {
            None => {
                out.insert(name.clone(), SymbolChange::Removed);
            }
            Some(new_text) if new_text != text => {
                out.insert(name.clone(), SymbolChange::Modified);
            }
            Some(_) => {}
        }
    }
    for name in new_symbols.keys() {
        if !old_symbols.contains_key(name) {
            out.insert(name.clone(), SymbolChange::Added);
        }
    }
    let names: Vec<String> = out.keys().cloned().collect();
    out.retain(|name, change| {
        *change != SymbolChange::Modified
            || !names
                .iter()
                .any(|other| other != name && other.starts_with(&format!("{name}.")))
    });
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn stage_prefixed_origins_resolve_to_paths() {
        use crate::snapshot::origin_path;
        assert_eq!(origin_path("src/api.ts").unwrap().as_str(), "src/api.ts");
        assert_eq!(
            origin_path("link:src/api.ts").unwrap().as_str(),
            "src/api.ts"
        );
        assert_eq!(origin_path("link:a/b:c.ts").unwrap().as_str(), "a/b:c.ts");
        assert!(origin_path("link:").is_none());
    }

    #[test]
    fn contract_queries_match_normalised_keys() {
        use knowell_store::ContractKind as K;
        let key = "POST /v1/subscriptions/{}/cancel";
        for q in [
            "post /v1/subscriptions/{id}/cancel",
            "post /v1/subscriptions/:id/cancel",
            "/v1/subscriptions/{}/cancel",
            "subscriptions",
        ] {
            assert!(contract_matches(K::Endpoint, key, q), "{q}");
        }
        assert!(!contract_matches(K::Endpoint, key, "post /v1/orders"));
        assert!(contract_matches(
            K::Topic,
            "subscription.cancelled",
            "subscription.cancelled"
        ));
    }

    use super::*;

    #[test]
    fn missing_call_coverage_is_not_an_absence_claim() {
        let message = call_coverage_message(&[]).unwrap();
        assert!(message.contains("not recorded"));
        assert!(!message.contains("no calls"));
    }

    #[test]
    fn call_coverage_uses_call_counters_and_keeps_static_analysis_partial() {
        let record = knowell_store::analysis::AnalysisCoverage {
            provider: "syntax".to_owned(),
            details: serde_json::json!({
                "call_sites": 2,
                "calls_written": 1,
                "calls_ambiguous": 0,
                "calls_unresolved": 1,
                "resolved": 100_000,
                "unresolved": 200_000,
                "calls_complete": false,
            }),
        };
        let message = call_coverage_message(&[record]).unwrap();
        assert!(message.contains("2 callee spans observed"));
        assert!(message.contains("1 unresolved call targets"));
        assert!(message.contains("whole-file observations"));
        assert!(!message.contains("100000") && !message.contains("200000"));
        assert!(message.contains("not a complete caller/callee"));
    }

    #[test]
    fn unavailable_or_missing_call_counters_cannot_report_complete_analysis() {
        for details in [
            serde_json::json!({"calls_complete": true, "source_unavailable": true, "calls_written": 0}),
            serde_json::json!({"calls_complete": true, "parse_unavailable": true, "call_sites": 0}),
            serde_json::json!({"calls_complete": true}),
            serde_json::json!({"calls_complete": true, "candidate_overflow": 1, "call_sites": 1}),
        ] {
            let record = knowell_store::analysis::AnalysisCoverage {
                provider: "scip".to_owned(),
                details,
            };
            let message = call_coverage_message(&[record]).unwrap();
            assert!(message.contains("coverage is partial"));
            assert!(!message.contains("no calls"));
        }
    }

    fn trace_fixture(candidate_edges: bool) -> (GraphCtx, Vec<NodeId>) {
        let project = Name::new("synthetic").unwrap();
        let symbol = |key: &str| NodeId::symbol(&project, key);
        let file = NodeId::file(&project, &RepoPath::new("root.rs").unwrap());
        let mut delta = knowell_graph::GraphDelta::new(project.clone(), 1).add_node(
            knowell_graph::Node::file(&project, &RepoPath::new("root.rs").unwrap()),
        );
        for key in ["start", "a", "aa", "b", "bb"] {
            delta = delta.add_node(knowell_graph::Node::symbol(&project, key, key));
        }
        let evidence = if candidate_edges {
            knowell_graph::EvidenceType::Heuristic
        } else {
            knowell_graph::EvidenceType::Syntactic
        };
        for (from, to, kind) in [
            (symbol("start"), symbol("a"), EdgeKind::Calls),
            (symbol("start"), symbol("aa"), EdgeKind::Calls),
            (file.clone(), symbol("b"), EdgeKind::Imports),
            (file.clone(), symbol("bb"), EdgeKind::Imports),
        ] {
            delta = delta.add_edge(
                &from,
                &to,
                knowell_graph::Edge::new(kind, evidence, knowell_graph::Resolution::Resolved),
            );
        }
        let mut graph = CodeGraph::new();
        graph.apply(delta).unwrap();
        (
            GraphCtx {
                graph: Arc::new(graph),
                views: BTreeMap::new(),
            },
            vec![symbol("start"), file],
        )
    }

    #[test]
    fn navigation_interleaves_starts_before_a_tight_global_node_cap() {
        let (ctx, starts) = trace_fixture(false);
        let spec = WalkSpec::new(2, Direction::Outgoing);
        let walks: Vec<_> = starts
            .iter()
            .map(|start| ctx.graph.walk_navigation(start, &spec, 12, 0).unwrap())
            .collect();
        let navigation = assemble_trace(&ctx, &starts, &walks, true, 4, 0);
        assert_eq!(
            navigation
                .nodes
                .iter()
                .map(|node| node.label.as_str())
                .collect::<Vec<_>>(),
            ["start", "root.rs", "a", "b"]
        );
        assert_eq!(navigation.edges.len(), 2);
        assert!(navigation.truncated);
        let legacy = assemble_trace(&ctx, &starts, &walks, false, 4, 0);
        assert_eq!(
            legacy
                .nodes
                .iter()
                .map(|node| node.label.as_str())
                .collect::<Vec<_>>(),
            ["start", "root.rs", "a", "aa"]
        );
    }

    #[test]
    fn trace_candidate_cap_is_global_across_starts_and_preserves_endpoints() {
        let (ctx, starts) = trace_fixture(true);
        let spec = WalkSpec::new(2, Direction::Outgoing);
        let walks: Vec<_> = starts
            .iter()
            .map(|start| ctx.graph.walk_navigation(start, &spec, 12, 8).unwrap())
            .collect();
        let output = assemble_trace(&ctx, &starts, &walks, true, 20, 1);
        assert_eq!(output.candidates_returned, 1);
        assert_eq!(output.candidates_omitted, 3);
        assert!(output.truncated);
        let nodes: BTreeSet<_> = output.nodes.iter().map(|node| node.node.as_str()).collect();
        assert!(
            output
                .edges
                .iter()
                .all(|edge| nodes.contains(edge.from.as_str()) && nodes.contains(edge.to.as_str()))
        );
    }

    #[test]
    fn candidates_clipped_by_final_trace_node_cap_are_counted() {
        let (ctx, starts) = trace_fixture(true);
        let spec = WalkSpec::new(2, Direction::Outgoing).with_node_budget(3);
        let walks: Vec<_> = starts
            .iter()
            .map(|start| ctx.graph.walk_navigation(start, &spec, 12, 8).unwrap())
            .collect();
        let output = assemble_trace(&ctx, &starts, &walks, true, 3, 8);
        assert_eq!(output.nodes.len(), 3);
        assert_eq!(output.candidates_returned, 1);
        assert_eq!(output.candidates_omitted, 3);
        assert!(output.truncated);
        let nodes: BTreeSet<_> = output.nodes.iter().map(|node| node.node.as_str()).collect();
        assert!(
            output
                .edges
                .iter()
                .all(|edge| nodes.contains(edge.from.as_str()) && nodes.contains(edge.to.as_str()))
        );
    }

    #[test]
    fn project_paths_require_a_component_boundary() {
        let root = RepoPath::new("packages/app").unwrap();
        assert_eq!(
            project_relative(
                &RepoPath::new("packages/app/src/a.ts").unwrap(),
                Some(&root)
            ),
            Some(RepoPath::new("src/a.ts").unwrap())
        );
        for path in [
            "packages/application/src/a.ts",
            "packages/app",
            "outside/src/a.ts",
        ] {
            assert!(project_relative(&RepoPath::new(path).unwrap(), Some(&root)).is_none());
        }
    }

    #[test]
    fn operational_git_errors_are_not_missing_refs() {
        assert!(is_missing_git_ref(&GitError::RefNotFound {
            target: "branch:gone".parse().unwrap()
        }));
        assert!(is_missing_git_ref(&GitError::CommitNotFound {
            id: "0".repeat(40)
        }));
        assert!(!is_missing_git_ref(&GitError::Git {
            operation: "reading allowed blob",
            message: "not found".to_owned(),
        }));
        assert!(!is_missing_git_ref(&GitError::NotARepository {
            path: std::path::PathBuf::from("synthetic"),
            message: "unreadable".to_owned(),
        }));
    }

    #[test]
    fn changed_symbols_prefers_members() {
        let path = RepoPath::new("svc.ts").unwrap();
        let old = "export class Svc {\n  a() { return 1; }\n  b() { return 2; }\n}\n";
        let new = "export class Svc {\n  a() { return 1; }\n  b() { return 3; }\n  c() { return 4; }\n}\n";
        let changes = changed_symbols(Some((&path, old)), Some((&path, new)));
        assert_eq!(changes.get("Svc.b"), Some(&SymbolChange::Modified));
        assert_eq!(changes.get("Svc.c"), Some(&SymbolChange::Added));
        assert!(!changes.contains_key("Svc"), "{changes:?}");
        assert!(!changes.contains_key("Svc.a"));
        let removed = changed_symbols(Some((&path, old)), None);
        assert_eq!(removed.get("Svc"), Some(&SymbolChange::Removed));
    }

    #[test]
    fn relations_map_by_contract_kind() {
        assert_eq!(
            relation_of(EdgeKind::Consumes, Some(GContractKind::Endpoint)),
            Some(RelationKind::HttpCall)
        );
        assert_eq!(
            relation_of(EdgeKind::Consumes, Some(GContractKind::Topic)),
            Some(RelationKind::Consumes)
        );
        assert_eq!(
            relation_of(EdgeKind::Exposes, Some(GContractKind::Rpc)),
            Some(RelationKind::RpcServes)
        );
        assert_eq!(relation_of(EdgeKind::Defines, None), None);
        assert_eq!(
            relation_of(EdgeKind::Imports, None),
            Some(RelationKind::Imports)
        );
    }
}
