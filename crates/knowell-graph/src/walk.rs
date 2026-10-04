//! Bounded breadth-first walks.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::error::GraphError;
use crate::graph::{CodeGraph, Neighbor};
use crate::model::{Direction, EdgeFilter, EdgeKind, EvidenceType, Hop, NodeId, Resolution};

/// Deepest walk accepted by [`CodeGraph::walk`].
pub const MAX_WALK_DEPTH: u8 = 5;

/// Parameters of a bounded walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkSpec {
    /// Maximum number of edges from the start, `1..=`[`MAX_WALK_DEPTH`].
    pub max_depth: u8,
    /// Which way to follow edges.
    pub direction: Direction,
    /// Which edges may be used.
    pub filter: EdgeFilter,
    /// Maximum number of nodes returned, excluding the start (at least 1).
    pub node_budget: usize,
}

impl WalkSpec {
    /// A walk of `max_depth` in `direction` over any edge, with a budget of 500 nodes.
    pub fn new(max_depth: u8, direction: Direction) -> Self {
        Self {
            max_depth,
            direction,
            filter: EdgeFilter::any(),
            node_budget: 500,
        }
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

/// A node reached by a walk and the shortest path that reached it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Visit {
    /// The reached node.
    pub node: NodeId,
    /// Number of edges on the path (1 or more).
    pub depth: u8,
    /// The path from the start, first hop first. Among shortest paths the one
    /// found first in stable neighbor order wins (edge kind, then node id).
    pub path: Vec<Hop>,
}

/// Result of [`CodeGraph::walk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkResult {
    /// The start node.
    pub start: NodeId,
    /// Visited nodes in breadth-first order (depth, then discovery order).
    pub visits: Vec<Visit>,
    /// `true` when the node budget stopped the walk early. Navigation also
    /// reports depth, per-node fanout and candidate budget omissions. Results
    /// remain deterministic but do not represent the full neighborhood.
    pub truncated: bool,
    /// Optional navigation candidates. Each path contains one final weak or
    /// non-resolved edge; those targets are inspected but never expanded.
    /// Empty for the legacy [`CodeGraph::walk`] traversal.
    pub candidate_visits: Vec<Visit>,
    /// Accepted strong adjacencies omitted by the per-node navigation limit.
    /// Counts inspected adjacencies, not the entire unseen neighborhood.
    pub fanout_omitted: usize,
    /// Candidate adjacencies omitted by the candidate or total node budget.
    /// Counts inspected adjacencies, not descendants of candidate leaves.
    pub candidate_omitted: usize,
    /// Matching adjacencies to unseen nodes beyond the navigation depth limit.
    /// Zero for legacy walks, whose depth is an explicit search boundary.
    pub depth_omitted: usize,
}

