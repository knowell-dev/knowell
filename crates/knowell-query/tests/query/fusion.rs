//! Fusion maths, tie-breaks, quotas, de-duplication, overlay shadowing,
//! manifest and scope filtering.

use std::collections::BTreeSet;

use knowell_query::{
    Candidate, Glossary, GlossaryEntry, Layer, MatchDetail, OverlayPin, PathFilter, PathGlob,
    QueryPlan, QueryScope, Reason, SearchConfig, SearchResponse, SourceKind, Sources, TermRelation,
    ViewId, plan, search,
};

use crate::common::{
    COMMIT, FakeSource, approx, blob, hit, lang, name, no_expansion, path, pinned, scope, view,
};

use SourceKind::{Exact, Lexical, Semantic};

fn run(
    plan: &QueryPlan,
    scope: &QueryScope,
    lists: [Vec<Candidate>; 3],
    config: &SearchConfig,
) -> SearchResponse {
    let [exact, lexical, semantic] = lists;
    let exact = FakeSource::answering(exact);
    let lexical = FakeSource::answering(lexical);
    let semantic = FakeSource::answering(semantic);
    let sources = Sources {
        exact: Some(&exact),
        lexical: Some(&lexical),
        semantic: Some(&semantic),
        ..Sources::default()
    };
    search(plan, scope, &sources, config).unwrap()
}

fn paths(response: &SearchResponse) -> Vec<&str> {
    response
        .results
        .iter()
        .map(|r| r.location.path.as_str())
        .collect()
}

/// A: lexical #1 + semantic #3; B: semantic #1; C: exact #1 + lexical #2; D: semantic #2.
fn rrf_lists() -> [Vec<Candidate>; 3] {
    [
        vec![hit(Exact, 1, "api", "src/c.rs", (1, 10))],
        vec![
            hit(Lexical, 1, "api", "src/a.rs", (1, 10)),
            hit(Lexical, 2, "api", "src/c.rs", (1, 10)),
        ],
        vec![
            hit(Semantic, 1, "api", "src/b.rs", (1, 10)),
            hit(Semantic, 2, "api", "src/d.rs", (1, 10)),
            hit(Semantic, 3, "api", "src/a.rs", (1, 10)),
        ],
    ]
}

#[test]
fn weighted_rrf_matches_hand_computation_for_behaviour_queries() {
    let p = plan("how is the invoice total calculated", &Glossary::default());
    let response = run(&p, &scope(&["api"]), rrf_lists(), &no_expansion());
    // Behaviour weights: exact 0.4, lexical 0.6, semantic 1.0; k = 60.
    let a = 0.6 / 61.0 + 1.0 / 63.0; // 0.025709…
    let b = 1.0 / 61.0; // 0.016393…
    let c = 0.4 / 61.0 + 0.6 / 62.0; // 0.016234…
    let d = 1.0 / 62.0; // 0.016129…
    assert_eq!(
        paths(&response),
        ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"]
    );
    for (result, expected) in response.results.iter().zip([a, b, c, d]) {
        assert!(
            approx(result.score.fused, expected),
            "{}",
            result.score.fused
        );
        assert_eq!(result.score.rrf_k, 60);
        let sum: f64 = result.score.sources.iter().map(|s| s.contribution).sum();
        assert!(approx(sum, result.score.fused));
    }
    let a_sources: Vec<(SourceKind, u32)> = response.results[0]
        .score
        .sources
        .iter()
        .map(|s| (s.source, s.rank))
        .collect();
    assert_eq!(a_sources, [(Lexical, 1), (Semantic, 3)]);
    assert!(approx(response.results[0].score.sources[0].weight, 0.6));
    let ranks: Vec<u32> = response.results.iter().map(|r| r.rank).collect();
    assert_eq!(ranks, [1, 2, 3, 4]);
}

