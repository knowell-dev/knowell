//! Context packing: skeletons first, bodies by rank, budget, overlap,
//! citations, omissions, uncertainties, determinism.

use knowell_query::{
    Candidate, CommitId, ContextPack, EdgeKind, EvidenceRole, EvidenceType, Glossary, OmitReason,
    Origin, QueryPlan, Resolution, SearchConfig, SearchResponse, SnippetKind, SourceError,
    SourceKind, Sources, TaskPackOptions, TaskSelectionStrategy, Uncertainty, pack, pack_task_with,
    pack_with, plan, search,
};

use crate::common::{
    COMMIT, FakeGraph, FakeSnippets, FakeSource, Words, blob, graph_node, hit, name, neighbor,
    no_expansion, pinned, scope, view,
};

use SnippetKind::{Body, Skeleton};

const BODY: &str = "w1 w2 w3 w4 w5 w6 w7 w8 w9 w10";

fn behaviour() -> QueryPlan {
    plan("where do we prevent double payments?", &Glossary::default())
}

fn respond(plan: &QueryPlan, candidates: Vec<Candidate>, config: &SearchConfig) -> SearchResponse {
    let lexical = FakeSource::answering(candidates);
    let sources = Sources {
        lexical: Some(&lexical),
        ..Sources::default()
    };
    search(plan, &scope(&["api"]), &sources, config).unwrap()
}

/// Three results a, b, c; each has a 2-word skeleton (cost 4 + 2 = 6 with the
/// 4-word citation label) and a 10-word body (cost 14).
fn three() -> (SearchResponse, FakeSnippets) {
    let candidates = vec![
        hit(SourceKind::Lexical, 1, "api", "src/a.rs", (1, 10)),
        hit(SourceKind::Lexical, 2, "api", "src/b.rs", (1, 10)),
        hit(SourceKind::Lexical, 3, "api", "src/c.rs", (1, 10)),
    ];
    let response = respond(&behaviour(), candidates, &no_expansion());
    let mut snippets = FakeSnippets::default();
    for (result, name) in response.results.iter().zip(["a", "b", "c"]) {
        snippets.set(&result.location, Skeleton, (1, 1), &format!("fn {name}()"));
        snippets.set(&result.location, Body, (1, 10), BODY);
    }
    (response, snippets)
}

fn shape(pack: &ContextPack) -> Vec<(Origin, SnippetKind, u32)> {
    pack.items
        .iter()
        .map(|i| (i.origin, i.kind, i.tokens))
        .collect()
}

fn rank(n: u32) -> Origin {
    Origin::Result { rank: n }
}

#[test]
fn skeletons_first_then_bodies_by_rank_within_budget() {
    let (response, snippets) = three();
    let pack = pack_with(&response, 30, &snippets, &Words);
    // 3 skeletons (18), then a's body replaces its skeleton (+8 = 26);
    // b's and c's bodies would need 8 more each with 4 left.
    assert_eq!(
        shape(&pack),
        [
            (rank(1), Body, 14),
            (rank(2), Skeleton, 6),
            (rank(3), Skeleton, 6)
        ]
    );
    assert_eq!(pack.used_tokens, 26);
    assert_eq!(pack.budget_tokens, 30);
    let omitted: Vec<(Origin, Option<SnippetKind>, &OmitReason)> = pack
        .omitted
        .iter()
        .map(|o| (o.origin, o.kind, &o.reason))
        .collect();
    let over = OmitReason::OverBudget {
        needed_tokens: 8,
        remaining_tokens: 4,
    };
    assert_eq!(
        omitted,
        [(rank(2), Some(Body), &over), (rank(3), Some(Body), &over)]
    );
    assert_eq!(pack.items[0].text, BODY);
    assert_eq!(pack.items[1].text, "fn b()");
}

#[test]
fn tight_budget_keeps_the_best_ranked_skeletons() {
    let (response, snippets) = three();
    let pack = pack_with(&response, 13, &snippets, &Words);
    assert_eq!(
        shape(&pack),
        [(rank(1), Skeleton, 6), (rank(2), Skeleton, 6)]
    );
    assert_eq!(pack.used_tokens, 12);
    let first = &pack.omitted[0];
    assert_eq!(first.origin, rank(3));
    assert_eq!(first.kind, None, "the whole item is missing");
    assert_eq!(
        first.reason,
        OmitReason::OverBudget {
            needed_tokens: 6,
            remaining_tokens: 1
        }
    );
}

