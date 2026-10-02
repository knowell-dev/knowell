//! Impact analysis: what is affected when some nodes change.
//!
//! "Affected" follows the direction in which a change *propagates*, which is
//! mostly against edge direction (a caller is affected when its callee
//! changes) and, for the provider side of a contract, along it (a controller
//! that changes alters the endpoint it exposes, and so its clients):
//!
//! - against these edges (the edge source is affected when the target changes):
//!   `Calls`, `References`, `Implements`, `Imports`, `DependsOn`, `Consumes`,
//!   `Reads`, `Writes`, `Produces`, `Exposes`, `Tests`, `Documents`, `Decides`;
//! - along `Exposes`, `Produces` and `Writes` (provider to contract). A node
//!   reached that way does not propagate back over those three kinds, so a
//!   change in one controller does not "impact" another controller exposing
//!   the same endpoint, only the endpoint's consumers.
//!
//! Tests, docs and decisions are reported but never expanded further.
//!
//! # Risk
//!
//! Each affected node carries the *best* path that reaches it (weakest
//! evidence, then fewest flagged edges, then fewest hops, then edge keys) and
//! a [`RiskLevel`] derived from that path:
//!
//! - `Unknown`: the path contains an unresolved edge;
//! - `Low`: weakest evidence is heuristic or model-suggested, or an edge is ambiguous;
//! - `Medium`: weakest evidence is syntactic, or strong evidence over three or more hops;
//! - `High`: strong evidence (semantic, runtime or contract-derived), all
//!   edges resolved, at most two hops.

use std::collections::{BTreeMap, BTreeSet};

use knowell_core::Name;

use crate::error::GraphError;
use crate::graph::CodeGraph;
use crate::model::{
    Direction, EdgeFilter, EdgeKey, EdgeKind, EvidenceType, Hop, NodeId, NodeKind, Resolution,
    flagged_count, weakest_evidence,
};

/// Deepest impact search accepted by [`CodeGraph::impact`].
pub const MAX_IMPACT_DEPTH: u8 = 8;

/// Edges over which a change reaches the edge *source*.
const INCOMING: [EdgeKind; 13] = [
    EdgeKind::Calls,
    EdgeKind::References,
    EdgeKind::Implements,
    EdgeKind::Imports,
    EdgeKind::DependsOn,
    EdgeKind::Consumes,
    EdgeKind::Reads,
    EdgeKind::Writes,
    EdgeKind::Produces,
    EdgeKind::Exposes,
    EdgeKind::Tests,
    EdgeKind::Documents,
    EdgeKind::Decides,
];

/// Provider-side kinds: a change also reaches the edge *target* (the contract).
const PROVIDER: [EdgeKind; 3] = [EdgeKind::Exposes, EdgeKind::Produces, EdgeKind::Writes];

/// Parameters of [`CodeGraph::impact`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactSpec {
    /// Changed nodes (at least one).
    pub targets: Vec<NodeId>,
    /// Maximum number of edges from a target, `1..=`[`MAX_IMPACT_DEPTH`].
    pub max_depth: u8,
    /// Which edges may carry impact.
    pub filter: EdgeFilter,
    /// Maximum number of affected nodes (at least 1).
    pub node_budget: usize,
}

impl ImpactSpec {
    /// Impact of changing `targets`: depth 5, any edge, budget of 1000 nodes.
    pub fn new(targets: Vec<NodeId>) -> Self {
        Self {
            targets,
            max_depth: 5,
            filter: EdgeFilter::any(),
            node_budget: 1000,
        }
    }

    /// Replaces the depth limit.
    #[must_use]
    pub fn with_max_depth(mut self, max_depth: u8) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// Replaces the edge filter.
    #[must_use]
    pub fn with_filter(mut self, filter: EdgeFilter) -> Self {
        self.filter = filter;
        self
    }

    /// Replaces the node budget.
    #[must_use]
    pub fn with_node_budget(mut self, node_budget: usize) -> Self {
        self.node_budget = node_budget;
        self
    }
}

