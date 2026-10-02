//! Code and contract graph: typed nodes and evidence-carrying edges,
//! bounded walks, cross-project flow tracing and impact analysis.
//!
//! The graph is an in-memory mirror of what storage holds for the active view
//! of every project. It is updated incrementally with [`GraphDelta`]s fenced
//! by view generation, and queried with:
//!
//! - [`CodeGraph::neighbors`] and [`CodeGraph::walk`]: bounded exploration;
//! - [`CodeGraph::trace_flow`]: the best paths between nodes, crossing contracts;
//! - [`CodeGraph::impact`]: reverse reachability with per-node risk and tests;
//! - [`CodeGraph::insights`]: deterministic checks for `know check`.
//!
//! Evidence type ([`EvidenceType`]) and resolution ([`Resolution`]) are kept
//! apart everywhere; no result collapses them into a single confidence number.
//! All outputs are deterministic: collections are sorted and ties are broken
//! explicitly.

mod error;
mod flow;
mod graph;
mod impact;
mod insights;
mod model;
mod walk;

pub use error::GraphError;
pub use flow::{FlowDirection, FlowPath, FlowSpec, FlowTrace, MAX_TRACE_DEPTH};
pub use graph::{ApplySummary, CodeGraph, GraphDelta, Neighbor};
pub use impact::{
    ImpactReport, ImpactSpec, ImpactedNode, MAX_IMPACT_DEPTH, ProjectImpact, Risk, RiskLevel,
    UnknownImpact, UnknownReason,
};
pub use insights::{Insight, InsightCode, InsightConfig, Severity};
pub use model::{
    ATTR_EXTERNAL, ATTR_LOCALE, ATTR_SCHEMA_HASH, ATTR_UNRESOLVED, ContractKind, Direction, Edge,
    EdgeFilter, EdgeKey, EdgeKind, EdgeRecord, EvidenceRef, EvidenceType, Hop, Node, NodeId,
    NodeKind, Resolution,
};
pub use walk::{MAX_WALK_DEPTH, Visit, WalkResult, WalkSpec};