#[test]
fn exact_symbol_queries_weight_exact_and_lexical_higher() {
    let p = plan("cancelSubscription", &Glossary::default());
    let response = run(&p, &scope(&["api"]), rrf_lists(), &no_expansion());
    // Exact-symbol weights: exact 1.0, lexical 0.8, semantic 0.3.
    let c = 1.0 / 61.0 + 0.8 / 62.0;
    let a = 0.8 / 61.0 + 0.3 / 63.0;
    let b = 0.3 / 61.0;
    let d = 0.3 / 62.0;
    assert_eq!(
        paths(&response),
        ["src/c.rs", "src/a.rs", "src/b.rs", "src/d.rs"]
    );
    for (result, expected) in response.results.iter().zip([c, a, b, d]) {
        assert!(approx(result.score.fused, expected));
    }
}

#[test]
fn rrf_constant_is_configurable() {
    let p = plan("how is the invoice total calculated", &Glossary::default());
    let mut config = no_expansion();
    config.fusion.rrf_k = 0;
    let response = run(&p, &scope(&["api"]), rrf_lists(), &config);
    // k = 0: A = 0.6/1 + 1/3, B = 1/1, C = 0.4/1 + 0.6/2, D = 1/2.
    assert_eq!(
        paths(&response),
        ["src/b.rs", "src/a.rs", "src/c.rs", "src/d.rs"]
    );
    assert!(approx(response.results[1].score.fused, 0.6 + 1.0 / 3.0));
}

#[test]
fn equal_scores_break_ties_by_location_regardless_of_input_order() {
    let p = plan("how is the invoice total calculated", &Glossary::default());
    let lexical = vec![
        hit(Lexical, 1, "web", "src/z.rs", (1, 5)),
        hit(Lexical, 1, "api", "src/z.rs", (1, 5)),
        hit(Lexical, 1, "api", "src/y.rs", (1, 5)),
    ];
    let mut reversed = lexical.clone();
    reversed.reverse();
    let s = scope(&["api", "web"]);
    let first = run(&p, &s, [vec![], lexical, vec![]], &no_expansion());
    let second = run(&p, &s, [vec![], reversed, vec![]], &no_expansion());
    let order: Vec<String> = first.results.iter().map(|r| r.id.clone()).collect();
    assert!(order[0].starts_with("api@main#1:src/y.rs"));
    assert!(order[1].starts_with("api@main#1:src/z.rs"));
    assert!(order[2].starts_with("web@main#1:src/z.rs"));
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap()
    );
}

#[test]
fn candidate_quota_limits_each_project_per_source() {
    let p = plan("how is the invoice total calculated", &Glossary::default());
    let mut lexical: Vec<Candidate> = (1..=5)
        .map(|rank| hit(Lexical, rank, "big", &format!("src/b{rank}.rs"), (1, 5)))
        .collect();
    lexical.push(hit(Lexical, 6, "small", "src/s.rs", (1, 5)));
    let mut config = no_expansion();
    config.fusion.candidate_quota_per_project = Some(2);
    config.fusion.result_quota_per_project = None;

    let source = FakeSource::answering(lexical);
    let sources = Sources {
        lexical: Some(&source),
        ..Sources::default()
    };
    let response = search(&p, &scope(&["big", "small"]), &sources, &config).unwrap();
    assert_eq!(paths(&response), ["src/b1.rs", "src/b2.rs", "src/s.rs"]);
    assert_eq!(response.stats.dropped.candidate_quota, 3);
    assert_eq!(
        source.limits.get(),
        Some((100, Some(2))),
        "sources are told the quota"
    );
}

