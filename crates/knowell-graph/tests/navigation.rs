//! Precision-gated navigation is distinct from exhaustive generic walking.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeSet;

use knowell_core::Name;
use knowell_graph::{
    CodeGraph, Direction, Edge, EdgeKind, EvidenceType, GraphDelta, GraphError, Node, NodeId,
    Resolution, WalkSpec,
};

fn project() -> Name {
    Name::new("synthetic").unwrap()
}

fn id(key: &str) -> NodeId {
    NodeId::symbol(&project(), key)
}

fn edge(kind: EdgeKind, evidence: EvidenceType, resolution: Resolution) -> Edge {
    Edge::new(kind, evidence, resolution)
}

fn fixture(reversed: bool) -> CodeGraph {
    let mut delta = GraphDelta::new(project(), 1);
    for key in [
        "start",
        "reader",
        "Streams",
        "weak",
        "ambiguous",
        "unresolved",
        "hidden",
    ] {
        delta.added_nodes.push(Node::symbol(&project(), key, key));
    }
    for (from, to, kind, evidence, resolution) in [
        (
            "start",
            "reader",
            EdgeKind::Calls,
            EvidenceType::Syntactic,
            Resolution::Resolved,
        ),
        (
            "start",
            "Streams",
            EdgeKind::References,
            EvidenceType::SemanticResolved,
            Resolution::Resolved,
        ),
        (
            "start",
            "weak",
            EdgeKind::Calls,
            EvidenceType::Heuristic,
            Resolution::Resolved,
        ),
        (
            "start",
            "ambiguous",
            EdgeKind::Calls,
            EvidenceType::SemanticResolved,
            Resolution::Ambiguous,
        ),
        (
            "start",
            "unresolved",
            EdgeKind::Calls,
            EvidenceType::Syntactic,
            Resolution::Unresolved,
        ),
        (
            "weak",
            "hidden",
            EdgeKind::Calls,
            EvidenceType::SemanticResolved,
            Resolution::Resolved,
        ),
        (
            "ambiguous",
            "hidden",
            EdgeKind::Calls,
            EvidenceType::SemanticResolved,
            Resolution::Resolved,
        ),
        (
            "unresolved",
            "hidden",
            EdgeKind::Calls,
            EvidenceType::SemanticResolved,
            Resolution::Resolved,
        ),
    ] {
        delta = delta.add_edge(&id(from), &id(to), edge(kind, evidence, resolution));
    }
    if reversed {
        delta.added_nodes.reverse();
        delta.added_edges.reverse();
    }
    let mut graph = CodeGraph::new();
    graph.apply(delta).unwrap();
    graph
}

#[test]
fn candidates_require_both_evidence_and_resolution_and_are_never_expanded() {
    let graph = fixture(false);
    let result = graph
        .walk_navigation(&id("start"), &WalkSpec::new(3, Direction::Outgoing), 12, 8)
        .unwrap();
    let main: BTreeSet<_> = result
        .visits
        .iter()
        .map(|visit| visit.node.clone())
        .collect();
    assert_eq!(main, BTreeSet::from([id("reader"), id("Streams")]));
    let candidates: BTreeSet<_> = result
        .candidate_visits
        .iter()
        .map(|visit| visit.node.clone())
        .collect();
    assert_eq!(
        candidates,
        BTreeSet::from([id("weak"), id("ambiguous"), id("unresolved")])
    );
    assert!(result.candidate_visits.iter().all(|visit| visit.depth == 1));
    assert!(!main.contains(&id("hidden")) && !candidates.contains(&id("hidden")));
    assert!(!result.truncated);
    assert_eq!(result.candidate_omitted, 0);
}

#[test]
fn zero_candidate_budget_is_explicit_and_legacy_walk_still_follows_candidates() {
    let graph = fixture(false);
    let spec = WalkSpec::new(3, Direction::Outgoing);
    let result = graph.walk_navigation(&id("start"), &spec, 12, 0).unwrap();
    assert!(result.candidate_visits.is_empty());
    assert_eq!(result.candidate_omitted, 3);
    assert!(result.truncated);
    let legacy = graph.walk(&id("start"), &spec).unwrap();
    assert!(legacy.visits.iter().any(|visit| visit.node == id("hidden")));
    assert!(legacy.candidate_visits.is_empty());
    assert_eq!(
        (
            legacy.fanout_omitted,
            legacy.candidate_omitted,
            legacy.depth_omitted
        ),
        (0, 0, 0)
    );
}

