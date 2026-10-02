//! Flow tracing across contracts and path ranking.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::*;
use knowell_graph::{
    CodeGraph, Edge, EdgeFilter, EdgeKind, EvidenceType, FlowDirection, FlowSpec, GraphDelta,
    GraphError, NodeId, Resolution,
};
use pretty_assertions::assert_eq;

fn nodes_of(path: &knowell_graph::FlowPath) -> Vec<&NodeId> {
    path.nodes.iter().collect()
}

/// A small single-project graph for ranking rules.
fn mini(nodes: &[&str], edges: Vec<(&str, &str, Edge)>) -> CodeGraph {
    let mut delta = GraphDelta::new(name("billing"), 1);
    for n in nodes {
        delta = delta.add_node(symbol_node("billing", n, n));
    }
    for (from, to, edge) in edges {
        delta = delta.add_edge(&sym("billing", from), &sym("billing", to), edge);
    }
    let mut graph = CodeGraph::new();
    graph.apply(delta).unwrap();
    graph
}

fn path_names(graph: &CodeGraph, spec: &FlowSpec) -> Vec<Vec<String>> {
    graph
        .trace_flow(spec)
        .unwrap()
        .paths
        .iter()
        .map(|p| {
            p.nodes
                .iter()
                .map(|n| n.as_str().rsplit(':').next().unwrap_or_default().to_owned())
                .collect()
        })
        .collect()
}

fn mini_spec(from: &str, to: &str) -> FlowSpec {
    FlowSpec::between(sym("billing", from), sym("billing", to))
}

#[test]
fn ui_call_to_table_crosses_two_contracts_and_three_projects() {
    let graph = fixture();
    let i = ids();
    let trace = graph
        .trace_flow(&FlowSpec::between(i.ui.clone(), i.audit.clone()).with_max_paths(3))
        .unwrap();
    assert!(!trace.truncated);
    assert_eq!(trace.paths.len(), 2);

    let best = &trace.paths[0];
    assert_eq!(
        nodes_of(best),
        [
            &i.ui,
            &i.endpoint,
            &i.controller,
            &i.service,
            &i.topic,
            &i.consumer,
            &i.audit
        ]
    );
    assert_eq!(best.len(), 6);
    assert_eq!(
        best.contracts,
        [i.endpoint.clone(), i.topic.clone(), i.audit.clone()]
    );
    assert_eq!(
        best.projects,
        [name("web"), name("billing"), name("worker")]
    );
    assert!(best.crosses_projects());
    assert_eq!(best.weakest_evidence, EvidenceType::ContractDerived);
    assert!(!best.is_flagged());
    // Request flows along the consumes edge, then against exposes; events run
    // against the consumes edge onto a topic.
    let forwards: Vec<bool> = best.hops.iter().map(|h| h.forward).collect();
    assert_eq!(forwards, [true, false, true, true, false, true]);
    assert_eq!(best.hops[0].from, i.ui);
    assert_eq!(best.hops[1].from, i.endpoint);
    assert_eq!(best.hops[1].to, i.controller);
}

#[test]
fn stronger_evidence_beats_shorter_path() {
    let graph = fixture();
    let i = ids();
    let trace = graph
        .trace_flow(&FlowSpec::between(i.ui.clone(), i.audit.clone()).with_max_paths(3))
        .unwrap();
    let weak = &trace.paths[1];
    assert_eq!(
        nodes_of(weak),
        [
            &i.ui,
            &i.helper,
            &i.service,
            &i.topic,
            &i.consumer,
            &i.audit
        ]
    );
    assert_eq!(weak.len(), 5);
    assert_eq!(weak.weakest_evidence, EvidenceType::Heuristic);
    // Shorter, but ranked second because its weakest edge is heuristic.
    assert!(trace.paths[0].len() > weak.len());
}

