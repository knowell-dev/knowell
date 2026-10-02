//! Bounded breadth-first walks.

use std::collections::{BTreeMap, VecDeque};

use crate::error::GraphError;
use crate::graph::CodeGraph;
use crate::model::{Direction, EdgeFilter, Hop, NodeId};

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
    /// `true` when the node budget stopped the walk early; the result is a
    /// deterministic prefix, not the full neighborhood.
    pub truncated: bool,
}

impl CodeGraph {
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
        })
    }
}