#[test]
fn result_quota_is_fair_and_work_conserving() {
    let p = plan("how is the invoice total calculated", &Glossary::default());
    let mut lexical: Vec<Candidate> = (1..=6)
        .map(|rank| hit(Lexical, rank, "big", &format!("src/b{rank}.rs"), (1, 5)))
        .collect();
    lexical.push(hit(Lexical, 7, "small", "src/s7.rs", (1, 5)));
    lexical.push(hit(Lexical, 8, "small", "src/s8.rs", (1, 5)));
    let mut config = no_expansion();
    config.fusion.result_limit = 5;
    config.fusion.result_quota_per_project = Some(2);
    let s = scope(&["big", "small"]);

    let response = run(&p, &s, [vec![], lexical.clone(), vec![]], &config);
    assert_eq!(
        paths(&response),
        [
            "src/b1.rs",
            "src/b2.rs",
            "src/s7.rs",
            "src/s8.rs",
            "src/b3.rs"
        ]
    );
    assert_eq!(response.stats.deferred_by_quota, 4);
    assert_eq!(response.stats.truncated_by_limit, 3);

    config.fusion.result_quota_per_project = None;
    let response = run(&p, &s, [vec![], lexical, vec![]], &config);
    assert_eq!(
        paths(&response),
        [
            "src/b1.rs",
            "src/b2.rs",
            "src/b3.rs",
            "src/b4.rs",
            "src/b5.rs"
        ]
    );
}

#[test]
fn overlapping_hits_on_the_same_content_merge_keeping_best_ranks() {
    let p = plan("how is the invoice total calculated", &Glossary::default());
    let mut exact = hit(Exact, 1, "api", "src/pay.rs", (10, 20));
    exact.symbol = Some("Payment::charge".into());
    let response = run(
        &p,
        &scope(&["api"]),
        [
            vec![exact.clone()],
            vec![hit(Lexical, 1, "api", "src/pay.rs", (1, 40))],
            vec![hit(Semantic, 1, "api", "src/pay.rs", (41, 60))],
        ],
        &no_expansion(),
    );
    assert_eq!(response.results.len(), 2);
    assert_eq!(response.stats.merged_duplicates, 1);
    let merged = response
        .results
        .iter()
        .find(|r| r.location.range.map(|r| r.start()) == Some(1))
        .unwrap();
    assert_eq!(merged.also_at, [exact.location()]);
    let sources: Vec<(SourceKind, u32)> = merged
        .score
        .sources
        .iter()
        .map(|s| (s.source, s.rank))
        .collect();
    assert_eq!(sources, [(Exact, 1), (Lexical, 1)]);
    assert!(approx(merged.score.fused, 0.4 / 61.0 + 0.6 / 61.0));
    assert_eq!(merged.symbol.as_deref(), Some("Payment::charge"));
    assert!(
        merged
            .why
            .iter()
            .any(|r| matches!(r, Reason::ExactMatch { .. }))
    );
    assert!(
        merged
            .why
            .iter()
            .any(|r| matches!(r, Reason::LexicalTerms { .. }))
    );
}

#[test]
fn identical_content_in_another_project_merges_into_the_stronger_hit() {
    let p = plan("how is the invoice total calculated", &Glossary::default());
    let api = hit(Lexical, 1, "api", "src/pay.rs", (1, 40));
    let mut vendored = hit(Semantic, 1, "web", "vendor/pay.rs", (1, 40));
    vendored.content_hash = api.content_hash;
    let response = run(
        &p,
        &scope(&["api", "web"]),
        [vec![], vec![api.clone()], vec![vendored]],
        &no_expansion(),
    );
    assert_eq!(response.results.len(), 1);
    let only = &response.results[0];
    assert_eq!(
        only.location.project,
        name("web"),
        "semantic #1 outweighs lexical #1"
    );
    assert_eq!(only.also_at, [api.location()]);
    assert_eq!(only.score.sources.len(), 2);
}

#[test]
fn different_content_or_file_level_hits_do_not_merge() {
    let p = plan("how is the invoice total calculated", &Glossary::default());
    let lexical = hit(Lexical, 1, "api", "src/pay.rs", (1, 40));
    let mut other_version = hit(Semantic, 1, "api", "src/pay.rs", (1, 40));
    other_version.content_hash = blob("another version");
    let mut file_level = hit(Exact, 1, "api", "src/pay.rs", (1, 1));
    file_level.range = None;
    let response = run(
        &p,
        &scope(&["api"]),
        [vec![file_level], vec![lexical], vec![other_version]],
        &no_expansion(),
    );
    assert_eq!(response.results.len(), 3);
    assert_eq!(response.stats.merged_duplicates, 0);
}