#[test]
fn per_node_budget_balances_relation_kinds() {
    let mut delta = GraphDelta::new(project(), 1)
        .add_node(Node::symbol(&project(), "start", "start"))
        .add_node(Node::symbol(&project(), "useful", "useful"))
        .add_edge(&id("start"), &id("useful"), Edge::resolved(EdgeKind::Calls));
    for index in 0..25 {
        let key = format!("type_{index:02}");
        delta = delta
            .add_node(Node::symbol(&project(), &key, &key))
            .add_edge(
                &id("start"),
                &id(&key),
                Edge::resolved(EdgeKind::References),
            );
    }
    let mut graph = CodeGraph::new();
    graph.apply(delta).unwrap();
    let result = graph
        .walk_navigation(&id("start"), &WalkSpec::new(2, Direction::Outgoing), 2, 0)
        .unwrap();
    let reached: Vec<_> = result
        .visits
        .iter()
        .map(|visit| visit.node.clone())
        .collect();
    assert_eq!(reached, [id("useful"), id("type_00")]);
    assert_eq!(result.fanout_omitted, 24);
    assert!(result.truncated);
}

#[test]
fn candidates_share_the_total_node_budget_and_do_not_override_a_strong_path() {
    let mut graph = fixture(false);
    graph
        .apply(GraphDelta::new(project(), 2).add_edge(
            &id("reader"),
            &id("Streams"),
            edge(
                EdgeKind::References,
                EvidenceType::Heuristic,
                Resolution::Resolved,
            ),
        ))
        .unwrap();
    let result = graph
        .walk_navigation(
            &id("start"),
            &WalkSpec::new(3, Direction::Outgoing).with_node_budget(2),
            12,
            8,
        )
        .unwrap();
    assert_eq!(result.visits.len(), 2);
    assert_eq!(result.candidate_omitted, 3);
    // An alternative weak edge to an already admitted node may be inspected
    // without consuming another node or replacing its primary path.
    assert_eq!(result.candidate_visits.len(), 1);
    assert_eq!(result.candidate_visits[0].node, id("Streams"));
    assert!(
        result
            .visits
            .iter()
            .all(|visit| visit.path.iter().all(|hop| {
                hop.resolution == Resolution::Resolved
                    && hop.evidence.at_least(EvidenceType::Syntactic)
            }))
    );
    assert!(result.truncated);
}

#[test]
fn navigation_reports_known_edges_beyond_depth_without_claiming_full_coverage() {
    let mut graph = fixture(false);
    graph
        .apply(GraphDelta::new(project(), 2).add_edge(
            &id("reader"),
            &id("hidden"),
            Edge::resolved(EdgeKind::Calls),
        ))
        .unwrap();
    let result = graph
        .walk_navigation(&id("start"), &WalkSpec::new(1, Direction::Outgoing), 12, 8)
        .unwrap();
    assert_eq!(result.depth_omitted, 1);
    assert!(result.truncated);
    assert!(!result.visits.iter().any(|visit| visit.node == id("hidden")));
    let legacy = graph
        .walk(&id("start"), &WalkSpec::new(1, Direction::Outgoing))
        .unwrap();
    assert!(
        !legacy.truncated,
        "legacy depth remains the requested neighborhood"
    );
}

