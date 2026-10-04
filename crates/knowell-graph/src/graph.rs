//! The in-memory graph and its generation-fenced incremental updates.

use std::collections::{BTreeMap, BTreeSet};

use knowell_core::Name;
use petgraph::Directed;
use petgraph::stable_graph::{EdgeIndex, NodeIndex, StableGraph};
use petgraph::visit::EdgeRef;

use crate::error::GraphError;
use crate::model::{Direction, Edge, EdgeFilter, EdgeKey, EdgeRecord, Hop, Node, NodeId};

/// An incremental change to the graph, produced for one project view at one
/// generation. Applying deltas in generation order makes the in-memory graph
/// mirror what storage holds.
///
/// Ownership rules (they keep one project's re-index from clobbering another's):
/// - added nodes must belong to `project` or to nobody (contracts, decisions);
/// - removed nodes must belong to `project` or to nobody;
/// - added or removed edges must have at least one endpoint owned by
///   `project`, or have two unowned endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphDelta {
    /// Project view the delta belongs to.
    pub project: Name,
    /// View generation; must be strictly greater than the graph's current
    /// generation for `project` (the first delta of a project uses 1 or more).
    pub generation: u64,
    /// Nodes to insert or replace (by id).
    pub added_nodes: Vec<Node>,
    /// Nodes to remove, together with every edge touching them.
    pub removed_nodes: Vec<NodeId>,
    /// Edges to insert or replace (by `(from, to, kind)`).
    pub added_edges: Vec<EdgeRecord>,
    /// Edges to remove.
    pub removed_edges: Vec<EdgeKey>,
}

impl GraphDelta {
    /// An empty delta.
    pub fn new(project: Name, generation: u64) -> Self {
        Self {
            project,
            generation,
            added_nodes: Vec::new(),
            removed_nodes: Vec::new(),
            added_edges: Vec::new(),
            removed_edges: Vec::new(),
        }
    }

    /// Adds a node to insert or replace.
    #[must_use]
    pub fn add_node(mut self, node: Node) -> Self {
        self.added_nodes.push(node);
        self
    }

    /// Adds an edge to insert or replace.
    #[must_use]
    pub fn add_edge(mut self, from: &NodeId, to: &NodeId, edge: Edge) -> Self {
        self.added_edges
            .push(EdgeRecord::new(from.clone(), to.clone(), edge));
        self
    }

    /// Adds a node to remove.
    #[must_use]
    pub fn remove_node(mut self, id: &NodeId) -> Self {
        self.removed_nodes.push(id.clone());
        self
    }

    /// Adds an edge to remove.
    #[must_use]
    pub fn remove_edge(mut self, key: EdgeKey) -> Self {
        self.removed_edges.push(key);
        self
    }
}

/// What an applied delta changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplySummary {
    /// Nodes that did not exist before.
    pub nodes_added: usize,
    /// Nodes that were replaced.
    pub nodes_replaced: usize,
    /// Nodes removed.
    pub nodes_removed: usize,
    /// Edges that did not exist before.
    pub edges_added: usize,
    /// Edges that were replaced.
    pub edges_replaced: usize,
    /// Edges removed explicitly or because an endpoint was removed.
    pub edges_removed: usize,
}

/// An edge seen from one of its endpoints.
#[derive(Debug, Clone, Copy)]
pub struct Neighbor<'a> {
    /// The node at the other end.
    pub node: &'a Node,
    /// The edge payload.
    pub edge: &'a Edge,
    /// Source of the edge.
    pub from: &'a NodeId,
    /// Target of the edge.
    pub to: &'a NodeId,
    /// `true` when the queried node is the edge's source (the edge leaves it).
    pub forward: bool,
}

impl Neighbor<'_> {
    /// Identity of the edge.
    pub fn key(&self) -> EdgeKey {
        EdgeKey {
            from: self.from.clone(),
            to: self.to.clone(),
            kind: self.edge.kind,
        }
    }

    /// The step from the queried node to [`Neighbor::node`] as a [`Hop`].
    pub fn hop(&self) -> Hop {
        let (start, end) = if self.forward {
            (self.from, self.to)
        } else {
            (self.to, self.from)
        };
        Hop {
            from: start.clone(),
            to: end.clone(),
            edge: self.key(),
            forward: self.forward,
            evidence: self.edge.evidence,
            resolution: self.edge.resolution,
        }
    }
}

