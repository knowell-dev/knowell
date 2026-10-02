//! Graph insights: each code has a positive and a negative case.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::collections::BTreeSet;

use common::*;
use knowell_core::RepoPath;
use knowell_graph::{
    ATTR_EXTERNAL, ATTR_LOCALE, ATTR_SCHEMA_HASH, CodeGraph, ContractKind, EdgeKind, GraphDelta,
    Insight, InsightCode, InsightConfig, Node, Severity,
};
use pretty_assertions::assert_eq;

fn run(graph: &CodeGraph) -> Vec<Insight> {
    graph.insights(&InsightConfig::default())
}

fn codes(insights: &[Insight]) -> Vec<&'static str> {
    insights.iter().map(|i| i.code.as_str()).collect()
}

fn billing(graph: &mut CodeGraph, f: impl FnOnce(GraphDelta) -> GraphDelta) {
    let next = graph.generation(&name("billing")) + 1;
    graph
        .apply(f(GraphDelta::new(name("billing"), next)))
        .unwrap();
}

fn web(graph: &mut CodeGraph, f: impl FnOnce(GraphDelta) -> GraphDelta) {
    let next = graph.generation(&name("web")) + 1;
    graph.apply(f(GraphDelta::new(name("web"), next))).unwrap();
}

fn worker(graph: &mut CodeGraph, f: impl FnOnce(GraphDelta) -> GraphDelta) {
    let next = graph.generation(&name("worker")) + 1;
    graph
        .apply(f(GraphDelta::new(name("worker"), next)))
        .unwrap();
}

#[test]
fn the_base_fixture_is_clean() {
    assert_eq!(run(&fixture()), Vec::<Insight>::new());
}

#[test]
fn code_strings_and_severities_are_stable() {
    let texts: Vec<&str> = InsightCode::ALL.iter().map(|c| c.as_str()).collect();
    assert_eq!(
        texts,
        [
            "graph.endpoint_without_client",
            "graph.topic_without_consumer",
            "graph.topic_without_producer",
            "graph.table_never_read",
            "graph.contract_drift",
            "graph.i18n_key_undefined",
            "graph.i18n_key_missing_locale",
            "graph.i18n_key_unused",
            "graph.env_undeclared",
        ]
    );
    let unique: BTreeSet<&str> = texts.iter().copied().collect();
    assert_eq!(unique.len(), texts.len());
    assert_eq!(InsightCode::ContractDrift.severity(), Severity::Error);
    assert_eq!(
        InsightCode::TopicWithoutConsumer.severity(),
        Severity::Warning
    );
    assert_eq!(InsightCode::TableNeverRead.severity(), Severity::Info);
    assert!(Severity::Info < Severity::Warning && Severity::Warning < Severity::Error);
}

#[test]
fn endpoint_without_client() {
    let mut graph = fixture();
    let i = ids();
    let legacy = contract(ContractKind::Endpoint, "GET /v1/legacy");
    billing(&mut graph, |d| {
        d.add_node(Node::contract(ContractKind::Endpoint, "GET /v1/legacy"))
            .add_edge(
                &i.controller,
                &legacy,
                derived(EdgeKind::Exposes).with_ref(ev("billing", "src/routes.rs", 7)),
            )
    });
    let found = run(&graph);
    assert_eq!(codes(&found), ["graph.endpoint_without_client"]);
    let insight = &found[0];
    assert_eq!(insight.subject, legacy);
    assert_eq!(insight.severity, Severity::Info);
    assert_eq!(insight.project, Some(name("billing")));
    assert_eq!(insight.related, std::slice::from_ref(&i.controller));
    assert_eq!(insight.edges.len(), 1);
    assert_eq!(insight.evidence, [ev("billing", "src/routes.rs", 7)]);
    assert!(insight.message.contains("GET /v1/legacy"));
}

#[test]
fn endpoint_with_any_client_or_marked_external_is_not_reported() {
    let mut graph = fixture();
    let i = ids();
    let dynamic = contract(ContractKind::Endpoint, "GET /v1/dynamic");
    let public = contract(ContractKind::Endpoint, "GET /v1/public");
    billing(&mut graph, |d| {
        d.add_node(Node::contract(ContractKind::Endpoint, "GET /v1/dynamic"))
            .add_node(
                Node::contract(ContractKind::Endpoint, "GET /v1/public")
                    .with_attr(ATTR_EXTERNAL, "true"),
            )
            .add_edge(&i.controller, &dynamic, derived(EdgeKind::Exposes))
            .add_edge(&i.controller, &public, derived(EdgeKind::Exposes))
    });
    // An unresolved client edge still counts: the endpoint may be used.
    web(&mut graph, |d| {
        d.add_edge(&i.ui, &dynamic, unresolved(EdgeKind::Consumes))
    });
    assert_eq!(run(&graph), Vec::<Insight>::new());
}