#[test]
fn navigation_is_deterministic_and_validates_limits() {
    let graph = fixture(false);
    let reverse = fixture(true);
    let spec = WalkSpec::new(3, Direction::Both).with_node_budget(4);
    assert_eq!(
        graph.walk_navigation(&id("start"), &spec, 2, 3).unwrap(),
        reverse.walk_navigation(&id("start"), &spec, 2, 3).unwrap(),
    );
    assert_eq!(
        graph.walk_navigation(&id("start"), &spec, 0, 3),
        Err(GraphError::InvalidLimit("per-node navigation limit")),
    );
    assert_eq!(
        graph.walk_navigation(&id("start"), &spec.clone().with_node_budget(0), 2, 3),
        Err(GraphError::InvalidLimit("node budget")),
    );
    assert_eq!(
        graph.walk_navigation(&id("start"), &WalkSpec::new(6, Direction::Both), 2, 3),
        Err(GraphError::InvalidDepth { given: 6, max: 5 }),
    );
    assert_eq!(
        graph.walk_navigation(&id("absent"), &spec, 2, 3),
        Err(GraphError::UnknownNode(id("absent"))),
    );
}

#[test]
fn bounded_candidates_have_a_complete_tie_break_for_parallel_relation_kinds() {
    let build = |reversed: bool| {
        let mut delta = GraphDelta::new(project(), 1)
            .add_node(Node::symbol(&project(), "start", "start"))
            .add_node(Node::symbol(&project(), "candidate", "candidate"));
        for kind in [EdgeKind::Calls, EdgeKind::References] {
            delta = delta.add_edge(
                &id("start"),
                &id("candidate"),
                edge(kind, EvidenceType::Heuristic, Resolution::Resolved),
            );
        }
        if reversed {
            delta.added_edges.reverse();
        }
        let mut graph = CodeGraph::new();
        graph.apply(delta).unwrap();
        graph
    };
    let spec = WalkSpec::new(2, Direction::Outgoing);
    let forward = build(false)
        .walk_navigation(&id("start"), &spec, 2, 1)
        .unwrap();
    let reverse = build(true)
        .walk_navigation(&id("start"), &spec, 2, 1)
        .unwrap();
    assert_eq!(forward, reverse);
    assert_eq!(forward.candidate_visits.len(), 1);
    assert_eq!(
        forward.candidate_visits[0].path[0].edge.kind,
        EdgeKind::Calls
    );
    assert_eq!(forward.candidate_omitted, 1);
}

#[test]
fn multiple_relation_kinds_to_one_target_do_not_hide_another_neighbor() {
    let mut graph = CodeGraph::new();
    graph
        .apply(
            GraphDelta::new(project(), 1)
                .add_node(Node::symbol(&project(), "start", "start"))
                .add_node(Node::symbol(&project(), "a_shared", "a_shared"))
                .add_node(Node::symbol(&project(), "z_useful", "z_useful"))
                .add_edge(
                    &id("start"),
                    &id("a_shared"),
                    Edge::resolved(EdgeKind::Calls),
                )
                .add_edge(
                    &id("start"),
                    &id("a_shared"),
                    Edge::resolved(EdgeKind::References),
                )
                .add_edge(
                    &id("start"),
                    &id("z_useful"),
                    Edge::resolved(EdgeKind::Calls),
                ),
        )
        .unwrap();
    let result = graph
        .walk_navigation(&id("start"), &WalkSpec::new(2, Direction::Outgoing), 2, 0)
        .unwrap();
    assert_eq!(
        result
            .visits
            .iter()
            .map(|visit| visit.node.clone())
            .collect::<Vec<_>>(),
        [id("a_shared"), id("z_useful")],
    );
    // One parallel relation is absent from the shortest-path tree. Its
    // omission is an edge count; neither of its endpoints is absent.
    assert_eq!(result.fanout_omitted, 1);
    assert!(result.truncated);
}

#[test]
fn depth_counts_include_weak_frontier_edges_without_following_them() {
    let mut graph = CodeGraph::new();
    graph
        .apply(
            GraphDelta::new(project(), 1)
                .add_node(Node::symbol(&project(), "start", "start"))
                .add_node(Node::symbol(&project(), "reader", "reader"))
                .add_node(Node::symbol(&project(), "candidate", "candidate"))
                .add_edge(&id("start"), &id("reader"), Edge::resolved(EdgeKind::Calls))
                .add_edge(
                    &id("reader"),
                    &id("candidate"),
                    edge(
                        EdgeKind::Calls,
                        EvidenceType::Heuristic,
                        Resolution::Ambiguous,
                    ),
                ),
        )
        .unwrap();
    let result = graph
        .walk_navigation(&id("start"), &WalkSpec::new(1, Direction::Outgoing), 12, 8)
        .unwrap();
    assert_eq!(result.depth_omitted, 1);
    assert!(result.candidate_visits.is_empty());
    assert!(result.truncated);
}

