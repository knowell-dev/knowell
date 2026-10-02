//! Impact analysis: reverse reachability, risk, tests and unknown impact.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::*;
use knowell_graph::{
    ATTR_UNRESOLVED, CodeGraph, ContractKind, Edge, EdgeFilter, EdgeKind, EvidenceType, GraphDelta,
    GraphError, ImpactReport, ImpactSpec, Node, NodeId, NodeKind, Resolution, RiskLevel,
    UnknownReason,
};
use pretty_assertions::assert_eq;

fn impact(graph: &CodeGraph, targets: &[&NodeId]) -> ImpactReport {
    let spec = ImpactSpec::new(targets.iter().map(|t| (*t).clone()).collect());
    graph.impact(&spec).unwrap()
}

fn affected_ids(report: &ImpactReport) -> Vec<&NodeId> {
    report.affected.iter().map(|a| &a.node).collect()
}

fn find<'a>(report: &'a ImpactReport, id: &NodeId) -> &'a knowell_graph::ImpactedNode {
    report
        .affected
        .iter()
        .chain(report.tests.iter())
        .find(|a| &a.node == id)
        .unwrap_or_else(|| panic!("{id} not in report"))
}

#[test]
fn service_change_reaches_callers_consumers_clients_and_tests() {
    let graph = fixture();
    let i = ids();
    let report = impact(&graph, &[&i.service]);

    let mut got: Vec<&NodeId> = affected_ids(&report);
    got.sort();
    let mut expected = vec![
        &i.controller,
        &i.helper,
        &i.topic,
        &i.subs,
        &i.repo,
        &i.consumer,
        &i.endpoint,
        &i.audit,
        &i.report,
        &i.ui,
    ];
    expected.sort();
    assert_eq!(got, expected);
    assert!(!report.truncated);
    assert_eq!(report.targets, std::slice::from_ref(&i.service));

    // Tests reached: the billing unit test directly, the web test through the endpoint's client.
    let test_ids: Vec<&NodeId> = report.tests.iter().map(|t| &t.node).collect();
    assert_eq!(test_ids, [&i.billing_test, &i.web_test]);
    assert_eq!(report.tests[0].risk.level, RiskLevel::High);
    assert_eq!(report.tests[0].depth, 1);
    assert_eq!(report.tests[1].depth, 4);
    assert_eq!(report.tests[1].risk.level, RiskLevel::Medium);
    assert!(report.tests.iter().all(|t| t.kind == NodeKind::Test));
    assert!(report.affected.iter().all(|a| a.kind != NodeKind::Test));
}

#[test]
fn risk_explains_the_carrying_edges() {
    let graph = fixture();
    let i = ids();
    let report = impact(&graph, &[&i.service]);

    let controller = find(&report, &i.controller);
    assert_eq!(controller.risk.level, RiskLevel::High);
    assert_eq!(controller.risk.carried_by.len(), 1);
    assert_eq!(controller.risk.carried_by[0].edge.kind, EdgeKind::Calls);
    assert!(
        !controller.risk.carried_by[0].forward,
        "a caller is reached against the edge"
    );

    // The helper is only a heuristic caller.
    let helper = find(&report, &i.helper);
    assert_eq!(helper.risk.level, RiskLevel::Low);
    assert_eq!(helper.risk.weakest_evidence, EvidenceType::Heuristic);

    // The UI is reached two ways: through the helper (heuristic, 2 hops) and
    // through the endpoint (contract-derived, 3 hops). The stronger one carries it.
    let ui = find(&report, &i.ui);
    assert_eq!(ui.depth, 3);
    assert_eq!(ui.risk.level, RiskLevel::Medium);
    assert_eq!(ui.risk.weakest_evidence, EvidenceType::ContractDerived);
    let via: Vec<EdgeKind> = ui.risk.carried_by.iter().map(|h| h.edge.kind).collect();
    assert_eq!(
        via,
        [EdgeKind::Calls, EdgeKind::Exposes, EdgeKind::Consumes]
    );
    assert_eq!(ui.contracts_crossed, std::slice::from_ref(&i.endpoint));

    // Provider side of a contract: service -> topic is walked along the edge.
    let topic = find(&report, &i.topic);
    assert!(topic.risk.carried_by[0].forward);
    assert_eq!(topic.risk.level, RiskLevel::High);

    let audit = find(&report, &i.audit);
    assert_eq!(audit.depth, 3);
    assert_eq!(audit.risk.level, RiskLevel::Medium);
    assert_eq!(audit.contracts_crossed, std::slice::from_ref(&i.topic));
}

