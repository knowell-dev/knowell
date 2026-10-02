//! `know check`: graph insights plus link-specific checks, as findings with
//! stable codes, severities and source locations.
//!
//! | Code | Severity | Meaning |
//! |---|---|---|
//! | `link.migration_entity_mismatch` | error | an ORM entity misses a column a migration added to its table, maps a column no migration defines, or maps a dropped table |
//! | `link.endpoint_undocumented` | warning | a service exposes an endpoint its OpenAPI document does not list |
//! | `link.endpoint_unimplemented` | warning | an OpenAPI document lists an endpoint its service does not expose |
//! | `link.consumer_field_mismatch` | warning | an event consumer's payload type reads fields the event schema does not define |
//! | `link.endpoint_without_provider` | info | an endpoint is called but no indexed project exposes or documents it |
//! | `link.unresolved_reference` | info | a contract key is only known at run time; the use is kept as unresolved |
//! | `graph.*` | see knowell-graph | graph insights (contract drift, missing i18n keys, undeclared env names, ...) |
//!
//! Findings never contain source text or values, only identifiers.

use std::collections::{BTreeMap, BTreeSet};

use knowell_core::{LineRange, Name, RepoPath};
use knowell_graph::{
    ATTR_UNRESOLVED, CodeGraph, ContractKind, Direction, EdgeFilter, EdgeKind, EvidenceRef,
    EvidenceType, InsightConfig, Node, NodeId, NodeKind, Resolution,
};
use serde::{Deserialize, Serialize};

use crate::error::LinkError;
use crate::link::LinkOutput;
use crate::model::{ATTR_FIELDS, ATTR_VERSION, ProjectExtractions};
use crate::normalize::{endpoint_parts, fold_name};

/// How serious a finding is. Ordered `Info < Warning < Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Worth knowing; often intentional.
    Info,
    /// Probably a defect or a gap.
    Warning,
    /// A mismatch that breaks at run time.
    Error,
}

impl Severity {
    /// Stable lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

impl From<knowell_graph::Severity> for Severity {
    fn from(value: knowell_graph::Severity) -> Self {
        match value {
            knowell_graph::Severity::Info => Self::Info,
            knowell_graph::Severity::Warning => Self::Warning,
            knowell_graph::Severity::Error => Self::Error,
        }
    }
}

/// A source location of a finding.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Location {
    /// Project (one repository root, or a root inside a monorepo).
    pub project: Name,
    /// Path relative to the project root.
    pub path: RepoPath,
    /// 1-based inclusive lines, when narrower than the file.
    pub range: Option<LineRange>,
}

impl From<&EvidenceRef> for Location {
    fn from(value: &EvidenceRef) -> Self {
        Self {
            project: value.project.clone(),
            path: value.path.clone(),
            range: value.range,
        }
    }
}

/// One check result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Stable code (`link.migration_entity_mismatch`, `graph.contract_drift`).
    pub code: String,
    /// Severity of the check.
    pub severity: Severity,
    /// Explanation with identifiers only (never source text or values).
    pub message: String,
    /// The node the finding is about (usually a contract).
    pub subject: Option<NodeId>,
    /// Project the finding is attributed to.
    pub project: Option<Name>,
    /// Locations, primary first, then sorted.
    pub locations: Vec<Location>,
    /// Source references that justify the finding, sorted and unique.
    pub evidence: Vec<EvidenceRef>,
}

/// Static description of a finding code (SARIF rule metadata).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckRule {
    /// Stable code.
    pub code: &'static str,
    /// Default severity.
    pub severity: Severity,
    /// One-line summary.
    pub summary: &'static str,
    /// How to fix or silence it.
    pub help: &'static str,
}

