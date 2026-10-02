//! Graph expansion: depth, node budget, fanout, edge kinds, scope, paths
//! and annotation of existing results.

use knowell_query::{
    Candidate, EdgeKind, EvidenceType, Glossary, QueryPlan, QueryScope, Reason, Resolution,
    SearchConfig, SearchResponse, SourceError, SourceKind, Sources, plan, search,
};

use crate::common::{FakeGraph, FakeSource, approx, graph_node, hit, name, neighbor, scope};

use EdgeKind::{Caller, Doc, Test};
use EvidenceType::{ContractDerived, HeuristicMatch, SemanticallyResolved, SyntacticObservation};
use Resolution::{Ambiguous, Resolved};

fn impact() -> QueryPlan {
    let p = plan("who calls chargeCard", &Glossary::default());
    assert_eq!(p.intent, knowell_query::Intent::Impact);
    p
}

fn seed(rank: u32, file: &str, symbol: &str) -> Candidate {
    let mut c = hit(SourceKind::Lexical, rank, "api", file, (1, 10));
    c.symbol = Some(symbol.to_owned());
    c
}

fn run(
    candidates: Vec<Candidate>,
    graph: &FakeGraph,
    s: &QueryScope,
    config: &SearchConfig,
) -> SearchResponse {
    let lexical = FakeSource::answering(candidates);
    let sources = Sources {
        lexical: Some(&lexical),
        graph: Some(graph),
        ..Sources::default()
    };
    search(&impact(), s, &sources, config).unwrap()
}

fn expanded_paths(response: &SearchResponse) -> Vec<&str> {
    response
        .expanded
        .iter()
        .map(|e| e.location.path.as_str())
        .collect()
}

fn chain() -> FakeGraph {
    let mut g = FakeGraph::default();
    g.add(
        "Svc::charge",
        neighbor(
            graph_node("api", "src/a.rs", (1, 5), "A::run"),
            Caller,
            SemanticallyResolved,
            Resolved,
        ),
    );
    g.add(
        "A::run",
        neighbor(
            graph_node("api", "src/b.rs", (1, 5), "B::run"),
            Caller,
            SyntacticObservation,
            Resolved,
        ),
    );
    g.add(
        "B::run",
        neighbor(
            graph_node("api", "src/c.rs", (1, 5), "C::run"),
            Caller,
            HeuristicMatch,
            Ambiguous,
        ),
    );
    g
}

#[test]
fn expansion_is_bounded_by_depth_and_carries_its_path() {
    let graph = chain();
    let mut config = SearchConfig::default();
    config.expansion.max_depth = 2;
    let response = run(
        vec![seed(1, "src/svc.rs", "Svc::charge")],
        &graph,
        &scope(&["api"]),
        &config,
    );
    assert_eq!(expanded_paths(&response), ["src/a.rs", "src/b.rs"]);
    assert_eq!(graph.calls.get(), 2);

    let seed_score = response.results[0].score.fused;
    let a = &response.expanded[0];
    let b = &response.expanded[1];
    assert_eq!((a.depth, b.depth), (1, 2));
    assert_eq!((a.seed_rank, b.seed_rank), (1, 1));
    assert!(approx(a.score, seed_score * 0.5));
    assert!(approx(b.score, seed_score * 0.25));
    let steps: Vec<(EdgeKind, EvidenceType)> =
        b.path.iter().map(|s| (s.edge, s.evidence)).collect();
    assert_eq!(
        steps,
        [
            (Caller, SemanticallyResolved),
            (Caller, SyntacticObservation)
        ]
    );
    assert_eq!(b.path[0].symbol.as_deref(), Some("A::run"));
    assert!(matches!(
        &b.why[0],
        Reason::GraphPath { seed, steps } if seed == &response.results[0].location && steps.len() == 2
    ));
    assert!(b.commit.is_some());

    config.expansion.max_depth = 3;
    let response = run(
        vec![seed(1, "src/svc.rs", "Svc::charge")],
        &chain(),
        &scope(&["api"]),
        &config,
    );
    assert_eq!(
        expanded_paths(&response),
        ["src/a.rs", "src/b.rs", "src/c.rs"]
    );
    assert!(response.expanded[2].path[2].is_uncertain());
}

#[test]
fn node_budget_stops_expansion_and_says_so() {
    let mut graph = FakeGraph::default();
    for i in (1..=5).rev() {
        graph.add(
            "Svc::charge",
            neighbor(
                graph_node("api", &format!("src/n{i}.rs"), (1, 5), &format!("N{i}")),
                Caller,
                SemanticallyResolved,
                Resolved,
            ),
        );
    }
    let mut config = SearchConfig::default();
    config.expansion.node_budget = 3;
    let response = run(
        vec![seed(1, "src/svc.rs", "Svc::charge")],
        &graph,
        &scope(&["api"]),
        &config,
    );
    assert_eq!(
        expanded_paths(&response),
        ["src/n1.rs", "src/n2.rs", "src/n3.rs"]
    );
    let stats = &response.stats.expansion;
    assert_eq!(stats.added, 3);
    assert_eq!(stats.skipped_by_budget, 2);
    assert!(stats.budget_exhausted);
    assert_eq!(stats.node_budget, 3);
}

