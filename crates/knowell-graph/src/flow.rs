//! Cross-project flow tracing.
//!
//! Edges are stored in the direction "a *kind* b" (a client *consumes* an
//! endpoint, a controller *exposes* it), but data and control flow do not
//! always follow that direction: a request flows client to endpoint to
//! controller, an event flows publisher to topic to subscriber. Tracing
//! therefore gives every edge kind a *flow orientation*:
//!
//! | Edge kind | Flow runs |
//! |---|---|
//! | `Calls`, `References`, `Produces`, `Writes` | `from` to `to` |
//! | `Consumes` onto a topic | `to` to `from` (topic to subscriber) |
//! | `Consumes` onto anything else (endpoint, RPC, ...) | `from` to `to` (request) |
//! | `Exposes`, `Reads` | `to` to `from` (contract to provider, table to reader) |
//!
//! Other kinds (`Defines`, `Contains`, `Tests`, `Documents`, ...) are not flow.
//!
//! # Ranking
//!
//! Paths are ordered by this key, compared left to right:
//!
//! 1. weakest evidence on the path (a path is as strong as its weakest edge),
//!    stronger first;
//! 2. number of ambiguous or unresolved edges, fewer first;
//! 3. length in edges, shorter first (for open-ended traces from a start node
//!    without a target, longer first, so the most complete flows lead);
//! 4. sum of evidence ranks over all edges, lower first;
//! 5. the sequence of edge keys, lexicographically (pure tie-break).

use std::collections::{BTreeMap, BTreeSet};

use knowell_core::Name;

use crate::error::GraphError;
use crate::graph::{CodeGraph, Neighbor};
use crate::model::{
    ContractKind, Direction, EdgeFilter, EdgeKey, EdgeKind, EvidenceType, Hop, NodeId, Resolution,
    flagged_count, weakest_evidence,
};

/// Deepest trace accepted by [`CodeGraph::trace_flow`].
pub const MAX_TRACE_DEPTH: u8 = 10;

/// Which way an open-ended trace follows flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FlowDirection {
    /// Follow flow forward: what does this feed?
    Downstream,
    /// Follow flow backward: what feeds this?
    Upstream,
}

/// Parameters of [`CodeGraph::trace_flow`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowSpec {
    /// Where the trace starts.
    pub from: NodeId,
    /// Where it must end; `None` traces every maximal flow from `from`.
    pub to: Option<NodeId>,
    /// Flow direction, relative to `from`.
    pub direction: FlowDirection,
    /// Maximum number of edges, `1..=`[`MAX_TRACE_DEPTH`].
    pub max_depth: u8,
    /// Number of best paths to return (at least 1).
    pub max_paths: usize,
    /// Which edges may be used.
    pub filter: EdgeFilter,
    /// Maximum number of edge expansions before the search stops (at least 1).
    pub expansion_budget: usize,
}

impl FlowSpec {
    /// Paths from `from` to `to` following flow, depth 8, 5 paths.
    pub fn between(from: NodeId, to: NodeId) -> Self {
        Self {
            from,
            to: Some(to),
            direction: FlowDirection::Downstream,
            max_depth: 8,
            max_paths: 5,
            filter: EdgeFilter::any(),
            expansion_budget: 50_000,
        }
    }

    /// Maximal flows starting at `from` in `direction`, depth 8, 5 paths.
    pub fn open(from: NodeId, direction: FlowDirection) -> Self {
        Self {
            from,
            to: None,
            direction,
            max_depth: 8,
            max_paths: 5,
            filter: EdgeFilter::any(),
            expansion_budget: 50_000,
        }
    }

    /// Replaces the depth limit.
    #[must_use]
    pub fn with_max_depth(mut self, max_depth: u8) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// Replaces the number of paths returned.
    #[must_use]
    pub fn with_max_paths(mut self, max_paths: usize) -> Self {
        self.max_paths = max_paths;
        self
    }

    /// Replaces the edge filter.
    #[must_use]
    pub fn with_filter(mut self, filter: EdgeFilter) -> Self {
        self.filter = filter;
        self
    }

    /// Replaces the expansion budget.
    #[must_use]
    pub fn with_expansion_budget(mut self, budget: usize) -> Self {
        self.expansion_budget = budget;
        self
    }

    /// Follows flow backward from `from` (for `between`, finds paths that
    /// reach `to` by walking against the flow).
    #[must_use]
    pub fn upstream(mut self) -> Self {
        self.direction = FlowDirection::Upstream;
        self
    }
}

