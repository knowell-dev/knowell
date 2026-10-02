//! Node, edge and evidence types.
//!
//! Evidence type and resolution status are two independent axes and are
//! never combined into one score: an edge can be strongly evidenced yet
//! ambiguous (two equally good targets), or weakly evidenced yet resolved.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use knowell_core::{ContentHash, LineRange, Name, RepoPath};
use serde::{Deserialize, Serialize};

use crate::error::GraphError;

/// Node attribute holding a contract's schema hash (hex text).
///
/// On a contract node it is the producer's current shape; on a `Consumes` or
/// `Reads` edge it is the shape the consumer was built against.
pub const ATTR_SCHEMA_HASH: &str = "schema_hash";
/// Node attribute (`"true"`) marking a placeholder target of an unresolved reference.
pub const ATTR_UNRESOLVED: &str = "unresolved";
/// Node attribute (`"true"`) marking a contract as used from outside the indexed workspace.
pub const ATTR_EXTERNAL: &str = "external";
/// Node attribute naming the locale of a locale file (for example `en`).
pub const ATTR_LOCALE: &str = "locale";

/// Stable string key of a node. The graph never interprets it; the constructors
/// below give the conventional shapes so independent producers agree on them.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NodeId(String);

impl NodeId {
    /// Wraps an arbitrary non-empty key.
    pub fn new(value: impl Into<String>) -> Result<Self, GraphError> {
        let value = value.into();
        if value.is_empty() {
            return Err(GraphError::EmptyNodeId);
        }
        Ok(Self(value))
    }

    /// Key of a project node.
    pub fn project(project: &Name) -> Self {
        Self(format!("project:{project}"))
    }

    /// Key of a file node.
    pub fn file(project: &Name, path: &RepoPath) -> Self {
        Self(format!("file:{project}:{path}"))
    }

    /// Key of a symbol node; `key` is the producer's stable symbol identity.
    pub fn symbol(project: &Name, key: &str) -> Self {
        Self(format!("symbol:{project}:{key}"))
    }

    /// Key of a contract node. Contracts are workspace-wide: the same
    /// `(kind, key)` from two projects is one node, which is what links them.
    pub fn contract(kind: ContractKind, key: &str) -> Self {
        Self(format!("contract:{}:{key}", kind.as_str()))
    }

    /// Key of a test node.
    pub fn test(project: &Name, key: &str) -> Self {
        Self(format!("test:{project}:{key}"))
    }

    /// Key of a documentation node.
    pub fn doc(project: &Name, key: &str) -> Self {
        Self(format!("doc:{project}:{key}"))
    }

    /// Key of a decision node.
    pub fn decision(key: &str) -> Self {
        Self(format!("decision:{key}"))
    }

    /// The key text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", self.0)
    }
}

impl TryFrom<String> for NodeId {
    type Error = GraphError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<NodeId> for String {
    fn from(value: NodeId) -> Self {
        value.0
    }
}

/// What a contract node stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractKind {
    /// An HTTP endpoint (`GET /v1/invoices/{id}`).
    Endpoint,
    /// An event topic or queue.
    Topic,
    /// A gRPC/RPC method.
    Rpc,
    /// A database table.
    Table,
    /// An environment variable or config **name** (never its value).
    EnvName,
    /// An i18n message key.
    I18nKey,
    /// A package or library.
    Package,
    /// A piece of infrastructure (service, port, queue broker).
    Infra,
}

impl ContractKind {
    /// Stable lowercase name, used in node ids and reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Endpoint => "endpoint",
            Self::Topic => "topic",
            Self::Rpc => "rpc",
            Self::Table => "table",
            Self::EnvName => "env_name",
            Self::I18nKey => "i18n_key",
            Self::Package => "package",
            Self::Infra => "infra",
        }
    }
}

/// The kind of a node, with the contract identity for contract nodes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// A project (a repository or a root inside a monorepo).
    Project,
    /// A source or config file.
    File,
    /// A code symbol (function, type, handler, ...).
    Symbol,
    /// A cross-project contract.
    Contract {
        /// What kind of contract.
        kind: ContractKind,
        /// Kind-specific identity (route, topic name, table name, ...).
        key: String,
    },
    /// A test.
    Test,
    /// A documentation page.
    Doc,
    /// A recorded decision.
    Decision,
}