#[test]
fn zero_budget_packs_nothing_and_says_so() {
    let (response, snippets) = three();
    let pack = pack_with(&response, 0, &snippets, &Words);
    assert!(pack.items.is_empty());
    assert_eq!(pack.used_tokens, 0);
    assert_eq!(pack.omitted.len(), 3);
}

#[test]
fn a_body_containing_other_packed_text_covers_it() {
    let mut file_level = hit(SourceKind::Exact, 1, "api", "src/f.rs", (1, 1));
    file_level.range = None;
    let method = hit(SourceKind::Lexical, 1, "api", "src/f.rs", (5, 8));
    let lexical = FakeSource::answering(vec![method]);
    let exact = FakeSource::answering(vec![file_level]);
    let sources = Sources {
        exact: Some(&exact),
        lexical: Some(&lexical),
        ..Sources::default()
    };
    let response = search(&behaviour(), &scope(&["api"]), &sources, &no_expansion()).unwrap();
    assert_eq!(response.results.len(), 2);
    let method_loc = response.results[0].location.clone();
    let file_loc = response.results[1].location.clone();
    assert_eq!(method_loc.range.map(|r| r.start()), Some(5));

    let mut snippets = FakeSnippets::default();
    snippets.set(&method_loc, Skeleton, (5, 5), "fn m()");
    snippets.set(&method_loc, Body, (5, 8), "fn m() { x }");
    snippets.set(&file_loc, Skeleton, (1, 20), "mod f outline");
    snippets.set(&file_loc, Body, (1, 20), &"t ".repeat(20));
    let pack = pack_with(&response, 100, &snippets, &Words);
    assert_eq!(shape(&pack), [(rank(2), Body, 24)]);
    let covered: Vec<(u32, u32)> = pack.items[0]
        .covers
        .iter()
        .map(|c| (c.range.start(), c.range.end()))
        .collect();
    assert_eq!(covered, [(5, 8)]);
    assert_eq!(pack.used_tokens, 24);
    assert!(pack.omitted.is_empty(), "covered text is not missing");
}

#[test]
fn equal_body_text_at_other_bindings_keeps_distinct_citations_and_costs() {
    let body = "pub fn apply() {\n    next();\n}";
    for (project, path) in [("api", "src/alternate.rs"), ("web", "src/apply.rs")] {
        let mut first = hit(SourceKind::Lexical, 1, "api", "src/apply.rs", (1, 3));
        first.content_hash = blob(body);
        let mut second = hit(SourceKind::Lexical, 2, project, path, (1, 3));
        second.content_hash = first.content_hash;
        let mut query_scope = scope(&["api", "web"]);
        if project == "web" {
            second.view = view("release");
            second.generation = 7;
            let pin = &mut query_scope
                .manifest
                .projects
                .get_mut(&name("web"))
                .unwrap()
                .base;
            *pin = pinned("release", 7);
            pin.commit = Some(CommitId::new("abcdef0123456789abcdef0123456789abcdef01").unwrap());
        }
        let lexical = FakeSource::answering(vec![first.clone(), second.clone()]);
        let response = search(
            &behaviour(),
            &query_scope,
            &Sources {
                lexical: Some(&lexical),
                ..Sources::default()
            },
            &no_expansion(),
        )
        .unwrap();
        assert_eq!(response.results.len(), 2);
        let mut snippets = FakeSnippets::default();
        for result in &response.results {
            snippets.set(&result.location, Skeleton, (1, 1), "pub fn apply()");
            snippets.set(&result.location, Body, (1, 3), body);
        }
        let packed = pack_with(&response, 1000, &snippets, &Words);
        assert_eq!(packed.items.len(), 2);
        assert!(packed.omitted.is_empty());
        assert!(
            packed
                .items
                .iter()
                .all(|item| item.kind == Body && item.text == body && item.covers.is_empty())
        );
        for candidate in [first, second] {
            let item = packed
                .items
                .iter()
                .find(|item| {
                    item.citation.project == candidate.project
                        && item.citation.path == candidate.path
                })
                .unwrap();
            assert_eq!(item.citation.view, candidate.view);
            assert_eq!(item.citation.generation, candidate.generation);
            assert_eq!(item.citation.content_hash, candidate.content_hash);
            assert_eq!(item.citation.range, candidate.range.unwrap());
            assert_eq!(
                item.citation.commit,
                query_scope
                    .manifest
                    .projects
                    .get(&candidate.project)
                    .unwrap()
                    .base
                    .commit
            );
        }
        assert_eq!(
            packed.used_tokens,
            packed.items.iter().map(|item| item.tokens).sum::<u32>()
        );
        assert!(packed.used_tokens <= packed.budget_tokens);
    }
}