/// One traced path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowPath {
    /// Nodes in path order, starting with the trace's `from`.
    pub nodes: Vec<NodeId>,
    /// The edges used, in path order. `hop.forward` tells whether the step
    /// ran along the stored edge direction.
    pub hops: Vec<Hop>,
    /// Weakest evidence on the path.
    pub weakest_evidence: EvidenceType,
    /// Number of ambiguous or unresolved edges. Non-zero means the path is
    /// *flagged*: it may not exist at run time.
    pub flagged_hops: usize,
    /// Projects the path passes through, in first-visit order.
    pub projects: Vec<Name>,
    /// Contract nodes the path crosses, in path order.
    pub contracts: Vec<NodeId>,
}

impl FlowPath {
    /// Whether any edge is ambiguous or unresolved.
    pub fn is_flagged(&self) -> bool {
        self.flagged_hops > 0
    }

    /// Number of edges.
    pub fn len(&self) -> usize {
        self.hops.len()
    }

    /// Whether the path has no edges (never true for returned paths).
    pub fn is_empty(&self) -> bool {
        self.hops.is_empty()
    }

    /// Whether the path touches more than one project.
    pub fn crosses_projects(&self) -> bool {
        self.projects.len() > 1
    }
}

/// Result of [`CodeGraph::trace_flow`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowTrace {
    /// Best paths first (see the module docs for the ranking).
    pub paths: Vec<FlowPath>,
    /// `true` when the expansion budget stopped the search; better paths may
    /// exist than the ones returned.
    pub truncated: bool,
    /// Number of edge expansions performed.
    pub expansions: usize,
}

/// Flow orientation of an edge: `Some(true)` when flow runs from the edge's
/// source to its target, `Some(false)` when against, `None` when the kind is
/// not a flow relation.
fn orientation(kind: EdgeKind, target_contract: Option<ContractKind>) -> Option<bool> {
    match kind {
        EdgeKind::Calls | EdgeKind::References | EdgeKind::Produces | EdgeKind::Writes => {
            Some(true)
        }
        EdgeKind::Consumes => Some(target_contract != Some(ContractKind::Topic)),
        EdgeKind::Exposes | EdgeKind::Reads => Some(false),
        _ => None,
    }
}

/// Sort key of a path; derived `Ord` is the documented ranking.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PathKey {
    weakest: u8,
    flagged: usize,
    len: i64,
    rank_sum: usize,
    edges: Vec<EdgeKey>,
}

struct Candidate {
    key: PathKey,
    nodes: Vec<NodeId>,
    hops: Vec<Hop>,
}

struct Search<'a> {
    graph: &'a CodeGraph,
    spec: &'a FlowSpec,
    results: Vec<Candidate>,
    explored: usize,
    truncated: bool,
    on_path: BTreeSet<NodeId>,
    nodes: Vec<NodeId>,
    hops: Vec<Hop>,
}

impl<'a> Search<'a> {
    /// Candidate next steps from `node` along flow, best edge per next node,
    /// ordered best first.
    fn steps(&self, node: &NodeId) -> Result<Vec<Neighbor<'a>>, GraphError> {
        let graph = self.graph;
        let downstream = self.spec.direction == FlowDirection::Downstream;
        let mut best: BTreeMap<&'a NodeId, Neighbor<'a>> = BTreeMap::new();
        for nb in graph.neighbors(node, Direction::Both, &self.spec.filter)? {
            let target_kind = graph.node(nb.to).and_then(|n| n.contract_kind());
            let Some(flows_forward) = orientation(nb.edge.kind, target_kind) else {
                continue;
            };
            // `nb.forward` is true when `node` is the edge source.
            let along_flow = nb.forward == flows_forward;
            if along_flow != downstream {
                continue;
            }
            let replace = match best.get(&nb.node.id) {
                Some(old) => step_rank(&nb) < step_rank(old),
                None => true,
            };
            if replace {
                best.insert(&nb.node.id, nb);
            }
        }
        let mut steps: Vec<Neighbor<'a>> = best.into_values().collect();
        steps.sort_by(|a, b| (step_rank(a), &a.node.id).cmp(&(step_rank(b), &b.node.id)));
        Ok(steps)
    }

    fn kth_prefix(&self) -> Option<(u8, usize)> {
        if self.results.len() < self.spec.max_paths {
            return None;
        }
        self.results.last().map(|c| (c.key.weakest, c.key.flagged))
    }

    fn record(&mut self) {
        let target_mode = self.spec.to.is_some();
        let len = i64::try_from(self.hops.len()).unwrap_or(i64::MAX);
        let key = PathKey {
            weakest: weakest_evidence(&self.hops).map_or(0, EvidenceType::rank),
            flagged: flagged_count(&self.hops),
            len: if target_mode { len } else { -len },
            rank_sum: self
                .hops
                .iter()
                .map(|h| usize::from(h.evidence.rank()))
                .sum(),
            edges: self.hops.iter().map(|h| h.edge.clone()).collect(),
        };
        let candidate = Candidate {
            key,
            nodes: self.nodes.clone(),
            hops: self.hops.clone(),
        };
        let pos = self
            .results
            .partition_point(|existing| existing.key < candidate.key);
        self.results.insert(pos, candidate);
        self.results.truncate(self.spec.max_paths);
    }

    fn dfs(&mut self, node: &NodeId) -> Result<(), GraphError> {
        let steps = self.steps(node)?;
        let mut pursued = false;
        for step in steps {
            if self.truncated {
                return Ok(());
            }
            if self.explored >= self.spec.expansion_budget {
                self.truncated = true;
                return Ok(());
            }
            self.explored += 1;
            let next = step.node.id.clone();
            if self.on_path.contains(&next) {
                continue;
            }
            pursued = true;
            self.hops.push(step.hop());
            self.nodes.push(next.clone());

            let prefix = (
                weakest_evidence(&self.hops).map_or(0, EvidenceType::rank),
                flagged_count(&self.hops),
            );
            let pruned = self.kth_prefix().is_some_and(|kth| prefix > kth);
            if !pruned {
                if self.spec.to.as_ref() == Some(&next) {
                    self.record();
                } else if self.hops.len() >= usize::from(self.spec.max_depth) {
                    if self.spec.to.is_none() {
                        self.record();
                    }
                } else {
                    self.on_path.insert(next.clone());
                    self.dfs(&next)?;
                    self.on_path.remove(&next);
                }
            }
            self.nodes.pop();
            self.hops.pop();
        }
        if !pursued && self.spec.to.is_none() && !self.hops.is_empty() {
            self.record();
        }
        Ok(())
    }
}

