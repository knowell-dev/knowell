//! Graph insights: deterministic checks over the contract graph, the inputs
//! of `know check`.
//!
//! Every insight has a stable [`InsightCode`], a [`Severity`], a subject node
//! and the edges and source references that justify it. Insights are only as
//! complete as the index: they assume every project that uses a contract is
//! indexed, so a contract used from outside the workspace should carry
//! [`ATTR_EXTERNAL`] (`"true"`) to be skipped.
//!
//! Conventions the checks rely on (edge `a --kind--> b`):
//!
//! | Contract | Provider side | Consumer side |
//! |---|---|---|
//! | endpoint | `Exposes` | `Consumes` |
//! | topic | `Produces` | `Consumes` |
//! | table | `Writes` | `Reads` |
//! | env name | `Defines` (deployment config declares it) | `Reads` |
//! | i18n key | `Defines` (a locale file, node attr `locale`) | `References`, `Reads`, `Consumes` |

use std::collections::BTreeSet;

use knowell_core::Name;

use crate::graph::{CodeGraph, Neighbor};
use crate::model::{
    ATTR_EXTERNAL, ATTR_LOCALE, ATTR_SCHEMA_HASH, ContractKind, Direction, EdgeKey, EdgeKind,
    EvidenceRef, Node, NodeId,
};

/// Stable identifier of a check. The text form (`graph.*`) is part of the
/// public contract: CI configuration and baselines refer to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum InsightCode {
    /// `graph.endpoint_without_client`: an endpoint nobody calls.
    EndpointWithoutClient,
    /// `graph.topic_without_consumer`: a topic nobody subscribes to.
    TopicWithoutConsumer,
    /// `graph.topic_without_producer`: a topic nobody publishes to.
    TopicWithoutProducer,
    /// `graph.table_never_read`: a table that is never read.
    TableNeverRead,
    /// `graph.contract_drift`: a consumer built against another schema hash.
    ContractDrift,
    /// `graph.i18n_key_undefined`: a key used but defined in no locale file.
    I18nKeyUndefined,
    /// `graph.i18n_key_missing_locale`: a used key missing from some locale.
    I18nKeyMissingLocale,
    /// `graph.i18n_key_unused`: a key defined but never used.
    I18nKeyUnused,
    /// `graph.env_undeclared`: an env name read in code but declared nowhere.
    EnvUndeclared,
}

impl InsightCode {
    /// Every code, in a fixed order.
    pub const ALL: [InsightCode; 9] = [
        Self::EndpointWithoutClient,
        Self::TopicWithoutConsumer,
        Self::TopicWithoutProducer,
        Self::TableNeverRead,
        Self::ContractDrift,
        Self::I18nKeyUndefined,
        Self::I18nKeyMissingLocale,
        Self::I18nKeyUnused,
        Self::EnvUndeclared,
    ];

    /// The stable text code.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EndpointWithoutClient => "graph.endpoint_without_client",
            Self::TopicWithoutConsumer => "graph.topic_without_consumer",
            Self::TopicWithoutProducer => "graph.topic_without_producer",
            Self::TableNeverRead => "graph.table_never_read",
            Self::ContractDrift => "graph.contract_drift",
            Self::I18nKeyUndefined => "graph.i18n_key_undefined",
            Self::I18nKeyMissingLocale => "graph.i18n_key_missing_locale",
            Self::I18nKeyUnused => "graph.i18n_key_unused",
            Self::EnvUndeclared => "graph.env_undeclared",
        }
    }

    /// The severity this check reports.
    pub fn severity(self) -> Severity {
        match self {
            Self::EndpointWithoutClient | Self::TableNeverRead | Self::I18nKeyUnused => {
                Severity::Info
            }
            Self::TopicWithoutConsumer | Self::TopicWithoutProducer | Self::EnvUndeclared => {
                Severity::Warning
            }
            Self::ContractDrift | Self::I18nKeyUndefined | Self::I18nKeyMissingLocale => {
                Severity::Error
            }
        }
    }
}

/// How serious an insight is. Ordered `Info < Warning < Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Severity {
    /// Worth knowing; often intentional.
    Info,
    /// Probably a defect or dead code.
    Warning,
    /// A mismatch that will break at run time.
    Error,
}

/// One finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Insight {
    /// Stable check code.
    pub code: InsightCode,
    /// Severity of the check.
    pub severity: Severity,
    /// The node the finding is about (usually a contract).
    pub subject: NodeId,
    /// Project the finding is attributed to (the offending consumer or
    /// provider), if one project is responsible.
    pub project: Option<Name>,
    /// Human-readable explanation. Contains identifiers only, never source text.
    pub message: String,
    /// Other nodes involved (consumers, definers), sorted.
    pub related: Vec<NodeId>,
    /// Edges that justify the finding, sorted.
    pub edges: Vec<EdgeKey>,
    /// Source locations of the subject and the justifying edges, sorted and unique.
    pub evidence: Vec<EvidenceRef>,
}