/// How likely a reached node is to really be affected. Declaration order is
/// strongest first, so `High < Medium < Low < Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RiskLevel {
    /// Strong, resolved, short path.
    High,
    /// Syntactic evidence or a long strong path.
    Medium,
    /// Heuristic, model-suggested or ambiguous.
    Low,
    /// The path depends on an unresolved edge.
    Unknown,
}

/// Why a node is at risk, with the edges that carry the impact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Risk {
    /// Level derived from the carrying path (see the module docs).
    pub level: RiskLevel,
    /// Weakest evidence on the path.
    pub weakest_evidence: EvidenceType,
    /// Number of ambiguous or unresolved edges on the path.
    pub flagged_hops: usize,
    /// The path from the changed node to this node, first hop first.
    pub carried_by: Vec<Hop>,
}

/// A node reached by impact propagation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactedNode {
    /// The node.
    pub node: NodeId,
    /// Display name.
    pub name: String,
    /// Node kind.
    pub kind: NodeKind,
    /// Owning project, `None` for contracts and decisions.
    pub project: Option<Name>,
    /// Number of edges on the carrying path.
    pub depth: usize,
    /// Risk and carrying edges.
    pub risk: Risk,
    /// Contract nodes passed on the way (excluding the node itself).
    pub contracts_crossed: Vec<NodeId>,
}

/// Why an impact is reported as unknown rather than certain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UnknownReason {
    /// The node was reached over a path that includes an unresolved edge.
    UnresolvedEdgeOnPath,
    /// An unresolved reference (dynamic call, computed name) uses the same
    /// name as a changed node; it may or may not point at it.
    NameMatchesUnresolvedReference,
}

/// A place where impact cannot be decided from the graph.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnknownImpact {
    /// The node that may be affected.
    pub node: NodeId,
    /// The unresolved edge responsible.
    pub edge: EdgeKey,
    /// Why it is unknown.
    pub reason: UnknownReason,
}

/// Impact grouped by project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectImpact {
    /// The project; `None` collects workspace-wide nodes (contracts, decisions).
    pub project: Option<Name>,
    /// Affected non-test nodes, ordered by risk, depth, id.
    pub affected: Vec<NodeId>,
    /// Tests reached in this project, ordered by risk, depth, id.
    pub tests: Vec<NodeId>,
    /// Strongest risk among affected nodes and tests of this project.
    pub highest_risk: RiskLevel,
}

/// Result of [`CodeGraph::impact`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactReport {
    /// The changed nodes.
    pub targets: Vec<NodeId>,
    /// Affected non-test nodes ordered by risk, depth, id. Targets are excluded.
    pub affected: Vec<ImpactedNode>,
    /// Test nodes reached: what to run.
    pub tests: Vec<ImpactedNode>,
    /// The same nodes grouped by project, sorted by project name with the
    /// project-less group last.
    pub by_project: Vec<ProjectImpact>,
    /// Places where impact is unknown rather than absent.
    pub unknown: Vec<UnknownImpact>,
    /// `true` when the node budget stopped the search early.
    pub truncated: bool,
}

#[derive(Clone)]
struct Entry {
    hops: Vec<Hop>,
    via_provider: bool,
}

impl Entry {
    fn key(&self) -> (u8, usize, usize, Vec<&EdgeKey>) {
        (
            weakest_evidence(&self.hops).map_or(0, EvidenceType::rank),
            flagged_count(&self.hops),
            self.hops.len(),
            self.hops.iter().map(|h| &h.edge).collect(),
        )
    }
}