#[test]
fn depth_limit_drops_long_paths() {
    let graph = fixture();
    let i = ids();
    let at_five = graph
        .trace_flow(&FlowSpec::between(i.ui.clone(), i.audit.clone()).with_max_depth(5))
        .unwrap();
    assert_eq!(at_five.paths.len(), 1);
    assert_eq!(at_five.paths[0].len(), 5);
    let at_four = graph
        .trace_flow(&FlowSpec::between(i.ui.clone(), i.audit.clone()).with_max_depth(4))
        .unwrap();
    assert!(at_four.paths.is_empty());
}

#[test]
fn evidence_filter_removes_weak_paths() {
    let graph = fixture();
    let i = ids();
    let spec = FlowSpec::between(i.ui.clone(), i.audit.clone())
        .with_filter(EdgeFilter::any().with_min_evidence(EvidenceType::Syntactic));
    let trace = graph.trace_flow(&spec).unwrap();
    assert_eq!(trace.paths.len(), 1);
    assert_eq!(
        trace.paths[0].weakest_evidence,
        EvidenceType::ContractDerived
    );
}

#[test]
fn reads_flow_from_table_to_reader() {
    let graph = fixture();
    let i = ids();
    let trace = graph
        .trace_flow(&FlowSpec::between(i.audit.clone(), i.report.clone()))
        .unwrap();
    assert_eq!(trace.paths.len(), 1);
    assert_eq!(trace.paths[0].hops.len(), 1);
    assert!(!trace.paths[0].hops[0].forward);
    // Against the flow nothing leads from the reader back... except upstream.
    let none = graph
        .trace_flow(&FlowSpec::between(i.report.clone(), i.audit.clone()))
        .unwrap();
    assert!(none.paths.is_empty());
    let up = graph
        .trace_flow(&FlowSpec::between(i.report.clone(), i.audit.clone()).upstream())
        .unwrap();
    assert_eq!(up.paths.len(), 1);
}

#[test]
fn open_downstream_trace_lists_maximal_flows() {
    let graph = fixture();
    let i = ids();
    let trace = graph
        .trace_flow(&FlowSpec::open(i.ui.clone(), FlowDirection::Downstream).with_max_paths(20))
        .unwrap();
    let ends: Vec<&NodeId> = trace.paths.iter().filter_map(|p| p.nodes.last()).collect();
    // Only flow end points: no path stops at the endpoint, topic or service.
    for stop in [
        &i.endpoint,
        &i.topic,
        &i.service,
        &i.controller,
        &i.consumer,
    ] {
        assert!(!ends.contains(&stop), "{stop} is not an end point");
    }
    assert!(ends.contains(&&i.report));
    assert!(ends.contains(&&i.i18n));

    let strong: Vec<_> = trace
        .paths
        .iter()
        .filter(|p| p.weakest_evidence == EvidenceType::ContractDerived)
        .collect();
    let longest = strong.first().unwrap();
    assert_eq!(longest.nodes.last(), Some(&i.report));
    assert_eq!(longest.len(), 7);
    let lens: Vec<usize> = strong.iter().map(|p| p.len()).collect();
    assert!(
        lens.windows(2).all(|w| w[0] >= w[1]),
        "longest first: {lens:?}"
    );
}

#[test]
fn upstream_trace_finds_what_feeds_a_node() {
    let graph = fixture();
    let i = ids();
    let trace = graph
        .trace_flow(&FlowSpec::open(i.audit.clone(), FlowDirection::Upstream))
        .unwrap();
    let best = &trace.paths[0];
    assert_eq!(
        nodes_of(best),
        [
            &i.audit,
            &i.consumer,
            &i.topic,
            &i.service,
            &i.controller,
            &i.endpoint,
            &i.ui
        ]
    );
    // The env read is a separate, weaker input of the consumer.
    let env_path = trace
        .paths
        .iter()
        .find(|p| p.nodes.last() == Some(&i.env))
        .unwrap();
    assert_eq!(env_path.weakest_evidence, EvidenceType::Syntactic);
    // The heuristic route through the helper is last.
    assert_eq!(
        trace.paths.last().unwrap().weakest_evidence,
        EvidenceType::Heuristic
    );
}