/// Every finding code this crate can report, in a fixed order.
pub const CHECK_RULES: &[CheckRule] = &[
    CheckRule {
        code: "link.migration_entity_mismatch",
        severity: Severity::Error,
        summary: "ORM entity and migrations disagree about a table's columns",
        help: "Update the entity (or add the missing migration) so it maps every column added by migrations and no column that does not exist.",
    },
    CheckRule {
        code: "link.endpoint_undocumented",
        severity: Severity::Warning,
        summary: "Endpoint served but missing from the service's OpenAPI document",
        help: "Add the operation to the OpenAPI document that describes this service, or remove the route.",
    },
    CheckRule {
        code: "link.endpoint_unimplemented",
        severity: Severity::Warning,
        summary: "OpenAPI operation that the service does not serve",
        help: "Implement the route or remove the operation from the document.",
    },
    CheckRule {
        code: "link.consumer_field_mismatch",
        severity: Severity::Warning,
        summary: "Event consumer reads fields the event schema does not define",
        help: "Align the consumer's payload type with the event schema (or publish a new schema version).",
    },
    CheckRule {
        code: "link.endpoint_without_provider",
        severity: Severity::Info,
        summary: "Endpoint called but not exposed or documented by any indexed project",
        help: "Check the path for typos; mark endpoints of external services as external.",
    },
    CheckRule {
        code: "link.unresolved_reference",
        severity: Severity::Info,
        summary: "Contract key only known at run time",
        help: "Use a literal or a module-level constant for the key so it can be linked.",
    },
    CheckRule {
        code: "graph.endpoint_without_client",
        severity: Severity::Info,
        summary: "Endpoint with no client in the indexed projects",
        help: "Remove dead endpoints, or mark endpoints used from outside the workspace as external.",
    },
    CheckRule {
        code: "graph.topic_without_consumer",
        severity: Severity::Warning,
        summary: "Topic is produced but nobody consumes it",
        help: "Add the consumer, stop publishing, or mark the topic as external.",
    },
    CheckRule {
        code: "graph.topic_without_producer",
        severity: Severity::Warning,
        summary: "Topic is consumed (or defined) but nobody produces it",
        help: "Add the producer or remove the subscription; mark external topics as external.",
    },
    CheckRule {
        code: "graph.table_never_read",
        severity: Severity::Info,
        summary: "Table that no indexed code reads",
        help: "Drop the table or index the project that reads it.",
    },
    CheckRule {
        code: "graph.contract_drift",
        severity: Severity::Error,
        summary: "Consumer built against an older shape of the contract",
        help: "Update the consumer to the current schema of the contract.",
    },
    CheckRule {
        code: "graph.i18n_key_undefined",
        severity: Severity::Error,
        summary: "i18n key used but defined in no locale file",
        help: "Add the key to every locale file.",
    },
    CheckRule {
        code: "graph.i18n_key_missing_locale",
        severity: Severity::Error,
        summary: "i18n key missing from some locales",
        help: "Add the key to the listed locale files.",
    },
    CheckRule {
        code: "graph.i18n_key_unused",
        severity: Severity::Info,
        summary: "i18n key defined but never used",
        help: "Remove the key or use it.",
    },
    CheckRule {
        code: "graph.env_undeclared",
        severity: Severity::Warning,
        summary: "Environment name read in code but declared in no deployment config",
        help: "Declare the variable (by name) in Compose / Kubernetes, or remove the read.",
    },
];

/// Options of [`check`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckOptions {
    /// Graph insight options (required locales, enabled insight codes).
    pub insights: InsightConfig,
    /// Finding codes to report; empty reports all.
    pub codes: BTreeSet<String>,
}

fn rule(code: &str) -> Option<&'static CheckRule> {
    CHECK_RULES.iter().find(|r| r.code == code)
}

fn finding(
    code: &str,
    message: String,
    subject: Option<NodeId>,
    project: Option<Name>,
    primary: Vec<EvidenceRef>,
    evidence: Vec<EvidenceRef>,
) -> Finding {
    let severity = rule(code).map_or(Severity::Warning, |r| r.severity);
    finding_with(code, severity, message, subject, project, primary, evidence)
}