fn risk_of(hops: &[Hop]) -> Risk {
    let weakest = weakest_evidence(hops).unwrap_or(EvidenceType::SemanticResolved);
    let flagged = flagged_count(hops);
    let any_unresolved = hops.iter().any(|h| h.resolution == Resolution::Unresolved);
    let any_ambiguous = hops.iter().any(|h| h.resolution == Resolution::Ambiguous);
    let level = if any_unresolved {
        RiskLevel::Unknown
    } else if any_ambiguous || weakest.rank() >= EvidenceType::Heuristic.rank() {
        RiskLevel::Low
    } else if weakest == EvidenceType::Syntactic || hops.len() > 2 {
        RiskLevel::Medium
    } else {
        RiskLevel::High
    };
    Risk {
        level,
        weakest_evidence: weakest,
        flagged_hops: flagged,
        carried_by: hops.to_vec(),
    }
}

/// Upper bound on relaxation rounds; each round deepens paths by one edge or
/// improves a node, so this is far above anything reachable at depth <= 8.
const MAX_ROUNDS: usize = 256;

impl CodeGraph {
    /// Reverse reachability from `spec.targets`: everything that may need to
    /// change, be re-tested or be re-read when the targets change.
    ///
    /// Errors when a target is unknown, the depth is outside `1..=8`, the
    /// budget is zero or no target is given. Unresolved and dynamic edges are
    /// reported in [`ImpactReport::unknown`] instead of being ignored.
    pub fn impact(&self, spec: &ImpactSpec) -> Result<ImpactReport, GraphError> {
        if spec.max_depth == 0 || spec.max_depth > MAX_IMPACT_DEPTH {
            return Err(GraphError::InvalidDepth {
                given: spec.max_depth,
                max: MAX_IMPACT_DEPTH,
            });
        }
        if spec.node_budget == 0 {
            return Err(GraphError::InvalidLimit("node budget"));
        }
        if spec.targets.is_empty() {
            return Err(GraphError::InvalidLimit("target count"));
        }
        let mut targets: BTreeSet<NodeId> = BTreeSet::new();
        for id in &spec.targets {
            if self.node(id).is_none() {
                return Err(GraphError::UnknownNode(id.clone()));
            }
            targets.insert(id.clone());
        }

        let mut best: BTreeMap<NodeId, Entry> = BTreeMap::new();
        for id in &targets {
            best.insert(
                id.clone(),
                Entry {
                    hops: Vec::new(),
                    via_provider: false,
                },
            );
        }
        let mut frontier: BTreeSet<NodeId> = targets.clone();
        let mut truncated = false;

        for _ in 0..MAX_ROUNDS {
            if frontier.is_empty() {
                break;
            }
            let mut next_frontier: BTreeSet<NodeId> = BTreeSet::new();
            for id in &frontier {
                let Some(entry) = best.get(id).cloned() else {
                    continue;
                };
                if entry.hops.len() >= usize::from(spec.max_depth) {
                    continue;
                }
                let terminal = self.node(id).is_some_and(|n| {
                    matches!(n.kind, NodeKind::Test | NodeKind::Doc | NodeKind::Decision)
                });
                if terminal && !entry.hops.is_empty() {
                    continue;
                }
                for nb in self.neighbors(id, Direction::Both, &spec.filter)? {
                    let kind = nb.edge.kind;
                    let via_provider = if nb.forward {
                        if !PROVIDER.contains(&kind) {
                            continue;
                        }
                        true
                    } else {
                        if !INCOMING.contains(&kind)
                            || (entry.via_provider && PROVIDER.contains(&kind))
                        {
                            continue;
                        }
                        false
                    };
                    let next = &nb.node.id;
                    if next == id || targets.contains(next) {
                        continue;
                    }
                    let mut hops = entry.hops.clone();
                    hops.push(nb.hop());
                    let candidate = Entry { hops, via_provider };
                    let better = match best.get(next) {
                        Some(old) => candidate.key() < old.key(),
                        None => {
                            if best.len().saturating_sub(targets.len()) >= spec.node_budget {
                                truncated = true;
                                continue;
                            }
                            true
                        }
                    };
                    if better {
                        best.insert(next.clone(), candidate);
                        next_frontier.insert(next.clone());
                    }
                }
            }
            frontier = next_frontier;
        }

        let mut affected = Vec::new();
        let mut tests = Vec::new();
        let mut unknown: BTreeSet<UnknownImpact> = BTreeSet::new();
        for (id, entry) in &best {
            if targets.contains(id) {
                continue;
            }
            let Some(node) = self.node(id) else { continue };
            let contracts_crossed = entry
                .hops
                .iter()
                .rev()
                .skip(1)
                .rev()
                .filter(|h| {
                    self.node(&h.to)
                        .is_some_and(|n| n.contract_kind().is_some())
                })
                .map(|h| h.to.clone())
                .collect();
            let impacted = ImpactedNode {
                node: id.clone(),
                name: node.name.clone(),
                kind: node.kind.clone(),
                project: node.project.clone(),
                depth: entry.hops.len(),
                risk: risk_of(&entry.hops),
                contracts_crossed,
            };
            if let Some(hop) = entry
                .hops
                .iter()
                .find(|h| h.resolution == Resolution::Unresolved)
            {
                unknown.insert(UnknownImpact {
                    node: id.clone(),
                    edge: hop.edge.clone(),
                    reason: UnknownReason::UnresolvedEdgeOnPath,
                });
            }
            if node.kind == NodeKind::Test {
                tests.push(impacted);
            } else {
                affected.push(impacted);
            }
        }
        let order = |a: &ImpactedNode, b: &ImpactedNode| {
            (a.risk.level, a.depth, &a.node).cmp(&(b.risk.level, b.depth, &b.node))
        };
        affected.sort_by(order);
        tests.sort_by(order);

        self.collect_name_matches(&targets, &mut unknown)?;

        let by_project = group_by_project(&affected, &tests);
        Ok(ImpactReport {
            targets: targets.into_iter().collect(),
            affected,
            tests,
            by_project,
            unknown: unknown.into_iter().collect(),
            truncated,
        })
    }