#[test]
fn evidence_outranks_length_in_mini_graph() {
    let graph = mini(
        &["a", "b", "c", "d"],
        vec![
            ("a", "d", heuristic(EdgeKind::Calls)),
            ("a", "b", strong(EdgeKind::Calls)),
            ("b", "c", strong(EdgeKind::Calls)),
            ("c", "d", strong(EdgeKind::Calls)),
        ],
    );
    let paths = path_names(&graph, &mini_spec("a", "d"));
    assert_eq!(paths, [vec!["a", "b", "c", "d"], vec!["a", "d"]]);
}

#[test]
fn resolved_outranks_ambiguous_and_flags_the_latter() {
    let ambiguous = Edge::new(
        EdgeKind::Calls,
        EvidenceType::SemanticResolved,
        Resolution::Ambiguous,
    );
    let graph = mini(
        &["a", "b", "d"],
        vec![
            ("a", "d", ambiguous),
            ("a", "b", strong(EdgeKind::Calls)),
            ("b", "d", strong(EdgeKind::Calls)),
        ],
    );
    let trace = graph.trace_flow(&mini_spec("a", "d")).unwrap();
    assert_eq!(trace.paths.len(), 2);
    assert!(!trace.paths[0].is_flagged());
    assert_eq!(trace.paths[0].len(), 2);
    assert!(trace.paths[1].is_flagged());
    assert_eq!(trace.paths[1].flagged_hops, 1);
    assert_eq!(trace.paths[1].len(), 1);
    // Excluding ambiguous edges drops the flagged path entirely.
    let spec = mini_spec("a", "d").with_filter(EdgeFilter::any().resolved_only());
    assert_eq!(graph.trace_flow(&spec).unwrap().paths.len(), 1);
}

#[test]
fn unresolved_only_path_is_returned_but_flagged() {
    let graph = mini(&["a", "d"], vec![("a", "d", unresolved(EdgeKind::Calls))]);
    let trace = graph.trace_flow(&mini_spec("a", "d")).unwrap();
    assert_eq!(trace.paths.len(), 1);
    assert!(trace.paths[0].is_flagged());
}

#[test]
fn shorter_wins_on_equal_evidence() {
    let graph = mini(
        &["a", "b", "c", "d"],
        vec![
            ("a", "b", strong(EdgeKind::Calls)),
            ("b", "c", strong(EdgeKind::Calls)),
            ("c", "d", strong(EdgeKind::Calls)),
            ("a", "d", strong(EdgeKind::Calls)),
        ],
    );
    let paths = path_names(&graph, &mini_spec("a", "d"));
    assert_eq!(paths, [vec!["a", "d"], vec!["a", "b", "c", "d"]]);
}

#[test]
fn ties_break_by_edge_keys_not_insertion_order() {
    let edges = vec![
        ("a", "c", strong(EdgeKind::Calls)),
        ("c", "d", strong(EdgeKind::Calls)),
        ("a", "b", strong(EdgeKind::Calls)),
        ("b", "d", strong(EdgeKind::Calls)),
    ];
    let forward = mini(&["a", "b", "c", "d"], edges.clone());
    let mut reversed_edges = edges;
    reversed_edges.reverse();
    let reversed = mini(&["d", "c", "b", "a"], reversed_edges);
    let expected = vec![vec!["a", "b", "d"], vec!["a", "c", "d"]];
    assert_eq!(path_names(&forward, &mini_spec("a", "d")), expected);
    assert_eq!(path_names(&reversed, &mini_spec("a", "d")), expected);
}

