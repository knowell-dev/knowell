//! Shared fixture: a fictional multi-project system.
//!
//! ```text
//! web (InvoicePage.onCancel)
//!   --consumes--> POST /v1/subscriptions/{id}/cancel  <--exposes-- billing (CancelController.handle)
//!   billing: handle --calls--> SubscriptionService.cancel
//!   --produces--> subscription.cancelled  <--consumes-- worker (AuditConsumer.on_cancelled)
//!   worker --writes--> audit_log   <--reads-- billing (AuditReport.run)
//!   billing: service --writes--> subscriptions <--reads-- SubscriptionRepo.find
//!   web: onCancel --calls(heuristic)--> helper --calls(heuristic)--> service
//!   worker reads env AUDIT_DB_URL, declared by deploy/compose.yml
//!   web uses i18n key cancel.title, defined in en and tr locale files
//! ```
//!
//! The base fixture is clean: it produces no insights.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
#![allow(dead_code, unreachable_pub)]

use knowell_core::{ContentHash, LineRange, Name, RepoPath};
use knowell_graph::{
    ATTR_LOCALE, ATTR_SCHEMA_HASH, CodeGraph, ContractKind, Edge, EdgeKind, EvidenceRef,
    EvidenceType, GraphDelta, Node, NodeId, Resolution,
};

pub const ENDPOINT: &str = "POST /v1/subscriptions/{id}/cancel";
pub const TOPIC: &str = "subscription.cancelled";
pub const T_SUBS: &str = "subscriptions";
pub const T_AUDIT: &str = "audit_log";
pub const ENV: &str = "AUDIT_DB_URL";
pub const I18N: &str = "cancel.title";
pub const HASH_V1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub const HASH_V2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

pub fn name(value: &str) -> Name {
    Name::new(value).unwrap()
}

pub fn ev(project: &str, path: &str, line: u32) -> EvidenceRef {
    EvidenceRef {
        project: name(project),
        path: RepoPath::new(path).unwrap(),
        range: Some(LineRange::new(line, line + 2).unwrap()),
        content_hash: ContentHash::of(path.as_bytes()),
    }
}

pub fn sym(project: &str, key: &str) -> NodeId {
    NodeId::symbol(&name(project), key)
}

pub fn contract(kind: ContractKind, key: &str) -> NodeId {
    NodeId::contract(kind, key)
}

pub fn file(project: &str, path: &str) -> NodeId {
    NodeId::file(&name(project), &RepoPath::new(path).unwrap())
}

pub fn symbol_node(project: &str, key: &str, display: &str) -> Node {
    Node::symbol(&name(project), key, display).with_source(ev(project, "src/lib.rs", 10))
}

pub fn strong(kind: EdgeKind) -> Edge {
    Edge::resolved(kind)
}

pub fn derived(kind: EdgeKind) -> Edge {
    Edge::new(kind, EvidenceType::ContractDerived, Resolution::Resolved)
}

pub fn heuristic(kind: EdgeKind) -> Edge {
    Edge::new(kind, EvidenceType::Heuristic, Resolution::Resolved)
}

pub fn unresolved(kind: EdgeKind) -> Edge {
    Edge::new(kind, EvidenceType::Heuristic, Resolution::Unresolved)
}

/// Ids used across tests.
pub struct Ids {
    pub ui: NodeId,
    pub helper: NodeId,
    pub controller: NodeId,
    pub service: NodeId,
    pub repo: NodeId,
    pub report: NodeId,
    pub consumer: NodeId,
    pub endpoint: NodeId,
    pub topic: NodeId,
    pub subs: NodeId,
    pub audit: NodeId,
    pub env: NodeId,
    pub i18n: NodeId,
    pub billing_test: NodeId,
    pub web_test: NodeId,
}

pub fn ids() -> Ids {
    Ids {
        ui: sym("web", "InvoicePage.onCancel"),
        helper: sym("web", "formatters.cancelHelper"),
        controller: sym("billing", "CancelController.handle"),
        service: sym("billing", "SubscriptionService.cancel"),
        repo: sym("billing", "SubscriptionRepo.find"),
        report: sym("billing", "AuditReport.run"),
        consumer: sym("worker", "AuditConsumer.on_cancelled"),
        endpoint: contract(ContractKind::Endpoint, ENDPOINT),
        topic: contract(ContractKind::Topic, TOPIC),
        subs: contract(ContractKind::Table, T_SUBS),
        audit: contract(ContractKind::Table, T_AUDIT),
        env: contract(ContractKind::EnvName, ENV),
        i18n: contract(ContractKind::I18nKey, I18N),
        billing_test: NodeId::test(&name("billing"), "cancel_test"),
        web_test: NodeId::test(&name("web"), "cancel_ui_test"),
    }
}

fn maybe_reverse<T>(mut items: Vec<T>, reverse: bool) -> Vec<T> {
    if reverse {
        items.reverse();
    }
    items
}

