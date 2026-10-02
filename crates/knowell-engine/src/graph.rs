//! The in-memory code graph (`knowell-graph::CodeGraph`) of a pinned
//! manifest, built from the stored edges of every pinned generation.
//!
//! Nodes: one per file and per parsed symbol (keyed by the store's symbol
//! key), plus contract nodes and placeholders for unresolved import names.
//! Edges: `defines` (file → symbol) and `contains` (container → member) as
//! the indexer stored them, `imports` (file → file or placeholder) from the
//! store, and any other file-level edge a relation stage wrote (contracts).
//! One delta per project at its pinned generation, so the graph's
//! generation fence matches the store's.
//!
//! Graphs are cached per set of pins and dropped when a view activates a
//! newer generation (the next query pins the new generation, so its graph
//! is built from the new edges).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use knowell_core::{Name, RepoPath};
use knowell_graph::{
    ATTR_UNRESOLVED, CodeGraph, ContractKind, Edge, EdgeKind, EvidenceRef, EvidenceType,
    GraphDelta, Node, NodeId, Resolution,
};
use knowell_store::graph::NodeRef;
use knowell_store::views::GenerationPin;

use crate::snapshot::{ImportTarget, Snapshot};

/// Graphs by their sorted pins, at most [`MAX_GRAPHS`].
#[derive(Debug, Default)]
pub(crate) struct GraphCache {
    entries: Mutex<Vec<(Vec<GenerationPin>, Arc<CodeGraph>)>>,
}

/// Graphs kept in memory.
const MAX_GRAPHS: usize = 4;

impl GraphCache {
    pub(crate) fn get(&self, pins: &[GenerationPin]) -> Option<Arc<CodeGraph>> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries
            .iter()
            .find(|(p, _)| p.as_slice() == pins)
            .map(|(_, g)| Arc::clone(g))
    }

    pub(crate) fn put(&self, pins: Vec<GenerationPin>, graph: Arc<CodeGraph>) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|(p, _)| *p != pins);
        entries.push((pins, graph));
        while entries.len() > MAX_GRAPHS {
            entries.remove(0);
        }
    }

    /// Drops every graph that pins `view` below `generation`.
    pub(crate) fn retire_older(&self, view: knowell_store::ViewId, generation: i64) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|(pins, _)| {
            !pins
                .iter()
                .any(|p| p.view == view && p.generation < generation)
        });
    }

    pub(crate) fn clear(&self) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

/// Store evidence type as the graph's.
pub(crate) fn graph_evidence(e: knowell_store::EvidenceType) -> EvidenceType {
    match e {
        knowell_store::EvidenceType::SemanticResolved => EvidenceType::SemanticResolved,
        knowell_store::EvidenceType::ContractDerived => EvidenceType::ContractDerived,
        knowell_store::EvidenceType::Syntactic => EvidenceType::Syntactic,
        knowell_store::EvidenceType::Heuristic => EvidenceType::Heuristic,
        knowell_store::EvidenceType::ModelSuggestion => EvidenceType::ModelSuggestion,
        knowell_store::EvidenceType::RuntimeObserved => EvidenceType::RuntimeObserved,
    }
}

fn graph_resolution(r: knowell_store::Resolution) -> Resolution {
    match r {
        knowell_store::Resolution::Resolved => Resolution::Resolved,
        knowell_store::Resolution::Ambiguous => Resolution::Ambiguous,
        knowell_store::Resolution::Unresolved => Resolution::Unresolved,
    }
}