#[test]
fn identical_body_bytes_at_distinct_call_bindings_support_both_emitted_roles() {
    // `next` binds differently in each module. The source adapter supplies
    // resolved occurrence-level call evidence independently of shared bytes.
    let body = "pub fn apply() {\n    next();\n}";
    let mut subject = hit(SourceKind::Lexical, 1, "api", "src/pipeline/b.rs", (1, 3));
    subject.content_hash = blob(body);
    subject.symbol = Some("B::apply".to_owned());
    let mut caller = graph_node("api", "src/pipeline/a.rs", (1, 3), "A::apply");
    caller.location.content_hash = subject.content_hash;
    let mut graph = FakeGraph::default();
    graph.add(
        "B::apply",
        neighbor(
            caller,
            EdgeKind::Caller,
            EvidenceType::SemanticallyResolved,
            Resolution::Resolved,
        ),
    );
    let lexical = FakeSource::answering(vec![subject]);
    let mut config = SearchConfig::default();
    config.expansion.max_depth = 1;
    let response = search(
        &plan("who calls B::apply", &Glossary::default()),
        &scope(&["api"]),
        &Sources {
            lexical: Some(&lexical),
            graph: Some(&graph),
            ..Sources::default()
        },
        &config,
    )
    .unwrap();
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.expanded.len(), 1);
    let mut snippets = FakeSnippets::default();
    snippets.set(&response.results[0].location, Body, (1, 3), body);
    snippets.set(&response.expanded[0].location, Body, (1, 3), body);
    let ordinary = pack_with(&response, 1000, &snippets, &Words);
    assert_eq!(ordinary.items.len(), 2);
    assert!(
        ordinary
            .items
            .iter()
            .all(|item| item.kind == Body && item.covers.is_empty())
    );
    let selected = pack_task_with(
        &response,
        1000,
        &snippets,
        &Words,
        &TaskPackOptions {
            strategy: TaskSelectionStrategy::BoundedBundles,
            desired_roles: vec![EvidenceRole::Implementation, EvidenceRole::Caller],
            ..TaskPackOptions::default()
        },
    )
    .unwrap();
    assert_eq!(selected.pack.items.len(), 2);
    assert_eq!(selected.selection.selected_candidates, 2);
    assert_eq!(
        selected.selection.covered_roles,
        [EvidenceRole::Implementation, EvidenceRole::Caller]
    );
    assert!(selected.selection.missing_roles.is_empty());
    let calls = selected
        .selection
        .role_evidence
        .iter()
        .find(|evidence| evidence.role == EvidenceRole::Caller)
        .unwrap();
    assert_eq!(calls.citations.len(), 2);
    let paths: std::collections::BTreeSet<_> = calls
        .citations
        .iter()
        .map(|citation| citation.path.as_str())
        .collect();
    assert_eq!(paths, ["src/pipeline/a.rs", "src/pipeline/b.rs"].into());
    for citation in &calls.citations {
        assert!(
            selected
                .pack
                .items
                .iter()
                .any(|item| item.kind == Body && &item.citation == citation)
        );
    }
}

#[test]
fn stale_failed_and_missing_snippets_are_omitted_with_reasons() {
    let (response, mut snippets) = three();
    let a = response.results[0].location.clone();
    let b = response.results[1].location.clone();
    let c = response.results[2].location.clone();
    // a: the source returns a different file version.
    let mut stale = snippets.skeletons.get(&a.label()).cloned().unwrap();
    stale.content_hash = blob("newer version");
    snippets.skeletons.insert(a.label(), stale);
    // b: the source fails.
    snippets.failing.insert(b.label());
    // c: nothing at all.
    snippets.skeletons.remove(&c.label());
    snippets.bodies.remove(&c.label());

    let pack = pack_with(&response, 1_000, &snippets, &Words);
    assert!(pack.items.is_empty());
    let reasons: Vec<&OmitReason> = pack.omitted.iter().map(|o| &o.reason).collect();
    assert_eq!(
        reasons,
        [
            &OmitReason::StaleContent {
                expected: a.content_hash,
                found: blob("newer version")
            },
            &OmitReason::SnippetFailed {
                message: "failed: disk read error".into()
            },
            &OmitReason::NoSnippet,
        ]
    );
}

