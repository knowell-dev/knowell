//! Delta application, fencing, neighbors and bounded walks.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::*;
use knowell_graph::{
    CodeGraph, ContractKind, Direction, EdgeFilter, EdgeKey, EdgeKind, EvidenceType, GraphDelta,
    GraphError, Node, NodeId, WalkSpec,
};
use pretty_assertions::assert_eq;

#[test]
fn fixture_applies_and_stamps_generations() {
    let graph = fixture();
    let i = ids();
    assert_eq!(graph.generation(&name("billing")), 1);
    assert_eq!(graph.generation(&name("deploy")), 1);
    assert_eq!(graph.generation(&name("unknown")), 0);
    assert_eq!(graph.node(&i.service).unwrap().generation, 1);
    assert!(graph.node(&i.endpoint).is_some());
    assert!(
        graph
            .edge(&EdgeKey {
                from: i.controller.clone(),
                to: i.endpoint.clone(),
                kind: EdgeKind::Exposes
            })
            .is_some()
    );
    let projects: Vec<String> = graph.generations().map(|(n, _)| n.to_string()).collect();
    assert_eq!(projects, ["billing", "deploy", "web", "worker"]);
}

#[test]
fn summary_counts_changes() {
    let mut graph = CodeGraph::new();
    let p = name("billing");
    let a = sym("billing", "a");
    let b = sym("billing", "b");
    let delta = GraphDelta::new(p.clone(), 1)
        .add_node(symbol_node("billing", "a", "a"))
        .add_node(symbol_node("billing", "b", "b"))
        .add_edge(&a, &b, strong(EdgeKind::Calls));
    let s = graph.apply(delta).unwrap();
    assert_eq!((s.nodes_added, s.edges_added), (2, 1));

    let delta = GraphDelta::new(p, 2)
        .add_node(symbol_node("billing", "a", "renamed"))
        .add_edge(&a, &b, heuristic(EdgeKind::Calls));
    let s = graph.apply(delta).unwrap();
    assert_eq!((s.nodes_replaced, s.edges_replaced), (1, 1));
    assert_eq!(graph.node(&a).unwrap().name, "renamed");
    assert_eq!(graph.node(&a).unwrap().generation, 2);
    // The untouched node keeps its old stamp.
    assert_eq!(graph.node(&b).unwrap().generation, 1);
    assert_eq!(graph.edge_count(), 1);
}

#[test]
fn stale_and_repeated_generations_are_rejected_without_change() {
    let mut graph = fixture();
    let before = (graph.node_count(), graph.edge_count());
    let i = ids();

    for generation in [0, 1] {
        let delta = GraphDelta::new(name("billing"), generation)
            .remove_node(&i.report)
            .add_node(symbol_node("billing", "late", "late"));
        let err = graph.apply(delta).unwrap_err();
        assert_eq!(
            err,
            GraphError::StaleGeneration {
                project: name("billing"),
                current: 1,
                got: generation
            }
        );
    }
    assert_eq!((graph.node_count(), graph.edge_count()), before);
    assert!(graph.node(&i.report).is_some());

    // Another project's generation is independent.
    let delta = GraphDelta::new(name("fresh"), 1).add_node(symbol_node("fresh", "x", "x"));
    assert!(graph.apply(delta).is_ok());
}

#[test]
fn failed_delta_is_atomic() {
    let mut graph = fixture();
    let before = (graph.node_count(), graph.edge_count());
    let i = ids();
    let ghost = sym("billing", "ghost");
    let delta = GraphDelta::new(name("billing"), 2)
        .add_node(symbol_node("billing", "ok", "ok"))
        .remove_node(&i.report)
        .add_edge(&i.service, &ghost, strong(EdgeKind::Calls));
    assert!(matches!(
        graph.apply(delta),
        Err(GraphError::DanglingEdge { .. })
    ));
    assert_eq!((graph.node_count(), graph.edge_count()), before);
    assert!(graph.node(&sym("billing", "ok")).is_none());
    assert!(graph.node(&i.report).is_some());
    assert_eq!(graph.generation(&name("billing")), 1);
}