#[test]
fn topic_without_consumer() {
    let mut graph = fixture();
    let i = ids();
    let paid = contract(ContractKind::Topic, "invoice.paid");
    billing(&mut graph, |d| {
        d.add_node(Node::contract(ContractKind::Topic, "invoice.paid"))
            .add_edge(&i.service, &paid, derived(EdgeKind::Produces))
    });
    let found = run(&graph);
    assert_eq!(codes(&found), ["graph.topic_without_consumer"]);
    assert_eq!(found[0].subject, paid);
    assert_eq!(found[0].severity, Severity::Warning);
    assert_eq!(found[0].project, Some(name("billing")));
}

#[test]
fn topic_without_producer() {
    let mut graph = fixture();
    let i = ids();
    let refund = contract(ContractKind::Topic, "refund.requested");
    worker(&mut graph, |d| {
        d.add_node(Node::contract(ContractKind::Topic, "refund.requested"))
            .add_edge(&i.consumer, &refund, derived(EdgeKind::Consumes))
    });
    let found = run(&graph);
    assert_eq!(codes(&found), ["graph.topic_without_producer"]);
    assert_eq!(found[0].subject, refund);
    assert_eq!(found[0].project, Some(name("worker")));
}

#[test]
fn orphan_topic_lacks_both_sides() {
    let mut graph = fixture();
    billing(&mut graph, |d| {
        d.add_node(Node::contract(ContractKind::Topic, "orphan"))
    });
    assert_eq!(
        codes(&run(&graph)),
        [
            "graph.topic_without_consumer",
            "graph.topic_without_producer"
        ]
    );
}

#[test]
fn table_never_read() {
    let mut graph = fixture();
    let i = ids();
    let outbox = contract(ContractKind::Table, "outbox");
    billing(&mut graph, |d| {
        d.add_node(Node::contract(ContractKind::Table, "outbox"))
            .add_edge(&i.service, &outbox, strong(EdgeKind::Writes))
    });
    let found = run(&graph);
    assert_eq!(codes(&found), ["graph.table_never_read"]);
    assert_eq!(found[0].subject, outbox);
    assert_eq!(found[0].severity, Severity::Info);

    // Reading it clears the finding.
    billing(&mut graph, |d| {
        d.add_edge(&i.report, &outbox, strong(EdgeKind::Reads))
    });
    assert_eq!(run(&graph), Vec::<Insight>::new());
}

#[test]
fn contract_drift_compares_schema_hashes() {
    let mut graph = fixture();
    let i = ids();
    // The producer moves to a new schema; the web client still targets the old one.
    billing(&mut graph, |d| {
        d.add_node(
            Node::contract(ContractKind::Endpoint, ENDPOINT).with_attr(ATTR_SCHEMA_HASH, HASH_V2),
        )
    });
    let found = run(&graph);
    assert_eq!(codes(&found), ["graph.contract_drift"]);
    let insight = &found[0];
    assert_eq!(insight.subject, i.endpoint);
    assert_eq!(insight.severity, Severity::Error);
    assert_eq!(insight.project, Some(name("web")));
    assert_eq!(insight.related, std::slice::from_ref(&i.ui));
    assert_eq!(insight.evidence, [ev("web", "src/api.ts", 12)]);
    assert!(insight.message.contains("aaaaaaaaaaaa"));
    assert!(insight.message.contains("bbbbbbbbbbbb"));

    // A client without a recorded hash is unknown, not drifting.
    let second = sym("web", "Other.client");
    web(&mut graph, |d| {
        d.add_node(symbol_node("web", "Other.client", "other"))
            .add_edge(&second, &i.endpoint, derived(EdgeKind::Consumes))
    });
    assert_eq!(codes(&run(&graph)), ["graph.contract_drift"]);

    // Updating the client to the new hash clears it.
    web(&mut graph, |d| {
        d.add_edge(
            &i.ui,
            &i.endpoint,
            derived(EdgeKind::Consumes).with_attr(ATTR_SCHEMA_HASH, HASH_V2),
        )
    });
    assert_eq!(run(&graph), Vec::<Insight>::new());
}