#[test]
fn affected_is_sorted_by_risk_then_depth_then_id() {
    let graph = fixture();
    let i = ids();
    let report = impact(&graph, &[&i.service]);
    let keys: Vec<(RiskLevel, usize)> = report
        .affected
        .iter()
        .map(|a| (a.risk.level, a.depth))
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
    let first_three: Vec<&NodeId> = report.affected.iter().take(3).map(|a| &a.node).collect();
    assert_eq!(first_three, [&i.subs, &i.topic, &i.controller]);
}

#[test]
fn impact_is_grouped_by_project() {
    let graph = fixture();
    let i = ids();
    let report = impact(&graph, &[&i.service]);
    let groups: Vec<(Option<String>, usize, usize, RiskLevel)> = report
        .by_project
        .iter()
        .map(|g| {
            (
                g.project.as_ref().map(ToString::to_string),
                g.affected.len(),
                g.tests.len(),
                g.highest_risk,
            )
        })
        .collect();
    assert_eq!(
        groups,
        [
            (Some("billing".to_owned()), 3, 1, RiskLevel::High), // controller, repo, report; billing test
            (Some("web".to_owned()), 2, 1, RiskLevel::Medium),   // helper, ui; web test
            (Some("worker".to_owned()), 1, 0, RiskLevel::High),  // consumer
            (None, 4, 0, RiskLevel::High), // topic, subs, endpoint, audit: shared contracts last
        ]
    );
    let web = &report.by_project[1];
    assert_eq!(web.tests, std::slice::from_ref(&i.web_test));
}

fn with_second_exposer() -> (CodeGraph, NodeId) {
    let mut graph = fixture();
    let i = ids();
    let gateway = sym("billing", "Gateway.cancel");
    let delta = GraphDelta::new(name("billing"), 2)
        .add_node(symbol_node("billing", "Gateway.cancel", "gateway"))
        .add_edge(&gateway, &i.endpoint, derived(EdgeKind::Exposes));
    graph.apply(delta).unwrap();
    (graph, gateway)
}

#[test]
fn provider_change_does_not_leak_to_other_providers_of_the_same_contract() {
    let (graph, gateway) = with_second_exposer();
    let i = ids();
    let report = impact(&graph, &[&i.controller]);
    let affected = affected_ids(&report);
    assert!(affected.contains(&&i.endpoint));
    assert!(affected.contains(&&i.ui));
    assert!(
        !affected.contains(&&gateway),
        "sibling provider is not impacted"
    );
    assert_eq!(find(&report, &i.web_test).depth, 3);
}

#[test]
fn contract_change_reaches_every_provider_and_client() {
    let (graph, gateway) = with_second_exposer();
    let i = ids();
    let report = impact(&graph, &[&i.endpoint]);
    let affected = affected_ids(&report);
    for id in [&i.controller, &gateway, &i.ui] {
        assert!(affected.contains(&id), "{id} should be affected");
    }
    assert!(
        !affected.contains(&&i.endpoint),
        "targets are not listed as affected"
    );
    assert_eq!(find(&report, &i.ui).risk.level, RiskLevel::High);
}

#[test]
fn multiple_targets_are_merged() {
    let graph = fixture();
    let i = ids();
    let report = impact(&graph, &[&i.audit, &i.repo, &i.audit]);
    assert_eq!(report.targets.len(), 2);
    let affected = affected_ids(&report);
    assert!(affected.contains(&&i.report)); // reads audit
    assert!(affected.contains(&&i.service)); // calls repo
    assert!(!affected.contains(&&i.repo));
}