#[test]
fn citations_pin_view_commit_range_and_hash() {
    let (response, snippets) = three();
    let pack = pack(&response, 10_000, &snippets);
    let citation = &pack.items[0].citation;
    assert_eq!(citation.commit.as_ref().map(|c| c.as_str()), Some(COMMIT));
    assert_eq!(citation.view.as_str(), "main");
    assert_eq!(citation.generation, 1);
    assert_eq!((citation.range.start(), citation.range.end()), (1, 10));
    assert_eq!(
        citation.content_hash,
        response.results[0].location.content_hash
    );
    let label = citation.label();
    assert!(label.starts_with("api@main#1 0123456789ab src/a.rs:L1-L10 "));
    assert!(pack.used_tokens <= pack.budget_tokens);
    assert_eq!(pack.items[0].why, response.results[0].why);
}

#[test]
fn packing_is_deterministic() {
    let (response, snippets) = three();
    for budget in [0, 7, 13, 26, 30, 1_000] {
        let first = pack_with(&response, budget, &snippets, &Words);
        let second = pack_with(&response, budget, &snippets, &Words);
        assert_eq!(first, second);
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
        assert!(first.used_tokens <= budget);
        let items: u32 = first.items.iter().map(|i| i.tokens).sum();
        assert_eq!(
            items, first.used_tokens,
            "used tokens are exactly the items' tokens"
        );
    }
}

#[test]
fn expanded_items_follow_results_and_uncertainties_are_listed() {
    let p = plan("who calls chargeCard", &Glossary::default());
    let mut seed = hit(SourceKind::Lexical, 1, "api", "src/svc.rs", (1, 10));
    seed.symbol = Some("Svc::charge".into());
    let extra = hit(SourceKind::Lexical, 2, "api", "src/extra.rs", (1, 10));
    let mut graph = FakeGraph::default();
    graph.add(
        "Svc::charge",
        neighbor(
            graph_node("api", "src/caller.rs", (3, 9), "Caller::go"),
            EdgeKind::Caller,
            EvidenceType::HeuristicMatch,
            Resolution::Ambiguous,
        ),
    );
    let lexical = FakeSource::answering(vec![seed, extra]);
    let sources = Sources {
        lexical: Some(&lexical),
        graph: Some(&graph),
        ..Sources::default()
    };
    let mut config = SearchConfig::default();
    config.fusion.result_limit = 1;
    let response = search(&p, &scope(&["api"]), &sources, &config).unwrap();
    assert_eq!(response.expanded.len(), 1);

    let mut snippets = FakeSnippets::default();
    snippets.set(
        &response.results[0].location,
        Skeleton,
        (1, 1),
        "fn charge()",
    );
    snippets.set(&response.expanded[0].location, Skeleton, (3, 3), "fn go()");
    let pack = pack_with(&response, 1_000, &snippets, &Words);
    let origins: Vec<Origin> = pack.items.iter().map(|i| i.origin).collect();
    assert_eq!(
        origins,
        [
            rank(1),
            Origin::Expanded {
                seed_rank: 1,
                depth: 1
            }
        ]
    );
    let u = &pack.uncertainties;
    assert!(u.iter().any(|x| matches!(x, Uncertainty::Degraded { degradation } if degradation.to_string() == "semantic: provider not configured")));
    assert!(u.iter().any(|x| matches!(
        x,
        Uncertainty::WeakGraphEvidence {
            evidence: EvidenceType::HeuristicMatch,
            resolution: Resolution::Ambiguous,
            ..
        }
    )));
    assert!(u.contains(&Uncertainty::MoreResults { truncated: 1 }));
}

#[test]
fn empty_results_become_an_uncertainty() {
    let lexical = FakeSource::failing(SourceError::Unavailable("index not built".into()));
    let sources = Sources {
        lexical: Some(&lexical),
        ..Sources::default()
    };
    let response = search(&behaviour(), &scope(&["api"]), &sources, &no_expansion()).unwrap();
    let pack = pack(&response, 100, &FakeSnippets::default());
    assert!(pack.items.is_empty());
    assert!(
        pack.uncertainties
            .iter()
            .any(|u| matches!(u, Uncertainty::NoResults { .. }))
    );
}