#[test]
fn max_paths_limits_and_keeps_the_best() {
    let graph = mini(
        &["a", "b", "c", "d"],
        vec![
            ("a", "b", strong(EdgeKind::Calls)),
            ("b", "d", strong(EdgeKind::Calls)),
            ("a", "c", heuristic(EdgeKind::Calls)),
            ("c", "d", heuristic(EdgeKind::Calls)),
            ("a", "d", unresolved(EdgeKind::Calls)),
        ],
    );
    let one = path_names(&graph, &mini_spec("a", "d").with_max_paths(1));
    assert_eq!(one, [vec!["a", "b", "d"]]);
    let two = path_names(&graph, &mini_spec("a", "d").with_max_paths(2));
    assert_eq!(two, [vec!["a", "b", "d"], vec!["a", "c", "d"]]);
}

#[test]
fn cycles_do_not_loop() {
    let graph = mini(
        &["a", "b", "c"],
        vec![
            ("a", "b", strong(EdgeKind::Calls)),
            ("b", "a", strong(EdgeKind::Calls)),
            ("b", "c", strong(EdgeKind::Calls)),
        ],
    );
    assert_eq!(
        path_names(&graph, &mini_spec("a", "c")),
        [vec!["a", "b", "c"]]
    );
}

#[test]
fn non_flow_edges_are_not_followed() {
    let graph = mini(&["a", "b"], vec![("a", "b", strong(EdgeKind::Contains))]);
    assert!(
        graph
            .trace_flow(&mini_spec("a", "b"))
            .unwrap()
            .paths
            .is_empty()
    );
}

#[test]
fn expansion_budget_reports_truncation() {
    let graph = fixture();
    let i = ids();
    let trace = graph
        .trace_flow(&FlowSpec::between(i.ui.clone(), i.audit.clone()).with_expansion_budget(3))
        .unwrap();
    assert!(trace.truncated);
    assert!(trace.expansions <= 3);
}

#[test]
fn same_node_has_no_path_and_arguments_are_validated() {
    let graph = fixture();
    let i = ids();
    assert!(
        graph
            .trace_flow(&FlowSpec::between(i.ui.clone(), i.ui.clone()))
            .unwrap()
            .paths
            .is_empty()
    );
    let ghost = sym("web", "ghost");
    assert_eq!(
        graph.trace_flow(&FlowSpec::between(ghost.clone(), i.ui.clone())),
        Err(GraphError::UnknownNode(ghost.clone()))
    );
    assert_eq!(
        graph.trace_flow(&FlowSpec::between(i.ui.clone(), ghost.clone())),
        Err(GraphError::UnknownNode(ghost))
    );
    assert_eq!(
        graph.trace_flow(&FlowSpec::between(i.ui.clone(), i.audit.clone()).with_max_depth(0)),
        Err(GraphError::InvalidDepth { given: 0, max: 10 })
    );
    assert_eq!(
        graph.trace_flow(&FlowSpec::between(i.ui.clone(), i.audit.clone()).with_max_depth(11)),
        Err(GraphError::InvalidDepth { given: 11, max: 10 })
    );
    assert_eq!(
        graph.trace_flow(&FlowSpec::between(i.ui.clone(), i.audit.clone()).with_max_paths(0)),
        Err(GraphError::InvalidLimit("path count"))
    );
    assert_eq!(
        graph
            .trace_flow(&FlowSpec::between(i.ui.clone(), i.audit.clone()).with_expansion_budget(0)),
        Err(GraphError::InvalidLimit("expansion budget"))
    );
}

#[test]
fn trace_is_deterministic_across_builds_and_runs() {
    let i = ids();
    let spec = FlowSpec::open(i.ui.clone(), FlowDirection::Downstream).with_max_paths(20);
    let a = fixture_with(false).trace_flow(&spec).unwrap();
    let b = fixture_with(true).trace_flow(&spec).unwrap();
    let c = fixture_with(false).trace_flow(&spec).unwrap();
    assert_eq!(a, b);
    assert_eq!(a, c);
    let spec = FlowSpec::between(i.ui.clone(), i.audit.clone());
    assert_eq!(
        fixture_with(false).trace_flow(&spec).unwrap(),
        fixture_with(true).trace_flow(&spec).unwrap()
    );
}