#[test]
fn unresolved_and_dynamic_edges_are_reported_as_unknown() {
    let mut graph = fixture();
    let i = ids();
    let cron = sym("billing", "Legacy.cron");
    let dispatcher = sym("billing", "Dispatcher.run");
    let placeholder = sym("billing", "dyn:cancel");
    let delta = GraphDelta::new(name("billing"), 2)
        .add_node(symbol_node("billing", "Legacy.cron", "cron"))
        .add_node(symbol_node("billing", "Dispatcher.run", "dispatch"))
        .add_node(
            Node::symbol(&name("billing"), "dyn:cancel", "cancel")
                .with_attr(ATTR_UNRESOLVED, "true"),
        )
        .add_edge(&cron, &i.service, unresolved(EdgeKind::Calls))
        .add_edge(&dispatcher, &placeholder, unresolved(EdgeKind::Calls));
    graph.apply(delta).unwrap();

    let report = impact(&graph, &[&i.service]);
    let cron_impact = find(&report, &cron);
    assert_eq!(cron_impact.risk.level, RiskLevel::Unknown);
    assert_eq!(cron_impact.risk.flagged_hops, 1);

    let unknown: Vec<(&NodeId, UnknownReason)> =
        report.unknown.iter().map(|u| (&u.node, u.reason)).collect();
    assert!(unknown.contains(&(&cron, UnknownReason::UnresolvedEdgeOnPath)));
    assert!(unknown.contains(&(&dispatcher, UnknownReason::NameMatchesUnresolvedReference)));
    // The dispatcher is not certain enough to be listed as affected.
    assert!(!affected_ids(&report).contains(&&dispatcher));
    // Sorted and unique.
    let mut sorted = report.unknown.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted, report.unknown);

    // A clean graph reports nothing unknown.
    assert!(impact(&fixture(), &[&i.service]).unknown.is_empty());
}

#[test]
fn ambiguous_edges_lower_risk_to_low() {
    let mut graph = fixture();
    let i = ids();
    let other = sym("billing", "Maybe.caller");
    let ambiguous = Edge::new(
        EdgeKind::Calls,
        EvidenceType::SemanticResolved,
        Resolution::Ambiguous,
    );
    let delta = GraphDelta::new(name("billing"), 2)
        .add_node(symbol_node("billing", "Maybe.caller", "maybe"))
        .add_edge(&other, &i.service, ambiguous);
    graph.apply(delta).unwrap();
    let report = impact(&graph, &[&i.service]);
    let item = find(&report, &other);
    assert_eq!(item.risk.level, RiskLevel::Low);
    assert_eq!(item.risk.flagged_hops, 1);
    assert!(
        report.unknown.is_empty(),
        "ambiguous is low risk, not unknown"
    );
}

#[test]
fn best_path_beats_shortest_path() {
    let mut graph = CodeGraph::new();
    let p = name("billing");
    let (t, x, y) = (
        sym("billing", "t"),
        sym("billing", "x"),
        sym("billing", "y"),
    );
    let delta = GraphDelta::new(p.clone(), 1)
        .add_node(symbol_node("billing", "t", "t"))
        .add_node(symbol_node("billing", "x", "x"))
        .add_node(symbol_node("billing", "y", "y"))
        .add_edge(&x, &t, heuristic(EdgeKind::Calls))
        .add_edge(&x, &y, strong(EdgeKind::Calls))
        .add_edge(&y, &t, strong(EdgeKind::Calls));
    graph.apply(delta).unwrap();
    let report = impact(&graph, &[&t]);
    let item = find(&report, &x);
    assert_eq!(item.depth, 2);
    assert_eq!(item.risk.level, RiskLevel::High);
    assert_eq!(item.risk.weakest_evidence, EvidenceType::SemanticResolved);
}