/// A source location backing a node or an edge. A line range alone never
/// identifies code, so the content hash of the file version is always present.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EvidenceRef {
    /// Project the file belongs to.
    pub project: Name,
    /// Repository-relative path.
    pub path: RepoPath,
    /// 1-based inclusive line range, if the evidence is narrower than the file.
    pub range: Option<LineRange>,
    /// Hash of the file version the evidence was taken from.
    pub content_hash: ContentHash,
}

/// A graph node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// Stable key.
    pub id: NodeId,
    /// What the node is.
    pub kind: NodeKind,
    /// Owning project; `None` for workspace-wide nodes (contracts, decisions).
    pub project: Option<Name>,
    /// View generation of the delta that last wrote the node (set by
    /// [`CodeGraph::apply`](crate::CodeGraph::apply); ignored on input).
    pub generation: u64,
    /// Short display name.
    pub name: String,
    /// Definition site, when known.
    pub source: Option<EvidenceRef>,
    /// Free-form attributes (see the `ATTR_*` constants).
    pub attrs: BTreeMap<String, String>,
}

impl Node {
    fn build(id: NodeId, kind: NodeKind, project: Option<Name>, name: impl Into<String>) -> Self {
        Self {
            id,
            kind,
            project,
            generation: 0,
            name: name.into(),
            source: None,
            attrs: BTreeMap::new(),
        }
    }

    /// A project node.
    pub fn project(project: &Name) -> Self {
        Self::build(
            NodeId::project(project),
            NodeKind::Project,
            Some(project.clone()),
            project.as_str(),
        )
    }

    /// A file node; the display name is the file name.
    pub fn file(project: &Name, path: &RepoPath) -> Self {
        Self::build(
            NodeId::file(project, path),
            NodeKind::File,
            Some(project.clone()),
            path.file_name(),
        )
    }

    /// A symbol node.
    pub fn symbol(project: &Name, key: &str, name: &str) -> Self {
        Self::build(
            NodeId::symbol(project, key),
            NodeKind::Symbol,
            Some(project.clone()),
            name,
        )
    }

    /// A contract node (no owning project); the display name is the key.
    pub fn contract(kind: ContractKind, key: &str) -> Self {
        Self::build(
            NodeId::contract(kind, key),
            NodeKind::Contract {
                kind,
                key: key.to_owned(),
            },
            None,
            key,
        )
    }

    /// A test node.
    pub fn test(project: &Name, key: &str, name: &str) -> Self {
        Self::build(
            NodeId::test(project, key),
            NodeKind::Test,
            Some(project.clone()),
            name,
        )
    }

    /// A documentation node.
    pub fn doc(project: &Name, key: &str, name: &str) -> Self {
        Self::build(
            NodeId::doc(project, key),
            NodeKind::Doc,
            Some(project.clone()),
            name,
        )
    }

    /// A workspace-wide decision node.
    pub fn decision(key: &str, name: &str) -> Self {
        Self::build(NodeId::decision(key), NodeKind::Decision, None, name)
    }

    /// Adds an attribute.
    #[must_use]
    pub fn with_attr(mut self, key: &str, value: &str) -> Self {
        self.attrs.insert(key.to_owned(), value.to_owned());
        self
    }

    /// Sets the definition site.
    #[must_use]
    pub fn with_source(mut self, source: EvidenceRef) -> Self {
        self.source = Some(source);
        self
    }

    /// Looks up an attribute.
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.get(key).map(String::as_str)
    }

    /// The contract kind, for contract nodes.
    pub fn contract_kind(&self) -> Option<ContractKind> {
        match &self.kind {
            NodeKind::Contract { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// Whether this node is a placeholder target for an unresolved reference.
    pub fn is_unresolved_placeholder(&self) -> bool {
        self.attr(ATTR_UNRESOLVED) == Some("true")
    }
}

/// Relationship between two nodes. An edge `a --kind--> b` reads "a *kind* b".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// Symbol calls symbol.
    Calls,
    /// Symbol refers to a symbol or contract without calling it.
    References,
    /// Symbol implements an interface/trait/contract symbol.
    Implements,
    /// File or symbol imports another.
    Imports,
    /// File defines a symbol or contract (a locale file defines an i18n key,
    /// a compose file declares an env name).
    Defines,
    /// Structural containment (project/file/symbol).
    Contains,
    /// Test covers a symbol, file or contract.
    Tests,
    /// Documentation describes a node.
    Documents,
    /// Code produces an event/message on a contract.
    Produces,
    /// Code consumes a contract: calls an endpoint or RPC, subscribes to a topic.
    Consumes,
    /// Code reads a table, env name or i18n key.
    Reads,
    /// Code writes a table.
    Writes,
    /// Code serves an endpoint or RPC.
    Exposes,
    /// Project/service depends on a package or another service.
    DependsOn,
    /// A decision governs a node.
    Decides,
}

impl EdgeKind {
    /// Stable lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Calls => "calls",
            Self::References => "references",
            Self::Implements => "implements",
            Self::Imports => "imports",
            Self::Defines => "defines",
            Self::Contains => "contains",
            Self::Tests => "tests",
            Self::Documents => "documents",
            Self::Produces => "produces",
            Self::Consumes => "consumes",
            Self::Reads => "reads",
            Self::Writes => "writes",
            Self::Exposes => "exposes",
            Self::DependsOn => "depends_on",
            Self::Decides => "decides",
        }
    }
}