#[test]
fn personal_overlay_shadows_base_view_paths() {
    let p = plan("how is the invoice total calculated", &Glossary::default());
    let mut s = scope(&["api"]);
    let pin = s.manifest.projects.get_mut(&name("api")).unwrap();
    pin.overlay = Some(OverlayPin {
        pin: pinned("wt-feature", 2),
        shadowed_paths: [path("src/deleted.rs")].into(),
    });
    let mut overlay_a = hit(Lexical, 2, "api", "src/a.rs", (1, 12));
    overlay_a.view = view("wt-feature");
    overlay_a.generation = 2;
    overlay_a.content_hash = blob("api/src/a.rs in the worktree");
    let lexical = vec![
        hit(Lexical, 1, "api", "src/a.rs", (1, 10)),
        overlay_a,
        hit(Lexical, 3, "api", "src/b.rs", (1, 10)),
        hit(Lexical, 4, "api", "src/deleted.rs", (1, 10)),
    ];
    let response = run(&p, &s, [vec![], lexical, vec![]], &no_expansion());
    assert_eq!(paths(&response), ["src/a.rs", "src/b.rs"]);
    assert_eq!(response.stats.dropped.shadowed, 2);
    let a = &response.results[0];
    assert_eq!(a.layer, Layer::Overlay);
    assert_eq!(a.location.view, view("wt-feature"));
    assert!(a.why.contains(&Reason::PersonalOverlay {
        view: view("wt-feature")
    }));
    assert_eq!(response.results[1].layer, Layer::Base);
    let layers: Vec<Layer> = response.searched.views.iter().map(|v| v.layer).collect();
    assert_eq!(layers, [Layer::Base, Layer::Overlay]);
}

#[test]
fn candidates_outside_pinned_views_are_dropped_and_counted() {
    let p = plan("how is the invoice total calculated", &Glossary::default());
    let mut wrong_generation = hit(Lexical, 2, "api", "src/b.rs", (1, 5));
    wrong_generation.generation = 2;
    let mut wrong_view = hit(Lexical, 3, "api", "src/c.rs", (1, 5));
    wrong_view.view = ViewId::new("release/2.x").unwrap();
    let lexical = vec![
        hit(Lexical, 1, "api", "src/a.rs", (1, 5)),
        wrong_generation,
        wrong_view,
        hit(Lexical, 4, "ghost", "src/d.rs", (1, 5)),
    ];
    let response = run(
        &p,
        &scope(&["api"]),
        [vec![], lexical, vec![]],
        &no_expansion(),
    );
    assert_eq!(paths(&response), ["src/a.rs"]);
    assert_eq!(response.stats.dropped.view_mismatch, 2);
    assert_eq!(response.stats.dropped.unpinned_project, 1);
    assert_eq!(response.stats.received.lexical, 4);
    assert_eq!(
        response.results[0].commit.as_ref().map(|c| c.as_str()),
        Some(COMMIT)
    );
}