impl CodeGraph {
    /// Walks resolved, syntactically-or-more-strongly evidenced relations first.
    ///
    /// `per_node_limit` bounds expanded neighbors per node (at least one),
    /// balanced across relation kinds so a type's reference fanout does not
    /// consume every slot. `candidate_limit` bounds additional weak, ambiguous
    /// or unresolved edge inspections across the whole walk; zero disables
    /// candidate inspection. Candidates are leaves and never carry expansion.
    ///
    /// Evidence and resolution remain separate: accepting a syntactic edge
    /// does not establish runtime behavior. Reported omissions describe only
    /// inspected adjacencies; neither an empty walk nor an omitted neighborhood
    /// proves absence. The legacy [`Self::walk`] ordering and filtering remain
    /// unchanged.
    pub fn walk_navigation(
        &self,
        start: &NodeId,
        spec: &WalkSpec,
        per_node_limit: usize,
        candidate_limit: usize,
    ) -> Result<WalkResult, GraphError> {
        if spec.max_depth == 0 || spec.max_depth > MAX_WALK_DEPTH {
            return Err(GraphError::InvalidDepth {
                given: spec.max_depth,
                max: MAX_WALK_DEPTH,
            });
        }
        if spec.node_budget == 0 {
            return Err(GraphError::InvalidLimit("node budget"));
        }
        if per_node_limit == 0 {
            return Err(GraphError::InvalidLimit("per-node navigation limit"));
        }
        if self.node(start).is_none() {
            return Err(GraphError::UnknownNode(start.clone()));
        }

        let mut parent: BTreeMap<NodeId, Hop> = BTreeMap::new();
        let mut depth_of = BTreeMap::from([(start.clone(), 0u8)]);
        let mut order = Vec::new();
        let mut queue = VecDeque::from([start.clone()]);
        let mut pending_candidates = Vec::new();
        let mut pending_candidate_edges = BTreeSet::new();
        let mut processed = BTreeSet::new();
        let mut fanout_omitted = 0usize;
        let mut candidate_omitted = 0usize;
        let mut depth_omitted = 0usize;
        let mut truncated = false;

        while let Some(current) = queue.pop_front() {
            processed.insert(current.clone());
            let depth = depth_of.get(&current).copied().unwrap_or(0);
            if depth >= spec.max_depth {
                self.visit_neighbors(&current, spec.direction, &spec.filter, |neighbor| {
                    if !depth_of.contains_key(&neighbor.node.id) {
                        depth_omitted = depth_omitted.saturating_add(1);
                    }
                })?;
                continue;
            }

            let mut primary: BTreeMap<EdgeKind, Vec<Neighbor<'_>>> = BTreeMap::new();
            let mut candidates = Vec::new();
            let mut primary_count = 0usize;
            let mut candidate_count = 0usize;
            let remaining_candidates = candidate_limit.saturating_sub(pending_candidates.len());
            self.visit_neighbors(&current, spec.direction, &spec.filter, |neighbor| {
                if neighbor.edge.resolution == Resolution::Resolved
                    && neighbor.edge.evidence.at_least(EvidenceType::Syntactic)
                {
                    if !depth_of.contains_key(&neighbor.node.id) {
                        primary_count = primary_count.saturating_add(1);
                        retain_navigation_neighbor(
                            primary.entry(neighbor.edge.kind).or_default(),
                            neighbor,
                            per_node_limit,
                        );
                    }
                } else {
                    // A Both walk observes an uncertain edge from each end.
                    // Charge it once before the bounded reservoir, even when
                    // its first observation could not fit the candidate cap.
                    // Processed primary nodes and kept keys are both bounded
                    // by existing traversal budgets.
                    if pending_candidate_edges.contains(&neighbor.key())
                        || (spec.direction == Direction::Both
                            && neighbor.node.id != current
                            && processed.contains(&neighbor.node.id))
                    {
                        return;
                    }
                    candidate_count = candidate_count.saturating_add(1);
                    retain_navigation_neighbor(&mut candidates, neighbor, remaining_candidates);
                }
            })?;
            candidate_omitted =
                candidate_omitted.saturating_add(candidate_count.saturating_sub(candidates.len()));
            for candidate in candidates {
                pending_candidate_edges.insert(candidate.key());
                pending_candidates.push((current.clone(), candidate.hop()));
            }

            let mut by_kind: BTreeMap<EdgeKind, VecDeque<Neighbor<'_>>> = primary
                .into_iter()
                .map(|(kind, neighbors)| (kind, VecDeque::from(neighbors)))
                .collect();
            let mut selected = Vec::new();
            let mut selected_nodes = BTreeSet::new();
            // Round-robin kind quotas keep one broad reference class from
            // hiding the calls, imports or tests adjacent to the same node.
            while selected.len() < per_node_limit {
                let mut advanced = false;
                for neighbors in by_kind.values_mut() {
                    if selected.len() >= per_node_limit {
                        break;
                    }
                    if let Some(neighbor) = neighbors.pop_front() {
                        if selected_nodes.insert(&neighbor.node.id) {
                            selected.push(neighbor);
                        }
                        advanced = true;
                    }
                }
                if !advanced {
                    break;
                }
            }
            fanout_omitted =
                fanout_omitted.saturating_add(primary_count.saturating_sub(selected.len()));
            for neighbor in selected {
                if depth_of.contains_key(&neighbor.node.id) {
                    continue;
                }
                if order.len() >= spec.node_budget {
                    truncated = true;
                    continue;
                }
                let id = neighbor.node.id.clone();
                parent.insert(id.clone(), neighbor.hop());
                depth_of.insert(id.clone(), depth.saturating_add(1));
                order.push(id.clone());
                queue.push_back(id);
            }
        }

        let visits = order
            .iter()
            .map(|node| Visit {
                node: node.clone(),
                depth: depth_of.get(node).copied().unwrap_or(0),
                path: navigation_path(&parent, node),
            })
            .collect();
        let mut admitted: BTreeSet<NodeId> = depth_of.keys().cloned().collect();
        let mut candidate_visits = Vec::new();
        let mut candidate_edges = BTreeSet::new();
        for (origin, hop) in pending_candidates {
            if !candidate_edges.insert(hop.edge.clone()) {
                continue;
            }
            if !admitted.contains(&hop.to) && admitted.len().saturating_sub(1) >= spec.node_budget {
                candidate_omitted = candidate_omitted.saturating_add(1);
                continue;
            }
            admitted.insert(hop.to.clone());
            let mut path = navigation_path(&parent, &origin);
            path.push(hop.clone());
            candidate_visits.push(Visit {
                node: hop.to,
                depth: depth_of
                    .get(&origin)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(1),
                path,
            });
        }
        truncated |= fanout_omitted > 0 || candidate_omitted > 0 || depth_omitted > 0;
        Ok(WalkResult {
            start: start.clone(),
            visits,
            truncated,
            candidate_visits,
            fanout_omitted,
            candidate_omitted,
            depth_omitted,
        })
    }