fn contract_kind(kind: knowell_store::ContractKind) -> ContractKind {
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

/// The graph kind of a stored edge kind (`calls`, `imports`, …).
fn edge_kind(kind: &str) -> Option<EdgeKind> {
    Some(match kind {
        "calls" => EdgeKind::Calls,
        "references" => EdgeKind::References,
        "implements" => EdgeKind::Implements,
        "imports" => EdgeKind::Imports,
        "defines" => EdgeKind::Defines,
        "contains" => EdgeKind::Contains,
        "tests" => EdgeKind::Tests,
        "documents" => EdgeKind::Documents,
        "produces" => EdgeKind::Produces,
        "consumes" => EdgeKind::Consumes,
        "reads" => EdgeKind::Reads,
        "writes" => EdgeKind::Writes,
        "exposes" => EdgeKind::Exposes,
        "depends_on" => EdgeKind::DependsOn,
        "decides" => EdgeKind::Decides,
        _ => return None,
    })
}

/// The graph relation of a contract participation (see the knowell-graph
/// conventions: clients consume, servers expose, publishers produce, …).
fn contract_edge(role: knowell_store::ContractRole, kind: ContractKind) -> EdgeKind {
    use knowell_store::ContractRole::{Consumer, Producer};
    match (role, kind) {
        (Producer, ContractKind::Endpoint | ContractKind::Rpc) => EdgeKind::Exposes,
        (Producer, ContractKind::Topic) => EdgeKind::Produces,
        (Producer, ContractKind::Table) => EdgeKind::Writes,
        (Producer, _) => EdgeKind::Defines,
        (Consumer, ContractKind::Endpoint | ContractKind::Rpc | ContractKind::Topic) => {
            EdgeKind::Consumes
        }
        (Consumer, ContractKind::Package | ContractKind::Infra) => EdgeKind::DependsOn,
        (Consumer, _) => EdgeKind::Reads,
    }
}

/// Lines `[a, b]` of an evidence JSON object.
fn evidence_json_lines(evidence: &serde_json::Value) -> Option<knowell_core::LineRange> {
    let lines = evidence.get("lines")?.as_array()?;
    let start = u32::try_from(lines.first()?.as_u64()?).ok()?;
    let end = u32::try_from(lines.get(1)?.as_u64()?).ok()?;
    knowell_core::LineRange::new(start, end).ok()
}

/// The node id of a file.
pub(crate) fn file_node(project: &Name, path: &RepoPath) -> NodeId {
    NodeId::file(project, path)
}

/// The node id of a parsed symbol (by store key).
pub(crate) fn symbol_node(project: &Name, key: &str) -> NodeId {
    NodeId::symbol(project, key)
}

/// The placeholder node of an unresolved import name.
fn name_node(project: &Name, name: &str) -> Result<NodeId, knowell_graph::GraphError> {
    NodeId::new(format!("name:{project}:{name}"))
}

/// Nodes and edges of one project's delta, deduplicated by id and key
/// (a delta must not repeat either).
#[derive(Default)]
struct DeltaParts {
    nodes: BTreeMap<NodeId, Node>,
    edges: BTreeMap<(NodeId, NodeId, EdgeKind), Edge>,
}

impl DeltaParts {
    fn node(&mut self, node: Node) {
        self.nodes.entry(node.id.clone()).or_insert(node);
    }

    fn edge(&mut self, from: NodeId, to: NodeId, edge: Edge) {
        self.edges.entry((from, to, edge.kind)).or_insert(edge);
    }

    fn placeholder(
        &mut self,
        project: &Name,
        name: &str,
    ) -> Result<NodeId, knowell_graph::GraphError> {
        let id = name_node(project, name)?;
        if !self.nodes.contains_key(&id) {
            let node = Node {
                id: id.clone(),
                ..Node::symbol(project, &format!("name:{name}"), name)
            }
            .with_attr(ATTR_UNRESOLVED, "true");
            self.nodes.insert(id.clone(), node);
        }
        Ok(id)
    }

    fn into_delta(self, project: Name, generation: u64) -> GraphDelta {
        let mut delta = GraphDelta::new(project, generation);
        for node in self.nodes.into_values() {
            delta = delta.add_node(node);
        }
        for ((from, to, _), edge) in self.edges {
            delta = delta.add_edge(&from, &to, edge);
        }
        delta
    }
}

/// Builds the graph of `snapshots` (one per project, at its pinned
/// generation).
pub(crate) fn build(snapshots: &[Arc<Snapshot>]) -> Result<CodeGraph, knowell_graph::GraphError> {
    let mut graph = CodeGraph::new();
    for snapshot in snapshots {
        let project = &snapshot.project;
        let generation = u64::try_from(snapshot.pin.generation).unwrap_or(1).max(1);
        let mut parts = DeltaParts::default();
        let evidence = |path: &RepoPath, range| {
            snapshot.file(path).map(|f| EvidenceRef {
                project: project.clone(),
                path: path.clone(),
                range,
                content_hash: f.content_hash,
            })
        };
        for (path, file) in &snapshot.files {
            parts.node(Node::file(project, path).with_source(EvidenceRef {
                project: project.clone(),
                path: path.clone(),
                range: None,
                content_hash: file.content_hash,
            }));
        }
        let mut by_store_id = BTreeMap::new();
        for symbol in &snapshot.symbols {
            let id = symbol_node(project, &symbol.key);
            let mut node = Node::symbol(project, &symbol.key, &symbol.local);
            if let Some(source) = evidence(&symbol.path, Some(symbol.lines)) {
                node = node.with_source(source);
            }
            parts.node(node);
            let mut edge = Edge::new(
                EdgeKind::Defines,
                EvidenceType::Syntactic,
                Resolution::Resolved,
            );
            if let Some(source) = evidence(&symbol.path, Some(symbol.lines)) {
                edge = edge.with_ref(source);
            }
            parts.edge(file_node(project, &symbol.path), id.clone(), edge);
            if let Some(store_id) = symbol.store_id {
                by_store_id.insert(store_id, id);
            }
        }
        for symbol in &snapshot.symbols {
            let Some(parent) = symbol.parent.and_then(|p| snapshot.symbols.get(p)) else {
                continue;
            };
            if parent.key == symbol.key {
                continue;
            }
            let mut edge = Edge::new(
                EdgeKind::Contains,
                EvidenceType::Syntactic,
                Resolution::Resolved,
            );
            if let Some(source) = evidence(&symbol.path, Some(symbol.lines)) {
                edge = edge.with_ref(source);
            }
            parts.edge(
                symbol_node(project, &parent.key),
                symbol_node(project, &symbol.key),
                edge,
            );
        }
        for import in &snapshot.imports {
            let to = match &import.to {
                ImportTarget::File(path) => {
                    if snapshot.file(path).is_none() || *path == import.from {
                        continue;
                    }
                    file_node(project, path)
                }
                ImportTarget::Name(name) => parts.placeholder(project, name)?,
            };
            let mut edge = Edge::new(
                EdgeKind::Imports,
                graph_evidence(import.evidence),
                graph_resolution(import.resolution),
            );
            if let Some(source) = evidence(&import.from, import.lines) {
                edge = edge.with_ref(source);
            }
            parts.edge(file_node(project, &import.from), to, edge);
        }
        for other in &snapshot.other_edges {
            let Some(kind) = edge_kind(&other.kind) else {
                continue;
            };
            let from = match &other.from {
                NodeRef::File { project: p, path } if *p == snapshot.project_id => {
                    if snapshot.file(path).is_none() {
                        continue;
                    }
                    file_node(project, path)
                }
                NodeRef::Symbol(id) => match by_store_id.get(id) {
                    Some(node) => node.clone(),
                    None => continue,
                },
                _ => continue,
            };
            let to = match &other.to {
                NodeRef::File { project: p, path } if *p == snapshot.project_id => {
                    if snapshot.file(path).is_none() {
                        continue;
                    }
                    file_node(project, path)
                }
                NodeRef::Symbol(id) => match by_store_id.get(id) {
                    Some(node) => node.clone(),
                    None => continue,
                },
                NodeRef::Contract { kind, key, .. } => {
                    let kind = contract_kind(*kind);
                    let node = Node::contract(kind, key);
                    let id = node.id.clone();
                    parts.node(node);
                    id
                }
                NodeRef::Name { name, .. } => parts.placeholder(project, name)?,
                _ => continue,
            };
            let mut edge = Edge::new(
                kind,
                graph_evidence(other.evidence),
                graph_resolution(other.resolution),
            );
            if let Some(source) = evidence(&other.origin, other.lines) {
                edge = edge.with_ref(source);
            }
            parts.edge(from, to, edge);
        }
        for party in &snapshot.contracts {
            let contract = &party.contract;
            let kind = contract_kind(contract.kind);
            let node = Node::contract(kind, &contract.key);
            let to = node.id.clone();
            let Some(origin) = crate::snapshot::origin_path(&contract.origin) else {
                continue;
            };
            let from = match contract.symbol.and_then(|id| by_store_id.get(&id)) {
                Some(symbol) => symbol.clone(),
                None if snapshot.file(&origin).is_some() => file_node(project, &origin),
                None => continue,
            };
            parts.node(node);
            let relation = contract_edge(contract.role, kind);
            let mut edge = Edge::new(
                relation,
                graph_evidence(contract.evidence_type),
                Resolution::Resolved,
            );
            if let Some(source) = evidence(&origin, evidence_json_lines(&contract.evidence)) {
                edge = edge.with_ref(source);
            }
            parts.edge(from, to, edge);
        }
        graph.apply(parts.into_delta(project.clone(), generation))?;
    }
    Ok(graph)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use knowell_core::{ContentHash, LineRange};
    use knowell_graph::{ImpactSpec, WalkSpec};
    use knowell_parse::SymbolKind;
    use knowell_store::{ProjectId, SymbolId, ViewId};
    use uuid::Uuid;

    use super::*;
    use crate::snapshot::{FileInfo, ImportEdge, OtherEdge, SnapshotParts, SymbolEntry};

    fn path(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn file() -> FileInfo {
        FileInfo {
            content_hash: ContentHash::of(b"x"),
            language: Some("typescript".into()),
            size_bytes: 10,
            line_count: 20,
            has_text: true,
        }
    }

    fn symbol(
        file: &str,
        local: &str,
        a: u32,
        b: u32,
        id: u128,
        parent: Option<usize>,
    ) -> SymbolEntry {
        SymbolEntry {
            key: format!("{file}#{local}"),
            local: local.into(),
            name: local.rsplit('.').next().unwrap().into(),
            kind: SymbolKind::Method,
            path: path(file),
            lines: LineRange::new(a, b).unwrap(),
            name_line: a,
            signature: String::new(),
            doc: None,
            parent,
            store_id: Some(SymbolId(Uuid::from_u128(id))),
        }
    }

    /// `svc.ts` defines `Svc` and `Svc.cancel` (twice: an overload repeats
    /// the key); `ctl.ts` imports it and its `Ctl.run` references
    /// `Svc.cancel`; `ctl.test.ts` imports `ctl.ts`.
    fn snapshot() -> Arc<Snapshot> {
        let project_id = ProjectId(Uuid::from_u128(1));
        let mut files = BTreeMap::new();
        for p in ["svc.ts", "ctl.ts", "ctl.test.ts"] {
            files.insert(path(p), file());
        }
        let symbols = vec![
            symbol("svc.ts", "Svc", 1, 10, 10, None),
            symbol("svc.ts", "Svc.cancel", 2, 4, 11, Some(0)),
            symbol("svc.ts", "Svc.cancel", 5, 9, 11, Some(0)),
            symbol("ctl.ts", "Ctl", 1, 10, 20, None),
            symbol("ctl.ts", "Ctl.run", 2, 9, 21, Some(3)),
        ];
        let import = |from: &str, to: &str| ImportEdge {
            from: path(from),
            to: ImportTarget::File(path(to)),
            evidence: knowell_store::EvidenceType::Syntactic,
            resolution: knowell_store::Resolution::Resolved,
            lines: LineRange::new(1, 1).ok(),
        };
        Arc::new(Snapshot::assemble(SnapshotParts {
            project: Name::new("api").unwrap(),
            project_id,
            pin: GenerationPin {
                view: ViewId(Uuid::from_u128(2)),
                generation: 3,
            },
            files,
            symbols,
            chunks: BTreeMap::new(),
            imports: vec![
                import("ctl.ts", "svc.ts"),
                import("ctl.test.ts", "ctl.ts"),
                ImportEdge {
                    from: path("ctl.ts"),
                    to: ImportTarget::Name("@nestjs/common".into()),
                    evidence: knowell_store::EvidenceType::Syntactic,
                    resolution: knowell_store::Resolution::Unresolved,
                    lines: None,
                },
            ],
            other_edges: vec![OtherEdge {
                origin: path("ctl.ts"),
                from: NodeRef::Symbol(SymbolId(Uuid::from_u128(21))),
                to: NodeRef::Symbol(SymbolId(Uuid::from_u128(11))),
                kind: "references".into(),
                evidence: knowell_store::EvidenceType::Heuristic,
                resolution: knowell_store::Resolution::Resolved,
                lines: LineRange::new(6, 6).ok(),
            }],
            contracts: Vec::new(),
        }))
    }

    #[test]
    fn builds_structure_imports_and_references() {
        let snapshot = snapshot();
        let graph = build(&[Arc::clone(&snapshot)]).unwrap();
        let project = Name::new("api").unwrap();
        let cancel = symbol_node(&project, "svc.ts#Svc.cancel");
        assert!(graph.node(&cancel).is_some());
        assert!(
            graph
                .node(&NodeId::new("name:api:@nestjs/common").unwrap())
                .is_some()
        );
        // A change of Svc.cancel reaches Ctl.run (reference) and, through
        // the file, ctl.ts and its test (imports).
        let report = graph
            .impact(&ImpactSpec::new(vec![
                cancel.clone(),
                file_node(&project, &path("svc.ts")),
            ]))
            .unwrap();
        let reached: Vec<String> = report
            .affected
            .iter()
            .chain(report.tests.iter())
            .map(|n| n.node.to_string())
            .collect();
        assert!(
            reached.contains(&"symbol:api:ctl.ts#Ctl.run".to_owned()),
            "{reached:?}"
        );
        assert!(
            reached.contains(&"file:api:ctl.ts".to_owned()),
            "{reached:?}"
        );
        assert!(
            reached.contains(&"file:api:ctl.test.ts".to_owned()),
            "{reached:?}"
        );
        let walk = graph
            .walk(
                &cancel,
                &WalkSpec::new(1, knowell_graph::Direction::Incoming),
            )
            .unwrap();
        assert!(
            walk.visits
                .iter()
                .any(|v| v.node.as_str() == "symbol:api:ctl.ts#Ctl.run")
        );
        // Snapshot lookups agree with the graph.
        let id = SymbolId(Uuid::from_u128(11));
        assert_eq!(snapshot.uses_of(id).count(), 1);
        assert_eq!(snapshot.used_by(SymbolId(Uuid::from_u128(21))).count(), 1);
        assert_eq!(snapshot.imports_of(&path("svc.ts")).count(), 1);
    }

    #[test]
    fn cache_retires_older_generations() {
        let cache = GraphCache::default();
        let pin = |g| GenerationPin {
            view: ViewId(Uuid::from_u128(2)),
            generation: g,
        };
        let graph = Arc::new(CodeGraph::new());
        cache.put(vec![pin(3)], Arc::clone(&graph));
        cache.put(vec![pin(4)], graph);
        assert!(cache.get(&[pin(3)]).is_some());
        cache.retire_older(ViewId(Uuid::from_u128(2)), 4);
        assert!(cache.get(&[pin(3)]).is_none());
        assert!(cache.get(&[pin(4)]).is_some());
    }
}