#[test]
fn evidence_exclusion_is_independent_of_strength_and_resolution() {
    let graph = fixture(false);
    let filter = knowell_graph::EdgeFilter::any().without_evidence(EvidenceType::SemanticResolved);
    let spec = WalkSpec::new(3, Direction::Outgoing).with_filter(filter);
    let result = graph.walk_navigation(&id("start"), &spec, 12, 8).unwrap();
    assert_eq!(
        result
            .visits
            .iter()
            .map(|visit| visit.node.clone())
            .collect::<Vec<_>>(),
        [id("reader")]
    );
    let candidates: BTreeSet<_> = result
        .candidate_visits
        .iter()
        .map(|visit| visit.node.clone())
        .collect();
    assert_eq!(candidates, BTreeSet::from([id("weak"), id("unresolved")]));
    assert!(
        !result
            .visits
            .iter()
            .any(|visit| visit.node == id("Streams"))
    );
    assert!(
        !result
            .candidate_visits
            .iter()
            .any(|visit| visit.node == id("ambiguous"))
    );
    // Legacy walks still honor explicit exclusions, without acquiring the
    // navigation-only evidence or resolution threshold.
    let legacy = graph.walk(&id("start"), &spec).unwrap();
    assert!(legacy.visits.iter().any(|visit| visit.node == id("weak")));
    assert!(!legacy.visits.iter().any(|visit| visit.node == id("hidden")));
}

#[test]
fn both_frontiers_do_not_spend_two_candidate_slots_on_the_same_edge() {
    let project = project();
    let mut delta = GraphDelta::new(project.clone(), 1);
    for key in ["start", "a", "b", "c"] {
        delta = delta.add_node(Node::symbol(&project, key, key));
    }
    delta = delta
        .add_edge(&id("start"), &id("a"), Edge::resolved(EdgeKind::Calls))
        .add_edge(&id("start"), &id("b"), Edge::resolved(EdgeKind::Calls))
        .add_edge(
            &id("a"),
            &id("b"),
            edge(
                EdgeKind::References,
                EvidenceType::Heuristic,
                Resolution::Resolved,
            ),
        )
        .add_edge(
            &id("b"),
            &id("c"),
            edge(
                EdgeKind::References,
                EvidenceType::Heuristic,
                Resolution::Resolved,
            ),
        );
    let mut graph = CodeGraph::new();
    graph.apply(delta.clone()).unwrap();
    let spec = WalkSpec::new(3, Direction::Both).with_node_budget(4);
    let result = graph.walk_navigation(&id("start"), &spec, 12, 2).unwrap();
    let candidate_edges: BTreeSet<_> = result
        .candidate_visits
        .iter()
        .map(|visit| visit.path.last().unwrap().edge.clone())
        .collect();
    assert_eq!(candidate_edges.len(), 2);
    assert!(
        result
            .candidate_visits
            .iter()
            .any(|visit| visit.node == id("c"))
    );
    assert_eq!(result.candidate_omitted, 0);
    assert!(!result.truncated);

    delta.added_nodes.reverse();
    delta.added_edges.reverse();
    let mut reversed = CodeGraph::new();
    reversed.apply(delta).unwrap();
    assert_eq!(
        result,
        reversed
            .walk_navigation(&id("start"), &spec, 12, 2)
            .unwrap()
    );

    let disabled = graph.walk_navigation(&id("start"), &spec, 12, 0).unwrap();
    assert_eq!(
        disabled.candidate_omitted, 2,
        "a Both observation counts each uncertain edge once"
    );
    assert!(disabled.candidate_visits.is_empty());
    assert!(disabled.truncated);
}