#[test]
fn contract_without_hash_never_drifts() {
    let mut graph = fixture();
    let i = ids();
    billing(&mut graph, |d| {
        d.add_node(Node::contract(ContractKind::Endpoint, ENDPOINT))
    });
    assert_eq!(run(&graph), Vec::<Insight>::new());
    web(&mut graph, |d| {
        d.add_edge(
            &i.ui,
            &i.endpoint,
            derived(EdgeKind::Consumes).with_attr(ATTR_SCHEMA_HASH, HASH_V2),
        )
    });
    assert_eq!(run(&graph), Vec::<Insight>::new());
}

fn add_locale_key(graph: &mut CodeGraph, key: &str, locales: &[&str], used: bool) {
    let i = ids();
    let id = contract(ContractKind::I18nKey, key);
    web(graph, |mut d| {
        d = d.add_node(Node::contract(ContractKind::I18nKey, key));
        for locale in locales {
            d = d.add_edge(
                &file("web", &format!("src/i18n/{locale}.json")),
                &id,
                strong(EdgeKind::Defines),
            );
        }
        if used {
            d = d.add_edge(
                &i.ui,
                &id,
                strong(EdgeKind::References).with_ref(ev("web", "src/Page.tsx", 3)),
            );
        }
        d
    });
}

#[test]
fn i18n_key_missing_in_a_locale() {
    let mut graph = fixture();
    add_locale_key(&mut graph, "cart.empty", &["en"], true);
    let found = run(&graph);
    assert_eq!(codes(&found), ["graph.i18n_key_missing_locale"]);
    assert!(found[0].message.ends_with(": tr"), "{}", found[0].message);
    assert_eq!(found[0].severity, Severity::Error);
    assert_eq!(found[0].project, Some(name("web")));
    assert_eq!(found[0].evidence, [ev("web", "src/Page.tsx", 3)]);
    assert_eq!(found[0].related.len(), 1);
}

#[test]
fn i18n_key_used_but_undefined() {
    let mut graph = fixture();
    add_locale_key(&mut graph, "cart.ghost", &[], true);
    let found = run(&graph);
    assert_eq!(codes(&found), ["graph.i18n_key_undefined"]);
    assert_eq!(found[0].severity, Severity::Error);
}

#[test]
fn i18n_key_defined_but_unused() {
    let mut graph = fixture();
    add_locale_key(&mut graph, "old.label", &["en", "tr"], false);
    let found = run(&graph);
    assert_eq!(codes(&found), ["graph.i18n_key_unused"]);
    assert_eq!(found[0].severity, Severity::Info);
}

#[test]
fn i18n_defined_everywhere_and_used_is_clean() {
    let mut graph = fixture();
    add_locale_key(&mut graph, "cart.title", &["en", "tr"], true);
    assert_eq!(run(&graph), Vec::<Insight>::new());
}

#[test]
fn required_locales_can_be_configured() {
    let graph = fixture();
    let config = InsightConfig {
        required_locales: Some(["de", "en", "tr"].iter().map(|s| (*s).to_owned()).collect()),
        ..InsightConfig::default()
    };
    let found = graph.insights(&config);
    assert_eq!(codes(&found), ["graph.i18n_key_missing_locale"]);
    assert!(found[0].message.ends_with(": de"));

    // A subset removes locales from the requirement.
    let config = InsightConfig {
        required_locales: Some(["en".to_owned()].into_iter().collect()),
        ..InsightConfig::default()
    };
    assert!(graph.insights(&config).is_empty());
}

#[test]
fn definers_without_locale_info_cannot_prove_a_gap() {
    let mut graph = fixture();
    let i = ids();
    let key = contract(ContractKind::I18nKey, "plain.key");
    let plain = file("web", "src/i18n/plain.json");
    web(&mut graph, |d| {
        d.add_node(Node::file(
            &name("web"),
            &RepoPath::new("src/i18n/plain.json").unwrap(),
        ))
        .add_node(Node::contract(ContractKind::I18nKey, "plain.key"))
        .add_edge(&plain, &key, strong(EdgeKind::Defines))
        .add_edge(&i.ui, &key, strong(EdgeKind::References))
    });
    assert_eq!(run(&graph), Vec::<Insight>::new());
    // Silence the unused import for the locale attribute constant in this file.
    assert_eq!(ATTR_LOCALE, "locale");
}