#[test]
fn ownership_is_enforced() {
    let mut graph = fixture();
    let i = ids();

    // A project cannot add a node claiming another project.
    let delta = GraphDelta::new(name("web"), 2).add_node(symbol_node("billing", "x", "x"));
    assert!(matches!(
        graph.apply(delta),
        Err(GraphError::ForeignNode { .. })
    ));

    // Nor replace or remove another project's node.
    let delta = GraphDelta::new(name("web"), 2).remove_node(&i.service);
    assert!(matches!(
        graph.apply(delta),
        Err(GraphError::ForeignNode { .. })
    ));
    let delta = GraphDelta::new(name("web"), 2).add_node(Node::contract(ContractKind::Table, "t"));
    assert!(graph.apply(delta).is_ok());

    // An edge between two nodes of another project is not web's to add or remove.
    let delta =
        GraphDelta::new(name("web"), 3).add_edge(&i.service, &i.repo, strong(EdgeKind::References));
    assert!(matches!(
        graph.apply(delta),
        Err(GraphError::ForeignEdge { .. })
    ));
    let key = EdgeKey {
        from: i.service.clone(),
        to: i.repo.clone(),
        kind: EdgeKind::Calls,
    };
    let delta = GraphDelta::new(name("web"), 3).remove_edge(key);
    assert!(matches!(
        graph.apply(delta),
        Err(GraphError::ForeignEdge { .. })
    ));
}

#[test]
fn unknown_and_duplicate_entries_are_rejected() {
    let mut graph = fixture();
    let i = ids();
    let missing = sym("billing", "missing");

    let delta = GraphDelta::new(name("billing"), 2).remove_node(&missing);
    assert_eq!(
        graph.apply(delta),
        Err(GraphError::UnknownNode(missing.clone()))
    );

    let key = EdgeKey {
        from: i.service.clone(),
        to: missing,
        kind: EdgeKind::Calls,
    };
    let delta = GraphDelta::new(name("billing"), 2).remove_edge(key.clone());
    assert_eq!(graph.apply(delta), Err(GraphError::UnknownEdge(key)));

    let delta = GraphDelta::new(name("billing"), 2)
        .add_node(symbol_node("billing", "d", "d"))
        .add_node(symbol_node("billing", "d", "d"));
    assert!(matches!(
        graph.apply(delta),
        Err(GraphError::DuplicateInDelta(_))
    ));

    let delta = GraphDelta::new(name("billing"), 2)
        .add_node(symbol_node("billing", "d", "d"))
        .remove_node(&sym("billing", "d"));
    assert!(matches!(
        graph.apply(delta),
        Err(GraphError::DuplicateInDelta(_) | GraphError::UnknownNode(_))
    ));
}

#[test]
fn removing_a_node_removes_its_edges_across_projects() {
    let mut graph = fixture();
    let i = ids();
    let edges_before = graph.edge_count();
    // billing re-index drops the service: every edge touching it goes, including web's helper edge.
    let delta = GraphDelta::new(name("billing"), 2).remove_node(&i.service);
    let summary = graph.apply(delta).unwrap();
    assert_eq!(summary.nodes_removed, 1);
    assert_eq!(summary.edges_removed, 6);
    assert_eq!(graph.edge_count(), edges_before - 6);
    assert!(graph.node(&i.service).is_none());
    assert_eq!(
        graph
            .neighbors(&i.service, Direction::Both, &EdgeFilter::any())
            .err(),
        Some(GraphError::UnknownNode(i.service.clone()))
    );
    // The topic survives, now without a producer.
    assert!(graph.node(&i.topic).is_some());
}

#[test]
fn explicit_edge_removal_and_node_reuse() {
    let mut graph = fixture();
    let i = ids();
    let key = EdgeKey {
        from: i.ui.clone(),
        to: i.helper.clone(),
        kind: EdgeKind::Calls,
    };
    let delta = GraphDelta::new(name("web"), 2).remove_edge(key.clone());
    assert_eq!(graph.apply(delta).unwrap().edges_removed, 1);
    assert!(graph.edge(&key).is_none());
    // Re-adding works.
    let delta =
        GraphDelta::new(name("web"), 3).add_edge(&i.ui, &i.helper, heuristic(EdgeKind::Calls));
    assert_eq!(graph.apply(delta).unwrap().edges_added, 1);
}