    /// Unresolved references whose placeholder shares a name with a target:
    /// the reference may be a dynamic use of the changed node.
    fn collect_name_matches(
        &self,
        targets: &BTreeSet<NodeId>,
        unknown: &mut BTreeSet<UnknownImpact>,
    ) -> Result<(), GraphError> {
        let names: BTreeSet<&str> = targets
            .iter()
            .filter_map(|id| self.node(id))
            .map(|n| n.name.as_str())
            .collect();
        for placeholder in self.nodes().filter(|n| n.is_unresolved_placeholder()) {
            if !names.contains(placeholder.name.as_str()) {
                continue;
            }
            for nb in self.incident(&placeholder.id, Direction::Incoming)? {
                if targets.contains(&nb.node.id) {
                    continue;
                }
                unknown.insert(UnknownImpact {
                    node: nb.node.id.clone(),
                    edge: nb.key(),
                    reason: UnknownReason::NameMatchesUnresolvedReference,
                });
            }
        }
        Ok(())
    }
}

fn group_by_project(affected: &[ImpactedNode], tests: &[ImpactedNode]) -> Vec<ProjectImpact> {
    let mut groups: BTreeMap<Option<Name>, ProjectImpact> = BTreeMap::new();
    for (list, is_test) in [(affected, false), (tests, true)] {
        for item in list {
            let group = groups
                .entry(item.project.clone())
                .or_insert_with(|| ProjectImpact {
                    project: item.project.clone(),
                    affected: Vec::new(),
                    tests: Vec::new(),
                    highest_risk: RiskLevel::Unknown,
                });
            if is_test {
                group.tests.push(item.node.clone());
            } else {
                group.affected.push(item.node.clone());
            }
            group.highest_risk = group.highest_risk.min(item.risk.level);
        }
    }
    let mut out: Vec<ProjectImpact> = groups.into_values().collect();
    out.sort_by(|a, b| (a.project.is_none(), &a.project).cmp(&(b.project.is_none(), &b.project)));
    out
}