#[test]
fn fanout_keeps_the_strongest_evidence_deterministically() {
    let mut graph = FakeGraph::default();
    graph.add(
        "Svc::charge",
        neighbor(
            graph_node("api", "src/n1.rs", (1, 5), "N1"),
            Caller,
            HeuristicMatch,
            Resolved,
        ),
    );
    graph.add(
        "Svc::charge",
        neighbor(
            graph_node("api", "src/n2.rs", (1, 5), "N2"),
            Caller,
            SemanticallyResolved,
            Resolved,
        ),
    );
    graph.add(
        "Svc::charge",
        neighbor(
            graph_node("api", "src/n3.rs", (1, 5), "N3"),
            Caller,
            ContractDerived,
            Resolved,
        ),
    );
    let mut config = SearchConfig::default();
    config.expansion.fanout = 2;
    let response = run(
        vec![seed(1, "src/svc.rs", "Svc::charge")],
        &graph,
        &scope(&["api"]),
        &config,
    );
    assert_eq!(expanded_paths(&response), ["src/n2.rs", "src/n3.rs"]);
}

#[test]
fn unrequested_edge_kinds_are_ignored() {
    let mut graph = FakeGraph::default();
    graph.add(
        "Svc::charge",
        neighbor(
            graph_node("api", "docs/adr.md", (1, 5), "ADR-7"),
            Doc,
            ContractDerived,
            Resolved,
        ),
    );
    graph.add(
        "Svc::charge",
        neighbor(
            graph_node("api", "src/n.rs", (1, 5), "N"),
            Caller,
            SemanticallyResolved,
            Resolved,
        ),
    );
    let response = run(
        vec![seed(1, "src/svc.rs", "Svc::charge")],
        &graph,
        &scope(&["api"]),
        &SearchConfig::default(),
    );
    assert_eq!(expanded_paths(&response), ["src/n.rs"]);
    assert_eq!(response.stats.expansion.ignored_edges, 1);
}

#[test]
fn neighbours_that_are_results_annotate_them_and_tests_say_what_they_reference() {
    let mut graph = FakeGraph::default();
    graph.add(
        "Svc::charge",
        neighbor(
            graph_node("api", "tests/svc_test.rs", (1, 10), "svc_test"),
            Test,
            SemanticallyResolved,
            Resolved,
        ),
    );
    graph.add(
        "Svc::charge",
        neighbor(
            graph_node("api", "tests/other_test.rs", (1, 5), "other_test"),
            Test,
            SyntacticObservation,
            Resolved,
        ),
    );
    let candidates = vec![
        seed(1, "src/svc.rs", "Svc::charge"),
        seed(2, "tests/svc_test.rs", "svc_test"),
    ];
    let response = run(
        candidates,
        &graph,
        &scope(&["api"]),
        &SearchConfig::default(),
    );
    assert_eq!(expanded_paths(&response), ["tests/other_test.rs"]);
    assert_eq!(response.stats.expansion.annotated, 1);
    let test_result = &response.results[1];
    assert!(
        test_result
            .why
            .iter()
            .any(|r| matches!(r, Reason::GraphPath { .. }))
    );
    assert!(test_result.why.contains(&Reason::TestReferences {
        subject: "Svc::charge".into()
    }));
    assert!(response.expanded[0].why.contains(&Reason::TestReferences {
        subject: "Svc::charge".into()
    }));
}

#[test]
fn neighbours_outside_pins_or_scope_are_dropped() {
    let mut graph = FakeGraph::default();
    let mut stale = graph_node("api", "src/stale.rs", (1, 5), "Stale");
    stale.location.generation = 5;
    for node in [
        graph_node("ghost", "src/g.rs", (1, 5), "G"),
        graph_node("web", "src/w.rs", (1, 5), "W"),
        stale,
        graph_node("api", "src/ok.rs", (1, 5), "Ok"),
    ] {
        graph.add(
            "Svc::charge",
            neighbor(node, Caller, SemanticallyResolved, Resolved),
        );
    }
    let mut s = scope(&["api", "web"]);
    s.projects = Some([name("api")].into());
    let response = run(
        vec![seed(1, "src/svc.rs", "Svc::charge")],
        &graph,
        &s,
        &SearchConfig::default(),
    );
    assert_eq!(expanded_paths(&response), ["src/ok.rs"]);
    let stats = &response.stats.expansion;
    assert_eq!(stats.dropped_outside_pinned_views, 2);
    assert_eq!(stats.dropped_by_scope, 1);
}

#[test]
fn shared_neighbours_are_added_once() {
    let mut graph = FakeGraph::default();
    for from in ["Svc::charge", "Svc::refund"] {
        graph.add(
            from,
            neighbor(
                graph_node("api", "src/shared.rs", (1, 5), "Shared"),
                Caller,
                SemanticallyResolved,
                Resolved,
            ),
        );
    }
    let candidates = vec![
        seed(1, "src/svc.rs", "Svc::charge"),
        seed(2, "src/refund.rs", "Svc::refund"),
    ];
    let response = run(
        candidates,
        &graph,
        &scope(&["api"]),
        &SearchConfig::default(),
    );
    assert_eq!(expanded_paths(&response), ["src/shared.rs"]);
    assert_eq!(response.expanded[0].seed_rank, 1);
    assert_eq!(response.stats.expansion.duplicates, 1);
}

#[test]
fn expander_failure_keeps_results_and_reports_it() {
    let graph = FakeGraph {
        fail: Some(SourceError::Failed("graph store down".into())),
        ..FakeGraph::default()
    };
    let response = run(
        vec![seed(1, "src/svc.rs", "Svc::charge")],
        &graph,
        &scope(&["api"]),
        &SearchConfig::default(),
    );
    assert_eq!(response.results.len(), 1);
    assert!(response.expanded.is_empty());
    assert!(
        response
            .degraded
            .iter()
            .any(|d| d.to_string() == "graph: failed: graph store down")
    );
    assert_eq!(
        graph.calls.get(),
        1,
        "a failing expander is not retried per node"
    );
}