#[test]
fn node_id_rejects_empty() {
    assert_eq!(NodeId::new(""), Err(GraphError::EmptyNodeId));
    assert_eq!(NodeId::new("x").unwrap().as_str(), "x");
}

#[test]
fn neighbors_are_filtered_and_ordered() {
    let graph = fixture();
    let i = ids();

    let all = graph
        .neighbors(&i.service, Direction::Both, &EdgeFilter::any())
        .unwrap();
    let described: Vec<(String, String, bool)> = all
        .iter()
        .map(|n| {
            (
                n.edge.kind.as_str().to_owned(),
                n.node.name.clone(),
                n.forward,
            )
        })
        .collect();
    assert_eq!(
        described,
        [
            // sorted by edge kind, then other node id, outgoing first
            ("calls".to_owned(), "handle".to_owned(), false),
            ("calls".to_owned(), "find".to_owned(), true),
            ("calls".to_owned(), "helper".to_owned(), false),
            ("tests".to_owned(), "cancel_works".to_owned(), false),
            (
                "produces".to_owned(),
                "subscription.cancelled".to_owned(),
                true
            ),
            ("writes".to_owned(), "subscriptions".to_owned(), true),
        ]
    );

    let incoming_calls = graph
        .neighbors(
            &i.service,
            Direction::Incoming,
            &EdgeFilter::any().with_kinds([EdgeKind::Calls]),
        )
        .unwrap();
    assert_eq!(incoming_calls.len(), 2);

    // Evidence threshold removes the heuristic caller.
    let strong_callers = graph
        .neighbors(
            &i.service,
            Direction::Incoming,
            &EdgeFilter::any()
                .with_kinds([EdgeKind::Calls])
                .with_min_evidence(EvidenceType::Syntactic),
        )
        .unwrap();
    assert_eq!(strong_callers.len(), 1);
    assert_eq!(strong_callers[0].node.id, i.controller);

    assert_eq!(
        graph
            .neighbors(&i.service, Direction::Outgoing, &EdgeFilter::any())
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn neighbors_filter_by_resolution() {
    let mut graph = fixture();
    let i = ids();
    let delta =
        GraphDelta::new(name("web"), 2).add_edge(&i.ui, &i.service, unresolved(EdgeKind::Calls));
    graph.apply(delta).unwrap();
    let any = graph
        .neighbors(
            &i.service,
            Direction::Incoming,
            &EdgeFilter::any().with_kinds([EdgeKind::Calls]),
        )
        .unwrap();
    assert_eq!(any.len(), 3);
    let resolved = graph
        .neighbors(
            &i.service,
            Direction::Incoming,
            &EdgeFilter::any()
                .with_kinds([EdgeKind::Calls])
                .resolved_only(),
        )
        .unwrap();
    assert_eq!(resolved.len(), 2);
}

#[test]
fn self_loops_are_listed_once() {
    let mut graph = CodeGraph::new();
    let a = sym("billing", "rec");
    let delta = GraphDelta::new(name("billing"), 1)
        .add_node(symbol_node("billing", "rec", "rec"))
        .add_edge(&a, &a, strong(EdgeKind::Calls));
    graph.apply(delta).unwrap();
    assert_eq!(
        graph
            .neighbors(&a, Direction::Both, &EdgeFilter::any())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn walk_respects_depth_and_reports_shortest_paths() {
    let graph = fixture();
    let i = ids();

    let one = graph
        .walk(&i.endpoint, &WalkSpec::new(1, Direction::Both))
        .unwrap();
    let reached: Vec<&NodeId> = one.visits.iter().map(|v| &v.node).collect();
    assert_eq!(reached, [&i.ui, &i.controller]);
    assert!(one.visits.iter().all(|v| v.depth == 1 && v.path.len() == 1));
    assert!(!one.truncated);

    let two = graph
        .walk(&i.endpoint, &WalkSpec::new(2, Direction::Both))
        .unwrap();
    let service = two.visits.iter().find(|v| v.node == i.service).unwrap();
    assert_eq!(service.depth, 2);
    assert_eq!(service.path.len(), 2);
    assert_eq!(service.path[0].from, i.endpoint);
    assert_eq!(service.path[0].to, i.controller);
    assert!(!service.path[0].forward); // exposes edge walked backwards
    assert_eq!(service.path[1].to, i.service);
    assert!(service.path[1].forward);
    // Nothing beyond depth 2 is returned.
    assert!(two.visits.iter().all(|v| v.depth <= 2));
    assert!(two.visits.iter().all(|v| v.node != i.topic));

    let deep = graph
        .walk(&i.endpoint, &WalkSpec::new(5, Direction::Outgoing))
        .unwrap();
    assert!(deep.visits.is_empty(), "the endpoint has no outgoing edges");
}

#[test]
fn walk_directions_differ() {
    let graph = fixture();
    let i = ids();
    let out = graph
        .walk(&i.service, &WalkSpec::new(1, Direction::Outgoing))
        .unwrap();
    let inc = graph
        .walk(&i.service, &WalkSpec::new(1, Direction::Incoming))
        .unwrap();
    assert_eq!(out.visits.len(), 3);
    assert_eq!(inc.visits.len(), 3);
    let out_ids: Vec<&NodeId> = out.visits.iter().map(|v| &v.node).collect();
    assert!(out_ids.contains(&&i.topic));
    assert!(out.visits.iter().all(|v| v.path.iter().all(|h| h.forward)));
    assert!(inc.visits.iter().all(|v| v.path.iter().all(|h| !h.forward)));
}

#[test]
fn walk_budget_truncates_deterministically() {
    let graph = fixture();
    let i = ids();
    let spec = WalkSpec::new(5, Direction::Both).with_node_budget(3);
    let a = graph.walk(&i.service, &spec).unwrap();
    let b = graph.walk(&i.service, &spec).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.visits.len(), 3);
    assert!(a.truncated);

    let roomy = graph
        .walk(
            &i.service,
            &WalkSpec::new(5, Direction::Both).with_node_budget(10_000),
        )
        .unwrap();
    assert!(!roomy.truncated);
    // The truncated result is a prefix of the full breadth-first order.
    assert_eq!(&roomy.visits[..3], &a.visits[..]);
}

#[test]
fn walk_filters_edges() {
    let graph = fixture();
    let i = ids();
    let spec = WalkSpec::new(5, Direction::Both)
        .with_filter(EdgeFilter::any().with_kinds([EdgeKind::Calls]));
    let result = graph.walk(&i.service, &spec).unwrap();
    let reached: Vec<&NodeId> = result.visits.iter().map(|v| &v.node).collect();
    assert!(reached.contains(&&i.controller));
    assert!(reached.contains(&&i.ui)); // via the heuristic helper
    assert!(!reached.contains(&&i.topic));

    let strict = WalkSpec::new(5, Direction::Both).with_filter(
        EdgeFilter::any()
            .with_kinds([EdgeKind::Calls])
            .with_min_evidence(EvidenceType::SemanticResolved),
    );
    let result = graph.walk(&i.service, &strict).unwrap();
    let reached: Vec<&NodeId> = result.visits.iter().map(|v| &v.node).collect();
    assert!(!reached.contains(&&i.helper));
}

#[test]
fn walk_validates_arguments() {
    let graph = fixture();
    let i = ids();
    for depth in [0, 6] {
        assert_eq!(
            graph.walk(&i.service, &WalkSpec::new(depth, Direction::Both)),
            Err(GraphError::InvalidDepth {
                given: depth,
                max: 5
            })
        );
    }
    assert_eq!(
        graph.walk(
            &i.service,
            &WalkSpec::new(2, Direction::Both).with_node_budget(0)
        ),
        Err(GraphError::InvalidLimit("node budget"))
    );
    let ghost = sym("billing", "ghost");
    assert_eq!(
        graph.walk(&ghost, &WalkSpec::new(2, Direction::Both)),
        Err(GraphError::UnknownNode(ghost))
    );
}

#[test]
fn walk_is_independent_of_insertion_order() {
    let forward = fixture_with(false);
    let reversed = fixture_with(true);
    let i = ids();
    let spec = WalkSpec::new(4, Direction::Both);
    assert_eq!(
        forward.walk(&i.service, &spec).unwrap(),
        reversed.walk(&i.service, &spec).unwrap()
    );
}