    /// Breadth-first walk from `start`, bounded by depth and node budget.
    ///
    /// Every node is reported once with its shortest path. Errors when the
    /// start is unknown, the depth is outside `1..=5`, or the budget is zero.
    pub fn walk(&self, start: &NodeId, spec: &WalkSpec) -> Result<WalkResult, GraphError> {
        if spec.max_depth == 0 || spec.max_depth > MAX_WALK_DEPTH {
            return Err(GraphError::InvalidDepth {
                given: spec.max_depth,
                max: MAX_WALK_DEPTH,
            });
        }
        if spec.node_budget == 0 {
            return Err(GraphError::InvalidLimit("node budget"));
        }
        if self.node(start).is_none() {
            return Err(GraphError::UnknownNode(start.clone()));
        }

        let mut parent: BTreeMap<NodeId, Hop> = BTreeMap::new();
        let mut depth_of: BTreeMap<NodeId, u8> = BTreeMap::new();
        depth_of.insert(start.clone(), 0);
        let mut order: Vec<NodeId> = Vec::new();
        let mut queue: VecDeque<NodeId> = VecDeque::new();
        queue.push_back(start.clone());
        let mut truncated = false;

        'outer: while let Some(current) = queue.pop_front() {
            let depth = depth_of.get(&current).copied().unwrap_or(0);
            if depth >= spec.max_depth {
                continue;
            }
            for neighbor in self.neighbors(&current, spec.direction, &spec.filter)? {
                if depth_of.contains_key(&neighbor.node.id) {
                    continue;
                }
                if order.len() >= spec.node_budget {
                    truncated = true;
                    break 'outer;
                }
                let id = neighbor.node.id.clone();
                depth_of.insert(id.clone(), depth.saturating_add(1));
                parent.insert(id.clone(), neighbor.hop());
                order.push(id.clone());
                queue.push_back(id);
            }
        }

        let visits = order
            .into_iter()
            .map(|node| {
                let mut path = Vec::new();
                let mut cursor = node.clone();
                while let Some(hop) = parent.get(&cursor) {
                    path.push(hop.clone());
                    cursor = hop.from.clone();
                }
                path.reverse();
                let depth = depth_of.get(&node).copied().unwrap_or(0);
                Visit { node, depth, path }
            })
            .collect();

        Ok(WalkResult {
            start: start.clone(),
            visits,
            truncated,
            candidate_visits: Vec::new(),
            fanout_omitted: 0,
            candidate_omitted: 0,
            depth_omitted: 0,
        })
    }
}

fn navigation_rank<'a>(neighbor: &Neighbor<'a>) -> (u8, Resolution, EdgeKind, &'a NodeId, bool) {
    (
        neighbor.edge.evidence.rank(),
        neighbor.edge.resolution,
        neighbor.edge.kind,
        &neighbor.node.id,
        !neighbor.forward,
    )
}

fn retain_navigation_neighbor<'a>(
    selected: &mut Vec<Neighbor<'a>>,
    neighbor: Neighbor<'a>,
    limit: usize,
) {
    if limit == 0 {
        return;
    }
    let rank = navigation_rank(&neighbor);
    let position = selected.partition_point(|existing| navigation_rank(existing) < rank);
    if position >= limit {
        return;
    }
    selected.insert(position, neighbor);
    selected.truncate(limit);
}

fn navigation_path(parent: &BTreeMap<NodeId, Hop>, node: &NodeId) -> Vec<Hop> {
    let mut path = Vec::new();
    let mut cursor = node.clone();
    while let Some(hop) = parent.get(&cursor) {
        path.push(hop.clone());
        cursor = hop.from.clone();
    }
    path.reverse();
    path
}