/// Options for [`CodeGraph::insights`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InsightConfig {
    /// Locales every i18n key must exist in. `None` uses the union of the
    /// locales found on locale files in the graph.
    pub required_locales: Option<BTreeSet<String>>,
    /// Checks to run; empty runs all of them.
    pub codes: BTreeSet<InsightCode>,
}

impl InsightConfig {
    fn enabled(&self, code: InsightCode) -> bool {
        self.codes.is_empty() || self.codes.contains(&code)
    }
}

/// Edges into a contract with the given kinds. A missing node cannot happen
/// for ids taken from the graph itself, so an error yields no edges.
fn incoming<'a>(graph: &'a CodeGraph, node: &Node, kinds: &[EdgeKind]) -> Vec<Neighbor<'a>> {
    let mut all = graph
        .incident(&node.id, Direction::Incoming)
        .unwrap_or_default();
    all.retain(|n| kinds.contains(&n.edge.kind));
    all
}

fn finish(
    code: InsightCode,
    subject: &Node,
    project: Option<Name>,
    message: String,
    support: &[Neighbor<'_>],
    related: &[Neighbor<'_>],
) -> Insight {
    let mut edges: Vec<EdgeKey> = support.iter().map(Neighbor::key).collect();
    edges.sort();
    edges.dedup();
    let mut related_ids: Vec<NodeId> = related.iter().map(|n| n.node.id.clone()).collect();
    related_ids.sort();
    related_ids.dedup();
    let mut evidence: Vec<EvidenceRef> = subject.source.iter().cloned().collect();
    for nb in support {
        evidence.extend(nb.edge.evidence_refs.iter().cloned());
    }
    evidence.sort();
    evidence.dedup();
    Insight {
        code,
        severity: code.severity(),
        subject: subject.id.clone(),
        project,
        message,
        related: related_ids,
        edges,
        evidence,
    }
}

/// Project of the first (lowest id) neighbor that has one.
fn first_project(nbs: &[Neighbor<'_>]) -> Option<Name> {
    let mut sorted: Vec<&Neighbor<'_>> = nbs.iter().collect();
    sorted.sort_by(|a, b| a.node.id.cmp(&b.node.id));
    sorted.iter().find_map(|n| n.node.project.clone())
}

fn short(hash: &str) -> String {
    hash.chars().take(12).collect()
}

impl CodeGraph {
    /// Runs the graph insights and returns the findings sorted by code,
    /// subject, project and message. Contracts carrying [`ATTR_EXTERNAL`]
    /// are skipped.
    pub fn insights(&self, config: &InsightConfig) -> Vec<Insight> {
        let mut out = Vec::new();
        let locales = self.required_locales(config);
        for node in self.nodes() {
            let Some(kind) = node.contract_kind() else {
                continue;
            };
            if node.attr(ATTR_EXTERNAL) == Some("true") {
                continue;
            }
            match kind {
                ContractKind::Endpoint => self.check_endpoint(node, config, &mut out),
                ContractKind::Topic => self.check_topic(node, config, &mut out),
                ContractKind::Table => self.check_table(node, config, &mut out),
                ContractKind::EnvName => self.check_env(node, config, &mut out),
                ContractKind::I18nKey => self.check_i18n(node, config, &locales, &mut out),
                ContractKind::Rpc | ContractKind::Package | ContractKind::Infra => {}
            }
            if config.enabled(InsightCode::ContractDrift) {
                self.check_drift(node, &mut out);
            }
        }
        out.sort_by(|a, b| {
            (a.code.as_str(), &a.subject, &a.project, &a.message).cmp(&(
                b.code.as_str(),
                &b.subject,
                &b.project,
                &b.message,
            ))
        });
        out
    }

    fn check_endpoint(&self, node: &Node, config: &InsightConfig, out: &mut Vec<Insight>) {
        let code = InsightCode::EndpointWithoutClient;
        if !config.enabled(code) || !incoming(self, node, &[EdgeKind::Consumes]).is_empty() {
            return;
        }
        let providers = incoming(self, node, &[EdgeKind::Exposes]);
        let message = format!(
            "endpoint `{}` has no client in the indexed projects",
            node.name
        );
        out.push(finish(
            code,
            node,
            first_project(&providers),
            message,
            &providers,
            &providers,
        ));
    }

    fn check_topic(&self, node: &Node, config: &InsightConfig, out: &mut Vec<Insight>) {
        let producers = incoming(self, node, &[EdgeKind::Produces]);
        let consumers = incoming(self, node, &[EdgeKind::Consumes]);
        let no_consumer = InsightCode::TopicWithoutConsumer;
        if config.enabled(no_consumer) && consumers.is_empty() {
            let message = format!("topic `{}` is produced but has no consumer", node.name);
            out.push(finish(
                no_consumer,
                node,
                first_project(&producers),
                message,
                &producers,
                &producers,
            ));
        }
        let no_producer = InsightCode::TopicWithoutProducer;
        if config.enabled(no_producer) && producers.is_empty() {
            let message = format!("topic `{}` is consumed but has no producer", node.name);
            out.push(finish(
                no_producer,
                node,
                first_project(&consumers),
                message,
                &consumers,
                &consumers,
            ));
        }
    }

    fn check_table(&self, node: &Node, config: &InsightConfig, out: &mut Vec<Insight>) {
        let code = InsightCode::TableNeverRead;
        if !config.enabled(code) || !incoming(self, node, &[EdgeKind::Reads]).is_empty() {
            return;
        }
        let writers = incoming(self, node, &[EdgeKind::Writes]);
        let message = format!("table `{}` is never read", node.name);
        out.push(finish(
            code,
            node,
            first_project(&writers),
            message,
            &writers,
            &writers,
        ));
    }

    fn check_env(&self, node: &Node, config: &InsightConfig, out: &mut Vec<Insight>) {
        let code = InsightCode::EnvUndeclared;
        if !config.enabled(code) || !incoming(self, node, &[EdgeKind::Defines]).is_empty() {
            return;
        }
        let readers = incoming(self, node, &[EdgeKind::Reads]);
        if readers.is_empty() {
            return;
        }
        let message = format!(
            "environment name `{}` is read in code but declared in no deployment config",
            node.name
        );
        out.push(finish(
            code,
            node,
            first_project(&readers),
            message,
            &readers,
            &readers,
        ));
    }

    /// Consumers whose recorded schema hash differs from the contract's.
    /// A missing hash on either side is "unknown", never drift.
    fn check_drift(&self, node: &Node, out: &mut Vec<Insight>) {
        let Some(current) = node.attr(ATTR_SCHEMA_HASH) else {
            return;
        };
        for nb in incoming(self, node, &[EdgeKind::Consumes, EdgeKind::Reads]) {
            let Some(expected) = nb.edge.attr(ATTR_SCHEMA_HASH) else {
                continue;
            };
            if expected == current {
                continue;
            }
            let message = format!(
                "`{}` was built against schema {} of `{}` but the contract is at {}",
                nb.node.name,
                short(expected),
                node.name,
                short(current)
            );
            out.push(finish(
                InsightCode::ContractDrift,
                node,
                nb.node.project.clone(),
                message,
                std::slice::from_ref(&nb),
                std::slice::from_ref(&nb),
            ));
        }
    }

    fn required_locales(&self, config: &InsightConfig) -> BTreeSet<String> {
        if let Some(explicit) = &config.required_locales {
            return explicit.clone();
        }
        let mut found = BTreeSet::new();
        for edge_key in self.edges().filter(|(k, _)| k.kind == EdgeKind::Defines) {
            let defines_i18n = self
                .node(&edge_key.0.to)
                .is_some_and(|n| n.contract_kind() == Some(ContractKind::I18nKey));
            if !defines_i18n {
                continue;
            }
            if let Some(locale) = self
                .node(&edge_key.0.from)
                .and_then(|n| n.attr(ATTR_LOCALE))
            {
                found.insert(locale.to_owned());
            }
        }
        found
    }

    fn check_i18n(
        &self,
        node: &Node,
        config: &InsightConfig,
        required: &BTreeSet<String>,
        out: &mut Vec<Insight>,
    ) {
        let definers = incoming(self, node, &[EdgeKind::Defines]);
        let users = incoming(
            self,
            node,
            &[EdgeKind::References, EdgeKind::Reads, EdgeKind::Consumes],
        );

        if users.is_empty() {
            let code = InsightCode::I18nKeyUnused;
            if config.enabled(code) && !definers.is_empty() {
                let message = format!("i18n key `{}` is defined but never used", node.name);
                out.push(finish(
                    code,
                    node,
                    first_project(&definers),
                    message,
                    &definers,
                    &definers,
                ));
            }
            return;
        }

        if definers.is_empty() {
            let code = InsightCode::I18nKeyUndefined;
            if config.enabled(code) {
                let message = format!(
                    "i18n key `{}` is used but defined in no locale file",
                    node.name
                );
                out.push(finish(
                    code,
                    node,
                    first_project(&users),
                    message,
                    &users,
                    &users,
                ));
            }
            return;
        }

        let code = InsightCode::I18nKeyMissingLocale;
        if !config.enabled(code) {
            return;
        }
        let defined: BTreeSet<&str> = definers
            .iter()
            .filter_map(|d| d.node.attr(ATTR_LOCALE))
            .collect();
        if defined.is_empty() {
            // No locale information on the definers: cannot tell what is missing.
            return;
        }
        let missing: Vec<&str> = required
            .iter()
            .map(String::as_str)
            .filter(|l| !defined.contains(l))
            .collect();
        if missing.is_empty() {
            return;
        }
        let message = format!(
            "i18n key `{}` is used but missing in locale(s): {}",
            node.name,
            missing.join(", ")
        );
        out.push(finish(
            code,
            node,
            first_project(&users),
            message,
            &users,
            &definers,
        ));
    }
}