/// The code and contract graph.
///
/// Backed by a petgraph `StableGraph` plus an id index. All iteration orders
/// exposed by this type are sorted, never hash-dependent.
#[derive(Debug, Clone, Default)]
pub struct CodeGraph {
    graph: StableGraph<Node, Edge, Directed>,
    nodes: BTreeMap<NodeId, NodeIndex>,
    edges: BTreeMap<EdgeKey, EdgeIndex>,
    generations: BTreeMap<Name, u64>,
}

impl CodeGraph {
    /// An empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of edges.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Looks up a node.
    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        self.nodes
            .get(id)
            .and_then(|idx| self.graph.node_weight(*idx))
    }

    /// Looks up an edge.
    pub fn edge(&self, key: &EdgeKey) -> Option<&Edge> {
        self.edges
            .get(key)
            .and_then(|idx| self.graph.edge_weight(*idx))
    }

    /// All nodes, ordered by id.
    pub fn nodes(&self) -> impl Iterator<Item = &Node> + '_ {
        self.nodes
            .values()
            .filter_map(|idx| self.graph.node_weight(*idx))
    }

    /// All edges, ordered by key.
    pub fn edges(&self) -> impl Iterator<Item = (&EdgeKey, &Edge)> + '_ {
        self.edges
            .iter()
            .filter_map(|(key, idx)| self.graph.edge_weight(*idx).map(|edge| (key, edge)))
    }

    /// The generation the graph holds for `project` (0 when it has none yet).
    pub fn generation(&self, project: &Name) -> u64 {
        self.generations.get(project).copied().unwrap_or(0)
    }

    /// Projects that have had at least one delta applied, with their generations.
    pub fn generations(&self) -> impl Iterator<Item = (&Name, u64)> + '_ {
        self.generations.iter().map(|(name, gen_)| (name, *gen_))
    }

    fn index_of(&self, id: &NodeId) -> Result<NodeIndex, GraphError> {
        self.nodes
            .get(id)
            .copied()
            .ok_or_else(|| GraphError::UnknownNode(id.clone()))
    }

    /// Every edge touching `id` in `direction`, unfiltered, in a stable order:
    /// by edge kind, then other node id, then outgoing before incoming.
    pub(crate) fn incident(
        &self,
        id: &NodeId,
        direction: Direction,
    ) -> Result<Vec<Neighbor<'_>>, GraphError> {
        let idx = self.index_of(id)?;
        let mut out = Vec::new();
        let want_out = matches!(direction, Direction::Outgoing | Direction::Both);
        let want_in = matches!(direction, Direction::Incoming | Direction::Both);
        let mut collect = |dir: petgraph::Direction, forward: bool| {
            for e in self.graph.edges_directed(idx, dir) {
                // A self-loop is reported once, as outgoing.
                if !forward && want_out && e.source() == e.target() {
                    continue;
                }
                let (Some(src), Some(dst)) = (
                    self.graph.node_weight(e.source()),
                    self.graph.node_weight(e.target()),
                ) else {
                    continue;
                };
                out.push(Neighbor {
                    node: if forward { dst } else { src },
                    edge: e.weight(),
                    from: &src.id,
                    to: &dst.id,
                    forward,
                });
            }
        };
        if want_out {
            collect(petgraph::Direction::Outgoing, true);
        }
        if want_in {
            collect(petgraph::Direction::Incoming, false);
        }
        out.sort_by(|a, b| {
            (a.edge.kind, &a.node.id, !a.forward).cmp(&(b.edge.kind, &b.node.id, !b.forward))
        });
        Ok(out)
    }

    /// Edges touching `id` in `direction` that pass `filter`, in a stable order
    /// (edge kind, other node id, outgoing before incoming).
    pub fn neighbors(
        &self,
        id: &NodeId,
        direction: Direction,
        filter: &EdgeFilter,
    ) -> Result<Vec<Neighbor<'_>>, GraphError> {
        let mut all = self.incident(id, direction)?;
        all.retain(|n| filter.matches(n.edge));
        Ok(all)
    }

    /// Streams matching adjacencies without allocating or sorting the whole
    /// fanout. Callers selecting a bounded result must supply deterministic
    /// ranking; petgraph's insertion order is not a result ordering.
    pub(crate) fn visit_neighbors<'a>(
        &'a self,
        id: &NodeId,
        direction: Direction,
        filter: &EdgeFilter,
        mut visit: impl FnMut(Neighbor<'a>),
    ) -> Result<(), GraphError> {
        let index = self.index_of(id)?;
        let want_out = matches!(direction, Direction::Outgoing | Direction::Both);
        let want_in = matches!(direction, Direction::Incoming | Direction::Both);
        for (enabled, direction, forward) in [
            (want_out, petgraph::Direction::Outgoing, true),
            (want_in, petgraph::Direction::Incoming, false),
        ] {
            if !enabled {
                continue;
            }
            for edge in self.graph.edges_directed(index, direction) {
                if !forward && want_out && edge.source() == edge.target() {
                    continue;
                }
                if !filter.matches(edge.weight()) {
                    continue;
                }
                let (Some(source), Some(target)) = (
                    self.graph.node_weight(edge.source()),
                    self.graph.node_weight(edge.target()),
                ) else {
                    continue;
                };
                visit(Neighbor {
                    node: if forward { target } else { source },
                    edge: edge.weight(),
                    from: &source.id,
                    to: &target.id,
                    forward,
                });
            }
        }
        Ok(())
    }

    fn edge_key_of(&self, idx: EdgeIndex) -> Option<EdgeKey> {
        let (s, t) = self.graph.edge_endpoints(idx)?;
        let kind = self.graph.edge_weight(idx)?.kind;
        Some(EdgeKey {
            from: self.graph.node_weight(s)?.id.clone(),
            to: self.graph.node_weight(t)?.id.clone(),
            kind,
        })
    }

    /// Applies a delta atomically: either every change is made or, on error,
    /// the graph is left untouched.
    ///
    /// Application order: explicit edge removals, node removals (cascading to
    /// incident edges), node upserts, edge upserts. Stale or repeated
    /// generations are rejected (fencing) so a late-finishing old job can
    /// never overwrite a newer view.
    pub fn apply(&mut self, delta: GraphDelta) -> Result<ApplySummary, GraphError> {
        self.validate(&delta)?;

        let mut summary = ApplySummary::default();

        for key in &delta.removed_edges {
            if let Some(idx) = self.edges.remove(key) {
                self.graph.remove_edge(idx);
                summary.edges_removed += 1;
            }
        }

        for id in &delta.removed_nodes {
            let Some(idx) = self.nodes.remove(id) else {
                continue;
            };
            let mut touching: Vec<EdgeIndex> = Vec::new();
            for dir in [petgraph::Direction::Outgoing, petgraph::Direction::Incoming] {
                touching.extend(self.graph.edges_directed(idx, dir).map(|e| e.id()));
            }
            for edge_idx in touching {
                if let Some(key) = self.edge_key_of(edge_idx)
                    && self.edges.remove(&key).is_some()
                {
                    summary.edges_removed += 1;
                }
            }
            self.graph.remove_node(idx);
            summary.nodes_removed += 1;
        }

        for mut node in delta.added_nodes {
            node.generation = delta.generation;
            if let Some(idx) = self.nodes.get(&node.id).copied() {
                if let Some(slot) = self.graph.node_weight_mut(idx) {
                    *slot = node;
                }
                summary.nodes_replaced += 1;
            } else {
                let id = node.id.clone();
                let idx = self.graph.add_node(node);
                self.nodes.insert(id, idx);
                summary.nodes_added += 1;
            }
        }

        for record in delta.added_edges {
            let key = record.key();
            if let Some(idx) = self.edges.get(&key).copied() {
                if let Some(slot) = self.graph.edge_weight_mut(idx) {
                    *slot = record.edge;
                }
                summary.edges_replaced += 1;
            } else if let (Some(from), Some(to)) = (
                self.nodes.get(&record.from).copied(),
                self.nodes.get(&record.to).copied(),
            ) {
                let idx = self.graph.add_edge(from, to, record.edge);
                self.edges.insert(key, idx);
                summary.edges_added += 1;
            }
            // Endpoints were validated to exist, so the else branch is unreachable.
        }

        self.generations
            .insert(delta.project.clone(), delta.generation);
        Ok(summary)
    }

    /// Checks every precondition of [`CodeGraph::apply`] without mutating.
    fn validate(&self, delta: &GraphDelta) -> Result<(), GraphError> {
        let current = self.generation(&delta.project);
        if delta.generation <= current {
            return Err(GraphError::StaleGeneration {
                project: delta.project.clone(),
                current,
                got: delta.generation,
            });
        }

        let mut added: BTreeMap<&NodeId, &Node> = BTreeMap::new();
        for node in &delta.added_nodes {
            if added.insert(&node.id, node).is_some() {
                return Err(GraphError::DuplicateInDelta(node.id.to_string()));
            }
            if let Some(owner) = &node.project
                && owner != &delta.project
            {
                return Err(self.foreign_node(delta, &node.id));
            }
            if let Some(existing) = self.node(&node.id)
                && let Some(owner) = &existing.project
                && owner != &delta.project
            {
                return Err(self.foreign_node(delta, &node.id));
            }
        }

        let mut removed: BTreeSet<&NodeId> = BTreeSet::new();
        for id in &delta.removed_nodes {
            if !removed.insert(id) || added.contains_key(id) {
                return Err(GraphError::DuplicateInDelta(id.to_string()));
            }
            let existing = self
                .node(id)
                .ok_or_else(|| GraphError::UnknownNode(id.clone()))?;
            if let Some(owner) = &existing.project
                && owner != &delta.project
            {
                return Err(self.foreign_node(delta, id));
            }
        }

        let mut removed_edges: BTreeSet<&EdgeKey> = BTreeSet::new();
        for key in &delta.removed_edges {
            if !removed_edges.insert(key) {
                return Err(GraphError::DuplicateInDelta(key.to_string()));
            }
            if !self.edges.contains_key(key) {
                return Err(GraphError::UnknownEdge(key.clone()));
            }
            let from = self.node(&key.from).and_then(|n| n.project.as_ref());
            let to = self.node(&key.to).and_then(|n| n.project.as_ref());
            if !edge_owned(&delta.project, from, to) {
                return Err(GraphError::ForeignEdge {
                    delta_project: delta.project.clone(),
                    edge: key.clone(),
                });
            }
        }

        let mut added_edges: BTreeSet<EdgeKey> = BTreeSet::new();
        for record in &delta.added_edges {
            let key = record.key();
            if !added_edges.insert(key.clone()) {
                return Err(GraphError::DuplicateInDelta(key.to_string()));
            }
            let mut owners: [Option<&Name>; 2] = [None, None];
            for (slot, id) in owners.iter_mut().zip([&record.from, &record.to]) {
                let node = match added.get(id) {
                    Some(node) => Some(*node),
                    None if removed.contains(id) => None,
                    None => self.node(id),
                };
                let Some(node) = node else {
                    return Err(GraphError::DanglingEdge {
                        edge: key,
                        node: id.clone(),
                    });
                };
                *slot = node.project.as_ref();
            }
            let [from, to] = owners;
            if !edge_owned(&delta.project, from, to) {
                return Err(GraphError::ForeignEdge {
                    delta_project: delta.project.clone(),
                    edge: key,
                });
            }
        }
        Ok(())
    }

    fn foreign_node(&self, delta: &GraphDelta, node: &NodeId) -> GraphError {
        GraphError::ForeignNode {
            delta_project: delta.project.clone(),
            node: node.clone(),
        }
    }
}

/// An edge is owned by a delta when one endpoint belongs to its project, or
/// when both endpoints are unowned (contract-to-contract links).
fn edge_owned(project: &Name, from: Option<&Name>, to: Option<&Name>) -> bool {
    from == Some(project) || to == Some(project) || (from.is_none() && to.is_none())
}