/// The four project deltas in application order.
pub fn fixture_deltas(reverse: bool) -> Vec<GraphDelta> {
    let i = ids();
    let b = name("billing");
    let w = name("web");
    let k = name("worker");
    let d = name("deploy");

    let mut billing = GraphDelta::new(b.clone(), 1);
    billing.added_nodes = maybe_reverse(
        vec![
            Node::project(&b),
            symbol_node("billing", "CancelController.handle", "handle"),
            symbol_node("billing", "SubscriptionService.cancel", "cancel"),
            symbol_node("billing", "SubscriptionRepo.find", "find"),
            symbol_node("billing", "AuditReport.run", "run"),
            Node::test(&b, "cancel_test", "cancel_works"),
            Node::contract(ContractKind::Endpoint, ENDPOINT).with_attr(ATTR_SCHEMA_HASH, HASH_V1),
            Node::contract(ContractKind::Topic, TOPIC),
            Node::contract(ContractKind::Table, T_SUBS),
            Node::contract(ContractKind::Table, T_AUDIT),
        ],
        reverse,
    );
    let billing_edges = maybe_reverse(
        vec![
            (&i.controller, &i.endpoint, derived(EdgeKind::Exposes)),
            (&i.controller, &i.service, strong(EdgeKind::Calls)),
            (&i.service, &i.topic, derived(EdgeKind::Produces)),
            (&i.service, &i.subs, strong(EdgeKind::Writes)),
            (&i.service, &i.repo, strong(EdgeKind::Calls)),
            (&i.repo, &i.subs, strong(EdgeKind::Reads)),
            (&i.report, &i.audit, strong(EdgeKind::Reads)),
            (&i.billing_test, &i.service, strong(EdgeKind::Tests)),
        ],
        reverse,
    );
    for (from, to, edge) in billing_edges {
        billing = billing.add_edge(from, to, edge);
    }

    let mut web = GraphDelta::new(w.clone(), 1);
    web.added_nodes = maybe_reverse(
        vec![
            Node::project(&w),
            symbol_node("web", "InvoicePage.onCancel", "onCancel"),
            symbol_node("web", "formatters.cancelHelper", "helper"),
            Node::test(&w, "cancel_ui_test", "cancel_ui_works"),
            Node::file(&w, &RepoPath::new("src/i18n/en.json").unwrap())
                .with_attr(ATTR_LOCALE, "en"),
            Node::file(&w, &RepoPath::new("src/i18n/tr.json").unwrap())
                .with_attr(ATTR_LOCALE, "tr"),
            Node::contract(ContractKind::I18nKey, I18N),
        ],
        reverse,
    );
    let en = file("web", "src/i18n/en.json");
    let tr = file("web", "src/i18n/tr.json");
    let web_edges = maybe_reverse(
        vec![
            (
                &i.ui,
                &i.endpoint,
                derived(EdgeKind::Consumes)
                    .with_attr(ATTR_SCHEMA_HASH, HASH_V1)
                    .with_ref(ev("web", "src/api.ts", 12)),
            ),
            (&i.ui, &i.helper, heuristic(EdgeKind::Calls)),
            (&i.helper, &i.service, heuristic(EdgeKind::Calls)),
            (&i.ui, &i.i18n, strong(EdgeKind::References)),
            (&en, &i.i18n, strong(EdgeKind::Defines)),
            (&tr, &i.i18n, strong(EdgeKind::Defines)),
            (&i.web_test, &i.ui, strong(EdgeKind::Tests)),
        ],
        reverse,
    );
    for (from, to, edge) in web_edges {
        web = web.add_edge(from, to, edge);
    }

    let mut worker = GraphDelta::new(k.clone(), 1);
    worker.added_nodes = maybe_reverse(
        vec![
            Node::project(&k),
            symbol_node("worker", "AuditConsumer.on_cancelled", "on_cancelled"),
            Node::contract(ContractKind::EnvName, ENV),
        ],
        reverse,
    );
    let worker_edges = maybe_reverse(
        vec![
            (&i.consumer, &i.topic, derived(EdgeKind::Consumes)),
            (&i.consumer, &i.audit, strong(EdgeKind::Writes)),
            (
                &i.consumer,
                &i.env,
                Edge::new(
                    EdgeKind::Reads,
                    EvidenceType::Syntactic,
                    Resolution::Resolved,
                ),
            ),
        ],
        reverse,
    );
    for (from, to, edge) in worker_edges {
        worker = worker.add_edge(from, to, edge);
    }

    let compose = file("deploy", "compose.yml");
    let deploy = GraphDelta::new(d.clone(), 1)
        .add_node(Node::project(&d))
        .add_node(Node::file(&d, &RepoPath::new("compose.yml").unwrap()))
        .add_edge(&compose, &i.env, strong(EdgeKind::Defines));

    vec![billing, web, worker, deploy]
}

pub fn fixture() -> CodeGraph {
    fixture_with(false)
}

pub fn fixture_with(reverse: bool) -> CodeGraph {
    let mut graph = CodeGraph::new();
    for delta in fixture_deltas(reverse) {
        graph.apply(delta).unwrap();
    }
    graph
}
