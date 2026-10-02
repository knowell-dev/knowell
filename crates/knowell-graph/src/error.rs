//! Error type for graph construction, updates and queries.

use knowell_core::Name;

use crate::model::{EdgeKey, NodeId};

/// Everything that can go wrong in this crate. Messages never contain file
/// contents or secret values; they only name graph identifiers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraphError {
    /// A node identifier was empty.
    #[error("node id must not be empty")]
    EmptyNodeId,
    /// A query or delta named a node that is not in the graph.
    #[error("unknown node `{0}`")]
    UnknownNode(NodeId),
    /// A delta tried to remove an edge that is not in the graph.
    #[error("unknown edge `{0}`")]
    UnknownEdge(EdgeKey),
    /// A delta carried a generation that is not newer than the graph's.
    #[error(
        "stale generation {got} for project `{project}`: the graph is already at generation {current}"
    )]
    StaleGeneration {
        /// Project whose view the delta belongs to.
        project: Name,
        /// Generation the graph currently holds for that project.
        current: u64,
        /// Generation carried by the rejected delta.
        got: u64,
    },
    /// A delta touched a node owned by another project's view.
    #[error("delta for project `{delta_project}` touches node `{node}` owned by another project")]
    ForeignNode {
        /// Project the delta belongs to.
        delta_project: Name,
        /// The offending node.
        node: NodeId,
    },
    /// A delta touched an edge none of whose endpoints belongs to its project.
    #[error("delta for project `{delta_project}` touches edge `{edge}` owned by another project")]
    ForeignEdge {
        /// Project the delta belongs to.
        delta_project: Name,
        /// The offending edge.
        edge: EdgeKey,
    },
    /// An added edge refers to a node that would not exist after the delta.
    #[error("edge `{edge}` refers to missing node `{node}`")]
    DanglingEdge {
        /// The offending edge.
        edge: EdgeKey,
        /// The endpoint that does not exist.
        node: NodeId,
    },
    /// The same node or edge appears twice in one delta.
    #[error("delta mentions `{0}` more than once")]
    DuplicateInDelta(String),
    /// A requested depth is outside the allowed range.
    #[error("depth {given} is outside the allowed range 1..={max}")]
    InvalidDepth {
        /// The rejected depth.
        given: u8,
        /// The largest accepted depth.
        max: u8,
    },
    /// A budget or limit that must be positive was zero.
    #[error("{0} must be at least 1")]
    InvalidLimit(&'static str),
}