fn step_rank(nb: &Neighbor<'_>) -> (u8, Resolution, EdgeKind, bool) {
    (
        nb.edge.evidence.rank(),
        nb.edge.resolution,
        nb.edge.kind,
        !nb.forward,
    )
}

impl CodeGraph {
    /// Finds the best flow paths, possibly crossing contract nodes, from
    /// `spec.from` either to `spec.to` or to every flow end point.
    ///
    /// Paths are simple (no node repeats), ranked as described in the module
    /// docs, and deterministic. Ambiguous and unresolved edges are allowed
    /// unless the filter excludes them; such paths are flagged
    /// ([`FlowPath::is_flagged`]) and rank below fully resolved ones of the
    /// same evidence strength.
    pub fn trace_flow(&self, spec: &FlowSpec) -> Result<FlowTrace, GraphError> {
        if spec.max_depth == 0 || spec.max_depth > MAX_TRACE_DEPTH {
            return Err(GraphError::InvalidDepth {
                given: spec.max_depth,
                max: MAX_TRACE_DEPTH,
            });
        }
        if spec.max_paths == 0 {
            return Err(GraphError::InvalidLimit("path count"));
        }
        if spec.expansion_budget == 0 {
            return Err(GraphError::InvalidLimit("expansion budget"));
        }
        if self.node(&spec.from).is_none() {
            return Err(GraphError::UnknownNode(spec.from.clone()));
        }
        if let Some(to) = &spec.to
            && self.node(to).is_none()
        {
            return Err(GraphError::UnknownNode(to.clone()));
        }

        let mut search = Search {
            graph: self,
            spec,
            results: Vec::new(),
            explored: 0,
            truncated: false,
            on_path: BTreeSet::from([spec.from.clone()]),
            nodes: vec![spec.from.clone()],
            hops: Vec::new(),
        };
        if spec.to.as_ref() != Some(&spec.from) {
            search.dfs(&spec.from)?;
        }

        let expansions = search.explored;
        let truncated = search.truncated;
        let paths = search
            .results
            .into_iter()
            .map(|cand| self.finish_path(cand))
            .collect();
        Ok(FlowTrace {
            paths,
            truncated,
            expansions,
        })
    }

    fn finish_path(&self, cand: Candidate) -> FlowPath {
        let mut projects: Vec<Name> = Vec::new();
        let mut contracts = Vec::new();
        for id in &cand.nodes {
            let Some(node) = self.node(id) else { continue };
            if let Some(project) = &node.project
                && !projects.contains(project)
            {
                projects.push(project.clone());
            }
            if node.contract_kind().is_some() {
                contracts.push(id.clone());
            }
        }
        FlowPath {
            weakest_evidence: weakest_evidence(&cand.hops)
                .unwrap_or(EvidenceType::SemanticResolved),
            flagged_hops: flagged_count(&cand.hops),
            nodes: cand.nodes,
            hops: cand.hops,
            projects,
            contracts,
        }
    }
}