#[test]
fn env_name_read_but_undeclared() {
    let mut graph = fixture();
    let i = ids();
    let missing = contract(ContractKind::EnvName, "KNOWELL_CANARY_UNDECLARED");
    worker(&mut graph, |d| {
        d.add_node(Node::contract(
            ContractKind::EnvName,
            "KNOWELL_CANARY_UNDECLARED",
        ))
        .add_edge(
            &i.consumer,
            &missing,
            strong(EdgeKind::Reads).with_ref(ev("worker", "src/config.rs", 4)),
        )
    });
    let found = run(&graph);
    assert_eq!(codes(&found), ["graph.env_undeclared"]);
    assert_eq!(found[0].subject, missing);
    assert_eq!(found[0].severity, Severity::Warning);
    assert_eq!(found[0].project, Some(name("worker")));
    assert_eq!(found[0].evidence, [ev("worker", "src/config.rs", 4)]);

    // Declaring it in deployment config clears the finding.
    let compose = file("deploy", "compose.yml");
    let next = graph.generation(&name("deploy")) + 1;
    graph
        .apply(GraphDelta::new(name("deploy"), next).add_edge(
            &compose,
            &missing,
            strong(EdgeKind::Defines),
        ))
        .unwrap();
    assert_eq!(run(&graph), Vec::<Insight>::new());
}

#[test]
fn declared_but_unread_env_is_not_reported() {
    let mut graph = fixture();
    let spare = contract(ContractKind::EnvName, "SPARE_NAME");
    let compose = file("deploy", "compose.yml");
    worker(&mut graph, |d| {
        d.add_node(Node::contract(ContractKind::EnvName, "SPARE_NAME"))
    });
    let next = graph.generation(&name("deploy")) + 1;
    graph
        .apply(GraphDelta::new(name("deploy"), next).add_edge(
            &compose,
            &spare,
            strong(EdgeKind::Defines),
        ))
        .unwrap();
    assert_eq!(run(&graph), Vec::<Insight>::new());
}

/// A graph with one finding of every code.
fn broken() -> CodeGraph {
    let mut graph = fixture();
    let i = ids();
    billing(&mut graph, |d| {
        d.add_node(
            Node::contract(ContractKind::Endpoint, ENDPOINT).with_attr(ATTR_SCHEMA_HASH, HASH_V2),
        )
        .add_node(Node::contract(ContractKind::Endpoint, "GET /v1/legacy"))
        .add_node(Node::contract(ContractKind::Topic, "invoice.paid"))
        .add_node(Node::contract(ContractKind::Table, "outbox"))
        .add_edge(
            &i.controller,
            &contract(ContractKind::Endpoint, "GET /v1/legacy"),
            derived(EdgeKind::Exposes),
        )
        .add_edge(
            &i.service,
            &contract(ContractKind::Topic, "invoice.paid"),
            derived(EdgeKind::Produces),
        )
        .add_edge(
            &i.service,
            &contract(ContractKind::Table, "outbox"),
            strong(EdgeKind::Writes),
        )
    });
    worker(&mut graph, |d| {
        d.add_node(Node::contract(ContractKind::Topic, "refund.requested"))
            .add_node(Node::contract(ContractKind::EnvName, "UNDECLARED_NAME"))
            .add_edge(
                &i.consumer,
                &contract(ContractKind::Topic, "refund.requested"),
                derived(EdgeKind::Consumes),
            )
            .add_edge(
                &i.consumer,
                &contract(ContractKind::EnvName, "UNDECLARED_NAME"),
                strong(EdgeKind::Reads),
            )
    });
    add_locale_key(&mut graph, "cart.empty", &["en"], true);
    add_locale_key(&mut graph, "cart.ghost", &[], true);
    add_locale_key(&mut graph, "old.label", &["en", "tr"], false);
    graph
}

#[test]
fn every_code_fires_and_output_is_sorted() {
    let found = run(&broken());
    assert_eq!(
        codes(&found),
        [
            "graph.contract_drift",
            "graph.endpoint_without_client",
            "graph.env_undeclared",
            "graph.i18n_key_missing_locale",
            "graph.i18n_key_undefined",
            "graph.i18n_key_unused",
            "graph.table_never_read",
            "graph.topic_without_consumer",
            "graph.topic_without_producer",
        ]
    );
    for insight in &found {
        assert_eq!(insight.severity, insight.code.severity());
        assert!(!insight.edges.is_empty());
        assert!(!insight.message.is_empty());
    }
}

#[test]
fn code_filter_runs_only_selected_checks() {
    let graph = broken();
    let config = InsightConfig {
        codes: [InsightCode::TableNeverRead, InsightCode::ContractDrift]
            .into_iter()
            .collect(),
        ..InsightConfig::default()
    };
    assert_eq!(
        codes(&graph.insights(&config)),
        ["graph.contract_drift", "graph.table_never_read"]
    );
}

#[test]
fn insights_are_deterministic() {
    let a = run(&broken());
    let b = run(&broken());
    assert_eq!(a, b);
}