/// How an edge was established. Declaration order is strongest first, so
/// `a < b` means "`a` is stronger evidence than `b`".
///
/// `RuntimeObserved` sits directly below `SemanticResolved`: an observed call
/// happened, but traces are sampled and may not cover all paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceType {
    /// A semantic index (SCIP / language server) resolved the reference.
    SemanticResolved,
    /// Observed at runtime (traces, logs).
    RuntimeObserved,
    /// Derived by matching both ends of a declared contract (route, schema, topic).
    ContractDerived,
    /// Syntactic match (tree-sitter pattern, name match within scope).
    Syntactic,
    /// Heuristic guess (naming, proximity).
    Heuristic,
    /// Suggested by a model; never authoritative.
    ModelSuggestion,
}

impl EvidenceType {
    /// Strength rank: 0 is strongest. Used for ordering and thresholds.
    pub fn rank(self) -> u8 {
        match self {
            Self::SemanticResolved => 0,
            Self::RuntimeObserved => 1,
            Self::ContractDerived => 2,
            Self::Syntactic => 3,
            Self::Heuristic => 4,
            Self::ModelSuggestion => 5,
        }
    }

    /// Whether this evidence is at least as strong as `min`.
    pub fn at_least(self, min: EvidenceType) -> bool {
        self.rank() <= min.rank()
    }

    /// Stable lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SemanticResolved => "semantic_resolved",
            Self::RuntimeObserved => "runtime_observed",
            Self::ContractDerived => "contract_derived",
            Self::Syntactic => "syntactic",
            Self::Heuristic => "heuristic",
            Self::ModelSuggestion => "model_suggestion",
        }
    }
}

/// Whether an edge's target is known. Independent of [`EvidenceType`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// Exactly one target.
    Resolved,
    /// Several equally plausible targets; the edge is one of them.
    Ambiguous,
    /// The target could not be determined (dynamic dispatch, computed names).
    Unresolved,
}

impl Resolution {
    /// Stable lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Ambiguous => "ambiguous",
            Self::Unresolved => "unresolved",
        }
    }
}

/// An edge payload (endpoints live in [`EdgeRecord`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    /// Relationship.
    pub kind: EdgeKind,
    /// How it was established.
    pub evidence: EvidenceType,
    /// Whether the target is known.
    pub resolution: Resolution,
    /// Source locations backing the edge.
    pub evidence_refs: Vec<EvidenceRef>,
    /// Free-form attributes (for example [`ATTR_SCHEMA_HASH`] on consumer edges).
    pub attrs: BTreeMap<String, String>,
}

impl Edge {
    /// An edge with no references or attributes.
    pub fn new(kind: EdgeKind, evidence: EvidenceType, resolution: Resolution) -> Self {
        Self {
            kind,
            evidence,
            resolution,
            evidence_refs: Vec::new(),
            attrs: BTreeMap::new(),
        }
    }

    /// A resolved, semantically evidenced edge: the common strong case.
    pub fn resolved(kind: EdgeKind) -> Self {
        Self::new(kind, EvidenceType::SemanticResolved, Resolution::Resolved)
    }

    /// Adds a source reference.
    #[must_use]
    pub fn with_ref(mut self, evidence_ref: EvidenceRef) -> Self {
        self.evidence_refs.push(evidence_ref);
        self
    }

    /// Adds an attribute.
    #[must_use]
    pub fn with_attr(mut self, key: &str, value: &str) -> Self {
        self.attrs.insert(key.to_owned(), value.to_owned());
        self
    }