#[allow(clippy::too_many_arguments)]
fn finding_with(
    code: &str,
    severity: Severity,
    message: String,
    subject: Option<NodeId>,
    project: Option<Name>,
    primary: Vec<EvidenceRef>,
    mut evidence: Vec<EvidenceRef>,
) -> Finding {
    let mut locations: Vec<Location> = Vec::new();
    for reference in primary.iter().chain(evidence.iter()) {
        let location = Location::from(reference);
        if !locations.contains(&location) {
            locations.push(location);
        }
    }
    if let Some(rest) = locations.get_mut(1..) {
        rest.sort();
    }
    evidence.extend(primary);
    evidence.sort();
    evidence.dedup();
    Finding {
        code: code.to_owned(),
        severity,
        message,
        subject,
        project,
        locations,
        evidence,
    }
}

fn is_placeholder(graph: &CodeGraph, id: &NodeId) -> bool {
    graph.node(id).is_some_and(Node::is_unresolved_placeholder)
}

fn incoming<'g>(
    graph: &'g CodeGraph,
    id: &NodeId,
    kind: EdgeKind,
) -> Vec<knowell_graph::Neighbor<'g>> {
    graph
        .neighbors(
            id,
            Direction::Incoming,
            &EdgeFilter::any().with_kinds([kind]),
        )
        .unwrap_or_default()
}

/// Runs the graph insights and the link checks.
///
/// # Errors
/// [`LinkError::Graph`] if the link output cannot be loaded into a graph.
pub fn check(
    projects: &[ProjectExtractions],
    linked: &LinkOutput,
    options: &CheckOptions,
) -> Result<Vec<Finding>, LinkError> {
    let graph = linked.graph()?;
    let mut out = Vec::new();

    for insight in graph.insights(&options.insights) {
        if is_placeholder(&graph, &insight.subject) {
            continue;
        }
        let primary: Vec<EvidenceRef> = insight
            .evidence
            .iter()
            .filter(|r| Some(&r.project) == insight.project.as_ref())
            .take(1)
            .cloned()
            .collect();
        out.push(finding_with(
            insight.code.as_str(),
            insight.severity.into(),
            insight.message.clone(),
            Some(insight.subject.clone()),
            insight.project.clone(),
            primary,
            insight.evidence.clone(),
        ));
    }
    migration_entity_mismatch(linked, &mut out);
    endpoint_documentation(&graph, &mut out);
    consumer_fields(&graph, projects, &mut out);
    endpoints_without_provider(&graph, &mut out);
    unresolved_references(&graph, &mut out);

    if !options.codes.is_empty() {
        out.retain(|f| options.codes.contains(&f.code));
    }
    out.sort_by(|a, b| {
        (
            &a.code,
            &a.project,
            &a.subject,
            a.locations.first(),
            &a.message,
        )
            .cmp(&(
                &b.code,
                &b.project,
                &b.subject,
                b.locations.first(),
                &b.message,
            ))
    });
    out.dedup();
    Ok(out)
}

fn entity_label(mapping: &crate::link::EntityMapping) -> String {
    mapping
        .symbol
        .as_ref()
        .map_or_else(|| mapping.path.to_string(), |s| s.qualified_name.clone())
}