#[test]
fn depth_budget_and_filter_bound_the_search() {
    let graph = fixture();
    let i = ids();

    let shallow = graph
        .impact(&ImpactSpec::new(vec![i.service.clone()]).with_max_depth(1))
        .unwrap();
    assert!(affected_ids(&shallow).contains(&&i.controller));
    assert!(!affected_ids(&shallow).contains(&&i.ui));
    assert!(shallow.affected.iter().all(|a| a.depth == 1));

    let tight = ImpactSpec::new(vec![i.service.clone()]).with_node_budget(2);
    let a = graph.impact(&tight).unwrap();
    assert!(a.truncated);
    assert_eq!(a.affected.len() + a.tests.len(), 2);
    assert_eq!(a, graph.impact(&tight).unwrap());

    let strong_only = graph
        .impact(
            &ImpactSpec::new(vec![i.service.clone()])
                .with_filter(EdgeFilter::any().with_min_evidence(EvidenceType::Syntactic)),
        )
        .unwrap();
    assert!(!affected_ids(&strong_only).contains(&&i.helper));
    assert!(
        affected_ids(&strong_only).contains(&&i.ui),
        "still reachable via the endpoint"
    );
}

#[test]
fn docs_are_reported_but_not_expanded() {
    let mut graph = fixture();
    let i = ids();
    let doc = NodeId::doc(&name("billing"), "cancel.md");
    let meta = NodeId::doc(&name("billing"), "index.md");
    let delta = GraphDelta::new(name("billing"), 2)
        .add_node(Node::doc(&name("billing"), "cancel.md", "cancel docs"))
        .add_node(Node::doc(&name("billing"), "index.md", "index"))
        .add_edge(&doc, &i.service, strong(EdgeKind::Documents))
        .add_edge(&meta, &doc, strong(EdgeKind::Documents));
    graph.apply(delta).unwrap();
    let report = impact(&graph, &[&i.service]);
    assert!(affected_ids(&report).contains(&&doc));
    assert!(!affected_ids(&report).contains(&&meta));
}

#[test]
fn impact_validates_arguments() {
    let graph = fixture();
    let i = ids();
    let ghost = sym("billing", "ghost");
    assert_eq!(
        graph.impact(&ImpactSpec::new(vec![ghost.clone()])),
        Err(GraphError::UnknownNode(ghost))
    );
    assert_eq!(
        graph.impact(&ImpactSpec::new(vec![])),
        Err(GraphError::InvalidLimit("target count"))
    );
    for depth in [0, 9] {
        assert_eq!(
            graph.impact(&ImpactSpec::new(vec![i.service.clone()]).with_max_depth(depth)),
            Err(GraphError::InvalidDepth {
                given: depth,
                max: 8
            })
        );
    }
    assert_eq!(
        graph.impact(&ImpactSpec::new(vec![i.service.clone()]).with_node_budget(0)),
        Err(GraphError::InvalidLimit("node budget"))
    );
}

#[test]
fn impact_is_deterministic_across_builds() {
    let i = ids();
    let spec = ImpactSpec::new(vec![i.service.clone(), i.audit.clone()]);
    let a = fixture_with(false).impact(&spec).unwrap();
    let b = fixture_with(true).impact(&spec).unwrap();
    assert_eq!(a, b);
}

#[test]
fn contract_only_graph_edges_are_handled() {
    // A package contract depended on by a project: changing the package reaches the dependent.
    let mut graph = CodeGraph::new();
    let p = name("billing");
    let package = NodeId::contract(ContractKind::Package, "serde");
    let project = NodeId::project(&p);
    let delta = GraphDelta::new(p.clone(), 1)
        .add_node(Node::project(&p))
        .add_node(Node::contract(ContractKind::Package, "serde"))
        .add_edge(&project, &package, strong(EdgeKind::DependsOn));
    graph.apply(delta).unwrap();
    let report = impact(&graph, &[&package]);
    assert_eq!(affected_ids(&report), [&project]);
}