#[test]
fn scope_filters_drop_and_count_by_reason() {
    let p = plan("how is the invoice total calculated", &Glossary::default());
    let mut s = scope(&["api", "web"]);
    s.projects = Some([name("api")].into());
    s.languages = Some([lang("rust")].into());
    s.paths = PathFilter {
        include: vec![PathGlob::new("src/**").unwrap()],
        exclude: vec![PathGlob::new("**/generated/**").unwrap()],
    };
    let mut typescript = hit(Lexical, 2, "api", "src/y.ts", (1, 5));
    typescript.language = Some(lang("typescript"));
    let mut unknown_language = hit(Lexical, 3, "api", "src/z.rs", (1, 5));
    unknown_language.language = None;
    let lexical = vec![
        hit(Lexical, 1, "web", "src/x.rs", (1, 5)),
        typescript,
        unknown_language,
        hit(Lexical, 4, "api", "docs/w.rs", (1, 5)),
        hit(Lexical, 5, "api", "src/generated/g.rs", (1, 5)),
        hit(Lexical, 6, "api", "src/ok.rs", (1, 5)),
    ];
    let response = run(&p, &s, [vec![], lexical, vec![]], &no_expansion());
    assert_eq!(paths(&response), ["src/ok.rs"]);
    let dropped = response.stats.dropped;
    assert_eq!(
        (
            dropped.project_filter,
            dropped.language_filter,
            dropped.path_filter
        ),
        (1, 2, 2)
    );
    let searched: BTreeSet<String> = response
        .searched
        .views
        .iter()
        .map(|v| v.project.to_string())
        .collect();
    assert_eq!(searched, BTreeSet::from(["api".to_owned()]));
}

#[test]
fn malformed_candidates_are_dropped_and_counted() {
    let p = plan("how is the invoice total calculated", &Glossary::default());
    let rank_zero = hit(Lexical, 0, "api", "src/a.rs", (1, 5));
    let mut not_finite = hit(Lexical, 2, "api", "src/b.rs", (1, 5));
    not_finite.raw_score = f64::NAN;
    let wrong_source = hit(Exact, 3, "api", "src/c.rs", (1, 5));
    let mut wrong_detail = hit(Lexical, 4, "api", "src/d.rs", (1, 5));
    wrong_detail.detail = MatchDetail::Semantic {
        profile: "x".into(),
    };
    let lexical = vec![
        rank_zero,
        not_finite,
        wrong_source,
        wrong_detail,
        hit(Lexical, 5, "api", "src/ok.rs", (1, 5)),
    ];
    let response = run(
        &p,
        &scope(&["api"]),
        [vec![], lexical, vec![]],
        &no_expansion(),
    );
    assert_eq!(paths(&response), ["src/ok.rs"]);
    assert_eq!(response.stats.dropped.malformed, 4);
}

#[test]
fn zero_weight_sources_are_not_consulted() {
    let p = plan("how is the invoice total calculated", &Glossary::default());
    let mut config = no_expansion();
    config.fusion.weights.behavior.semantic = 0.0;
    let lexical = FakeSource::answering(vec![hit(Lexical, 1, "api", "src/a.rs", (1, 5))]);
    let semantic = FakeSource::answering(vec![hit(Semantic, 1, "api", "src/b.rs", (1, 5))]);
    let sources = Sources {
        lexical: Some(&lexical),
        semantic: Some(&semantic),
        ..Sources::default()
    };
    let response = search(&p, &scope(&["api"]), &sources, &config).unwrap();
    assert_eq!(semantic.calls.get(), 0);
    assert_eq!(lexical.calls.get(), 1);
    assert_eq!(response.searched.consulted, [Lexical]);
    // Exact is consulted for behaviour queries but not configured: reported.
    let degraded: Vec<String> = response.degraded.iter().map(ToString::to_string).collect();
    assert_eq!(degraded, ["exact: source not configured"]);
}

#[test]
fn glossary_expansions_are_explained() {
    let glossary = Glossary::new(vec![GlossaryEntry::approved(
        "ödeme",
        "payment",
        TermRelation::Translation,
    )])
    .unwrap();
    let p = plan("ödeme nerede iptal ediliyor", &glossary);
    let mut candidate = hit(Lexical, 1, "api", "src/payment.rs", (1, 5));
    candidate.detail = MatchDetail::Lexical {
        terms: vec!["payment".into(), "cancel".into()],
    };
    let response = run(
        &p,
        &scope(&["api"]),
        [vec![], vec![candidate], vec![]],
        &no_expansion(),
    );
    let why = &response.results[0].why;
    assert!(why.iter().any(|r| matches!(
        r,
        Reason::GlossaryExpansion { matched, expansion, .. } if matched == "odeme" && expansion == "payment"
    )));
}