fn migration_entity_mismatch(linked: &LinkOutput, out: &mut Vec<Finding>) {
    for mapping in linked.entities() {
        let Some(schema) = linked.tables().get(&mapping.table) else {
            continue;
        };
        if mapping.columns.is_empty() {
            continue;
        }
        let subject = NodeId::contract(ContractKind::Table, &mapping.table);
        let label = entity_label(mapping);
        if schema.dropped {
            out.push(finding(
                "link.migration_entity_mismatch",
                format!(
                    "`{label}` maps table `{}`, which the migrations drop",
                    mapping.table
                ),
                Some(subject),
                Some(mapping.project.clone()),
                vec![mapping.evidence.clone()],
                Vec::new(),
            ));
            continue;
        }
        let current = schema.columns();
        let unknown: Vec<&String> = mapping.columns.difference(&current).collect();
        let missing: Vec<(&String, &EvidenceRef)> = schema
            .added
            .iter()
            .filter(|(column, _)| current.contains(*column) && !mapping.columns.contains(*column))
            .collect();
        if unknown.is_empty() && missing.is_empty() {
            continue;
        }
        let mut parts = Vec::new();
        if !missing.is_empty() {
            let mut files: Vec<String> = missing.iter().map(|(_, r)| r.path.to_string()).collect();
            files.sort();
            files.dedup();
            parts.push(format!(
                "misses column(s) {} added by {}",
                missing
                    .iter()
                    .map(|(c, _)| format!("`{c}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
                files.join(", ")
            ));
        }
        if !unknown.is_empty() {
            parts.push(format!(
                "maps column(s) {} that no migration defines",
                unknown
                    .iter()
                    .map(|c| format!("`{c}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        out.push(finding(
            "link.migration_entity_mismatch",
            format!(
                "`{label}` in {} maps table `{}` but {}",
                mapping.project,
                mapping.table,
                parts.join("; ")
            ),
            Some(subject),
            Some(mapping.project.clone()),
            vec![mapping.evidence.clone()],
            missing.iter().map(|(_, r)| (*r).clone()).collect(),
        ));
    }
}

/// `(method, path)` of an endpoint id; `None` for other nodes.
fn endpoint_of(node: &Node) -> Option<(String, String)> {
    match &node.kind {
        NodeKind::Contract {
            kind: ContractKind::Endpoint,
            key,
        } => {
            let (method, _) = endpoint_parts(key);
            let path = key
                .split_once(' ')
                .map_or(key.as_str(), |(_, p)| p)
                .to_owned();
            Some((method, path))
        }
        _ => None,
    }
}

fn methods_compatible(a: &str, b: &str) -> bool {
    a == b || a == "*" || b == "*"
}

fn endpoint_documentation(graph: &CodeGraph, out: &mut Vec<Finding>) {
    // Documents: (project, path) -> endpoints (method, path, id, evidence).
    type Endpoint = (String, String, NodeId, Vec<EvidenceRef>);
    let mut documents: BTreeMap<(Name, RepoPath), Vec<Endpoint>> = BTreeMap::new();
    let mut providers: BTreeMap<Name, Vec<Endpoint>> = BTreeMap::new();
    for node in graph.nodes() {
        let Some((method, path)) = endpoint_of(node) else {
            continue;
        };
        if node.is_unresolved_placeholder()
            || node.attr(knowell_graph::ATTR_EXTERNAL) == Some("true")
        {
            continue;
        }
        for nb in incoming(graph, &node.id, EdgeKind::Defines) {
            if nb.edge.evidence != EvidenceType::ContractDerived {
                continue;
            }
            let (Some(project), NodeKind::File) = (nb.node.project.clone(), &nb.node.kind) else {
                continue;
            };
            let Some(path_of_doc) = nb.edge.evidence_refs.first().map(|r| r.path.clone()) else {
                continue;
            };
            documents.entry((project, path_of_doc)).or_default().push((
                method.clone(),
                path.clone(),
                node.id.clone(),
                nb.edge.evidence_refs.clone(),
            ));
        }
        for nb in incoming(graph, &node.id, EdgeKind::Exposes) {
            if nb.edge.resolution == Resolution::Unresolved {
                continue;
            }
            let Some(project) = nb.node.project.clone() else {
                continue;
            };
            providers.entry(project).or_default().push((
                method.clone(),
                path.clone(),
                node.id.clone(),
                nb.edge.evidence_refs.clone(),
            ));
        }
    }
    let documented_in = |doc: &[Endpoint], method: &str, path: &str| {
        doc.iter()
            .any(|(m, p, _, _)| p == path && methods_compatible(m, method))
    };
    // A document covers a provider when they share an endpoint, or when the
    // document is named after the provider project.
    let covers = |(doc_key, doc): (&(Name, RepoPath), &Vec<Endpoint>),
                  provider: &Name,
                  served: &[Endpoint]| {
        let stem = doc_key
            .1
            .file_name()
            .split('.')
            .next()
            .unwrap_or("")
            .to_owned();
        stem == provider.as_str() || served.iter().any(|(m, p, _, _)| documented_in(doc, m, p))
    };
    for (provider, served) in &providers {
        let covering: Vec<(&(Name, RepoPath), &Vec<Endpoint>)> = documents
            .iter()
            .filter(|entry| covers(*entry, provider, served))
            .collect();
        if covering.is_empty() {
            continue;
        }
        let mut reported = BTreeSet::new();
        for (method, path, id, refs) in served {
            if covering
                .iter()
                .any(|(_, doc)| documented_in(doc, method, path))
                || !reported.insert(id)
            {
                continue;
            }
            let docs: Vec<String> = covering
                .iter()
                .map(|((p, path), _)| format!("{p}/{path}"))
                .collect();
            let doc_refs: Vec<EvidenceRef> = covering
                .iter()
                .filter_map(|(_, doc)| doc.first().and_then(|(_, _, _, r)| r.first().cloned()))
                .collect();
            out.push(finding(
                "link.endpoint_undocumented",
                format!(
                    "{provider} serves `{method} {path}` but {} does not document it",
                    docs.join(", ")
                ),
                Some(id.clone()),
                Some(provider.clone()),
                refs.iter().take(1).cloned().collect(),
                doc_refs,
            ));
        }
    }
    for (doc_key, doc) in &documents {
        let covered: Vec<(&Name, &Vec<Endpoint>)> = providers
            .iter()
            .filter(|(provider, served)| covers((doc_key, doc), provider, served))
            .collect();
        if covered.is_empty() {
            continue;
        }
        for (method, path, id, refs) in doc {
            let served = covered.iter().any(|(_, served)| {
                served
                    .iter()
                    .any(|(m, p, _, _)| p == path && methods_compatible(m, method))
            });
            if served {
                continue;
            }
            let names: Vec<String> = covered.iter().map(|(p, _)| p.to_string()).collect();
            out.push(finding(
                "link.endpoint_unimplemented",
                format!(
                    "{}/{} documents `{method} {path}` but {} does not serve it",
                    doc_key.0,
                    doc_key.1,
                    names.join(", ")
                ),
                Some(id.clone()),
                Some(doc_key.0.clone()),
                refs.iter().take(1).cloned().collect(),
                Vec::new(),
            ));
        }
    }
}

fn consumer_fields(graph: &CodeGraph, projects: &[ProjectExtractions], out: &mut Vec<Finding>) {
    for node in graph.nodes() {
        let Some(ContractKind::Topic) = node.contract_kind() else {
            continue;
        };
        let Some(fields) = node.attr(ATTR_FIELDS) else {
            continue;
        };
        let schema: BTreeSet<String> = fields.split(',').map(fold_name).collect();
        let topic = fold_name(&node.name);
        let names: BTreeSet<String> = ["", "event", "payload", "message", "data", "v1", "v2", "v3"]
            .iter()
            .map(|suffix| format!("{topic}{suffix}"))
            .collect();
        let consumers: BTreeSet<Name> = incoming(graph, &node.id, EdgeKind::Consumes)
            .iter()
            .filter(|nb| nb.edge.resolution != Resolution::Unresolved)
            .filter_map(|nb| nb.node.project.clone())
            .collect();
        for project in projects.iter().filter(|p| consumers.contains(&p.project)) {
            for shape in &project.shapes {
                let short = shape
                    .symbol
                    .qualified_name
                    .rsplit(['.', ':'])
                    .next()
                    .unwrap_or(&shape.symbol.qualified_name);
                if !names.contains(&fold_name(short)) {
                    continue;
                }
                let unknown: Vec<&String> = shape
                    .fields
                    .iter()
                    .filter(|f| !schema.contains(&fold_name(f)))
                    .collect();
                if unknown.is_empty() {
                    continue;
                }
                let version = node
                    .attr(ATTR_VERSION)
                    .map(|v| format!(" v{v}"))
                    .unwrap_or_default();
                let site = EvidenceRef {
                    project: project.project.clone(),
                    path: shape.path.clone(),
                    range: Some(shape.symbol.range),
                    content_hash: shape.content_hash,
                };
                out.push(finding(
                    "link.consumer_field_mismatch",
                    format!(
                        "`{}` in {} reads field(s) {} that the `{}`{version} schema does not define",
                        shape.symbol.qualified_name,
                        project.project,
                        unknown
                            .iter()
                            .map(|f| format!("`{f}`"))
                            .collect::<Vec<_>>()
                            .join(", "),
                        node.name
                    ),
                    Some(node.id.clone()),
                    Some(project.project.clone()),
                    vec![site],
                    node.source.iter().cloned().collect(),
                ));
            }
        }
    }
}

fn endpoints_without_provider(graph: &CodeGraph, out: &mut Vec<Finding>) {
    for node in graph.nodes() {
        if endpoint_of(node).is_none()
            || node.is_unresolved_placeholder()
            || node.attr(knowell_graph::ATTR_EXTERNAL) == Some("true")
        {
            continue;
        }
        let consumers = incoming(graph, &node.id, EdgeKind::Consumes);
        if consumers.is_empty()
            || !incoming(graph, &node.id, EdgeKind::Exposes).is_empty()
            || !incoming(graph, &node.id, EdgeKind::Defines).is_empty()
        {
            continue;
        }
        let mut refs: Vec<EvidenceRef> = consumers
            .iter()
            .flat_map(|nb| nb.edge.evidence_refs.iter().cloned())
            .collect();
        refs.sort();
        let project = refs.first().map(|r| r.project.clone());
        out.push(finding(
            "link.endpoint_without_provider",
            format!(
                "`{}` is called but no indexed project exposes or documents it",
                node.name
            ),
            Some(node.id.clone()),
            project,
            refs.iter().take(1).cloned().collect(),
            refs,
        ));
    }
}

/// Article and readable name of a contract kind.
fn kind_label(kind: Option<ContractKind>) -> (&'static str, &'static str) {
    match kind {
        Some(ContractKind::Endpoint) => ("an", "endpoint"),
        Some(ContractKind::Topic) => ("a", "topic"),
        Some(ContractKind::Rpc) => ("an", "RPC"),
        Some(ContractKind::Table) => ("a", "table"),
        Some(ContractKind::EnvName) => ("an", "environment name"),
        Some(ContractKind::I18nKey) => ("an", "i18n key"),
        Some(ContractKind::Package) => ("a", "package"),
        Some(ContractKind::Infra) => ("an", "infrastructure contract"),
        None => ("a", "contract"),
    }
}

fn unresolved_references(graph: &CodeGraph, out: &mut Vec<Finding>) {
    for (key, edge) in graph.edges() {
        if edge.resolution != Resolution::Unresolved {
            continue;
        }
        let Some(target) = graph.node(&key.to) else {
            continue;
        };
        if target.attr(ATTR_UNRESOLVED) != Some("true") {
            continue;
        }
        let source = graph.node(&key.from);
        let source_name = source.map_or_else(|| key.from.to_string(), |n| n.name.clone());
        let (article, kind) = kind_label(target.contract_kind());
        // `POST {}` is fully dynamic; `POST /v1/x/{}/y` is a route pattern
        // with a run-time part that matched no known contract.
        let literal_part = match target.contract_kind() {
            Some(ContractKind::Endpoint) => target
                .name
                .split_once(' ')
                .map_or(target.name.as_str(), |(_, path)| path),
            _ => target.name.as_str(),
        };
        let fully_dynamic = !literal_part
            .replace("{}", "")
            .chars()
            .any(char::is_alphanumeric);
        let message = if fully_dynamic {
            format!(
                "`{source_name}` uses {article} {kind} whose key is only known at run time (`{}`)",
                target.name
            )
        } else {
            format!(
                "`{source_name}` uses {kind} `{}`, built at run time, which matches no known contract",
                target.name
            )
        };
        out.push(finding(
            "link.unresolved_reference",
            message,
            Some(key.to.clone()),
            source.and_then(|n| n.project.clone()),
            edge.evidence_refs.iter().take(1).cloned().collect(),
            edge.evidence_refs.clone(),
        ));
    }
}