    /// Looks up an attribute.
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.get(key).map(String::as_str)
    }
}

/// Identity of an edge: at most one edge exists per `(from, to, kind)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EdgeKey {
    /// Source node.
    pub from: NodeId,
    /// Target node.
    pub to: NodeId,
    /// Relationship.
    pub kind: EdgeKind,
}

impl fmt::Display for EdgeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} -{}-> {}", self.from, self.kind.as_str(), self.to)
    }
}

/// An edge together with its endpoints, as carried by deltas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeRecord {
    /// Source node.
    pub from: NodeId,
    /// Target node.
    pub to: NodeId,
    /// Payload.
    pub edge: Edge,
}

impl EdgeRecord {
    /// Builds a record.
    pub fn new(from: NodeId, to: NodeId, edge: Edge) -> Self {
        Self { from, to, edge }
    }

    /// The identity of this edge.
    pub fn key(&self) -> EdgeKey {
        EdgeKey {
            from: self.from.clone(),
            to: self.to.clone(),
            kind: self.edge.kind,
        }
    }
}

/// Traversal direction relative to edge orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Follow edges from source to target.
    Outgoing,
    /// Follow edges from target to source.
    Incoming,
    /// Follow both.
    Both,
}

/// Which edges a traversal may use. The default accepts every edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeFilter {
    /// Allowed kinds; empty means all kinds.
    pub kinds: BTreeSet<EdgeKind>,
    /// Weakest accepted evidence; `None` accepts any.
    pub min_evidence: Option<EvidenceType>,
    /// Whether [`Resolution::Ambiguous`] edges may be used.
    pub allow_ambiguous: bool,
    /// Whether [`Resolution::Unresolved`] edges may be used.
    pub allow_unresolved: bool,
}

impl Default for EdgeFilter {
    fn default() -> Self {
        Self::any()
    }
}

impl EdgeFilter {
    /// Accepts every edge.
    pub fn any() -> Self {
        Self {
            kinds: BTreeSet::new(),
            min_evidence: None,
            allow_ambiguous: true,
            allow_unresolved: true,
        }
    }

    /// Restricts to the given kinds.
    #[must_use]
    pub fn with_kinds(mut self, kinds: impl IntoIterator<Item = EdgeKind>) -> Self {
        self.kinds = kinds.into_iter().collect();
        self
    }

    /// Requires evidence at least as strong as `min`.
    #[must_use]
    pub fn with_min_evidence(mut self, min: EvidenceType) -> Self {
        self.min_evidence = Some(min);
        self
    }

    /// Rejects ambiguous and unresolved edges.
    #[must_use]
    pub fn resolved_only(mut self) -> Self {
        self.allow_ambiguous = false;
        self.allow_unresolved = false;
        self
    }

    /// Whether `edge` passes the filter.
    pub fn matches(&self, edge: &Edge) -> bool {
        if !self.kinds.is_empty() && !self.kinds.contains(&edge.kind) {
            return false;
        }
        if let Some(min) = self.min_evidence
            && !edge.evidence.at_least(min)
        {
            return false;
        }
        match edge.resolution {
            Resolution::Resolved => true,
            Resolution::Ambiguous => self.allow_ambiguous,
            Resolution::Unresolved => self.allow_unresolved,
        }
    }
}

/// One traversed edge on a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hop {
    /// Node the step started from.
    pub from: NodeId,
    /// Node the step arrived at.
    pub to: NodeId,
    /// The edge used.
    pub edge: EdgeKey,
    /// `true` when the step followed the edge's own orientation (`edge.from -> edge.to`).
    pub forward: bool,
    /// Evidence type of the edge.
    pub evidence: EvidenceType,
    /// Resolution of the edge.
    pub resolution: Resolution,
}

impl Hop {
    /// Whether the edge is ambiguous or unresolved.
    pub fn is_flagged(&self) -> bool {
        self.resolution != Resolution::Resolved
    }
}

/// Weakest evidence on a path (`None` for an empty path).
pub(crate) fn weakest_evidence(hops: &[Hop]) -> Option<EvidenceType> {
    hops.iter().map(|h| h.evidence).max()
}

/// Number of ambiguous or unresolved hops on a path.
pub(crate) fn flagged_count(hops: &[Hop]) -> usize {
    hops.iter().filter(|h| h.is_flagged()).count()
}
