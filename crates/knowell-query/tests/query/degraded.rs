//! Degraded mode (missing / failing sources), empty-result explanations and
//! the optional reranker.

use std::cell::Cell;

use knowell_query::{
    ABSENCE_NOTE, Candidate, CoverageGap, EdgeKind, EmptyReason, Glossary, Intent,
    IntentClassifier, OverlayPin, PlanOptions, ProjectCoverage, QueryError, QueryPlan, QueryScope,
    SearchConfig, SearchResponse, SourceError, SourceKind, SourceLists, SourceStatus, Sources,
    plan, plan_with, search, search_with_candidates,
};

use crate::common::{FakeReranker, FakeSource, hit, lang, name, no_expansion, path, pinned, scope};

use SourceKind::{Exact, Lexical, Semantic};

fn behaviour() -> QueryPlan {
    plan("where do we prevent double payments?", &Glossary::default())
}

fn reasons(response: &SearchResponse) -> &[EmptyReason] {
    &response.empty.as_ref().unwrap().reasons
}

#[test]
fn missing_semantic_provider_is_reported_not_hidden() {
    let lexical = FakeSource::answering(vec![hit(Lexical, 1, "api", "src/pay.rs", (1, 9))]);
    let exact = FakeSource::answering(vec![]);
    let sources = Sources {
        exact: Some(&exact),
        lexical: Some(&lexical),
        ..Sources::default()
    };
    let response = search(&behaviour(), &scope(&["api"]), &sources, &no_expansion()).unwrap();
    assert_eq!(response.results.len(), 1);
    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(
        json["degraded"],
        serde_json::json!(["semantic: provider not configured"])
    );
    assert_eq!(response.searched.consulted, [Exact, Lexical]);
    assert!(response.empty.is_none());
}

#[test]
fn failing_source_is_reported_and_others_still_answer() {
    let lexical = FakeSource::failing(SourceError::Failed("index offline".into()));
    let semantic = FakeSource::answering(vec![hit(Semantic, 1, "api", "src/pay.rs", (1, 9))]);
    let exact = FakeSource::failing(SourceError::Unavailable(
        "symbol table for generation 1 is still building".into(),
    ));
    let sources = Sources {
        exact: Some(&exact),
        lexical: Some(&lexical),
        semantic: Some(&semantic),
        ..Sources::default()
    };
    let response = search(&behaviour(), &scope(&["api"]), &sources, &no_expansion()).unwrap();
    let degraded: Vec<String> = response.degraded.iter().map(ToString::to_string).collect();
    assert_eq!(
        degraded,
        [
            "exact: symbol table for generation 1 is still building",
            "lexical: failed: index offline"
        ]
    );
    assert_eq!(response.searched.consulted, [Exact, Lexical, Semantic]);
    assert_eq!(response.searched.answered, [Semantic]);
    assert_eq!(response.results.len(), 1);
}

#[test]
fn no_sources_at_all_explains_itself() {
    let response = search(
        &behaviour(),
        &scope(&["api"]),
        &Sources::default(),
        &SearchConfig::default(),
    )
    .unwrap();
    assert!(response.results.is_empty());
    let empty = response.empty.as_ref().unwrap();
    assert_eq!(empty.note, ABSENCE_NOTE);
    let [EmptyReason::SourcesUnavailable { sources }] = empty.reasons.as_slice() else {
        panic!("{:?}", empty.reasons);
    };
    assert_eq!(sources.len(), 3);
    // Expansion is not attempted without results, so no graph degradation.
    assert_eq!(response.degraded.len(), 3);
}

#[test]
fn empty_query_consults_nothing() {
    let lexical = FakeSource::answering(vec![hit(Lexical, 1, "api", "src/a.rs", (1, 2))]);
    let sources = Sources {
        lexical: Some(&lexical),
        ..Sources::default()
    };
    let p = plan("   ", &Glossary::default());
    let response = search(&p, &scope(&["api"]), &sources, &SearchConfig::default()).unwrap();
    assert_eq!(lexical.calls.get(), 0);
    assert_eq!(reasons(&response), [EmptyReason::EmptyQuery]);
}

#[test]
fn unknown_and_unindexed_projects_are_named() {
    let mut s = scope(&["api"]);
    s.manifest.not_indexed.insert(name("billing"));
    s.projects = Some([name("billing"), name("nope")].into());
    let lexical = FakeSource::answering(vec![]);
    let sources = Sources {
        lexical: Some(&lexical),
        ..Sources::default()
    };
    let response = search(&behaviour(), &s, &sources, &no_expansion()).unwrap();
    assert_eq!(lexical.calls.get(), 0, "nothing to search");
    let r = reasons(&response);
    assert_eq!(
        r[..3],
        [
            EmptyReason::ProjectNotIndexed {
                project: name("billing")
            },
            EmptyReason::UnknownProject {
                project: name("nope")
            },
            EmptyReason::NoProjectsInScope,
        ]
    );
}

#[test]
fn no_candidates_in_the_selected_ref() {
    let exact = FakeSource::answering(vec![]);
    let lexical = FakeSource::answering(vec![]);
    let semantic = FakeSource::answering(vec![]);
    let sources = Sources {
        exact: Some(&exact),
        lexical: Some(&lexical),
        semantic: Some(&semantic),
        ..Sources::default()
    };
    let response = search(&behaviour(), &scope(&["api"]), &sources, &no_expansion()).unwrap();
    let [EmptyReason::NoCandidatesInSelectedRef { views }] = reasons(&response) else {
        panic!("{:?}", reasons(&response));
    };
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].view.as_str(), "main");
    assert_eq!(views[0].generation, 1);
}

fn only_lexical(candidates: Vec<Candidate>, s: &QueryScope) -> SearchResponse {
    let lists = SourceLists {
        exact: SourceStatus::NotConsulted,
        lexical: SourceStatus::Answered(candidates),
        semantic: SourceStatus::NotConsulted,
    };
    search_with_candidates(&behaviour(), s, lists, None, None, &no_expansion()).unwrap()
}

#[test]
fn every_candidate_filtered_by_scope() {
    let mut s = scope(&["api"]);
    s.paths.include = vec![knowell_query::PathGlob::new("web/**").unwrap()];
    let response = only_lexical(vec![hit(Lexical, 1, "api", "src/a.rs", (1, 2))], &s);
    assert_eq!(
        reasons(&response),
        [EmptyReason::AllFilteredByScope {
            project_filter: 0,
            language_filter: 0,
            path_filter: 1
        }]
    );
}

#[test]
fn every_candidate_outside_pinned_views() {
    let mut newer = hit(Lexical, 1, "api", "src/a.rs", (1, 2));
    newer.generation = 2;
    let response = only_lexical(vec![newer], &scope(&["api"]));
    assert_eq!(
        reasons(&response),
        [EmptyReason::AllOutsidePinnedViews {
            unpinned_project: 0,
            view_mismatch: 1
        }]
    );
}

#[test]
fn every_candidate_shadowed_by_the_overlay() {
    let mut s = scope(&["api"]);
    s.manifest.projects.get_mut(&name("api")).unwrap().overlay = Some(OverlayPin {
        pin: pinned("wt", 3),
        shadowed_paths: [path("src/a.rs")].into(),
    });
    let response = only_lexical(vec![hit(Lexical, 1, "api", "src/a.rs", (1, 2))], &s);
    assert_eq!(
        reasons(&response),
        [EmptyReason::AllShadowedByOverlay { shadowed: 1 }]
    );
}

#[test]
fn impact_without_reference_resolution_says_so() {
    let mut s = scope(&["api"]);
    s.manifest.projects.get_mut(&name("api")).unwrap().coverage = ProjectCoverage {
        languages: [lang("dart"), lang("rust")].into(),
        reference_resolution: [lang("rust")].into(),
    };
    let p = plan("who calls submitOrder", &Glossary::default());
    assert_eq!(p.intent, Intent::Impact);
    let lists = SourceLists {
        exact: SourceStatus::Answered(vec![]),
        lexical: SourceStatus::Answered(vec![]),
        semantic: SourceStatus::Answered(vec![]),
    };
    let response = search_with_candidates(&p, &s, lists, None, None, &no_expansion()).unwrap();
    let gap = CoverageGap::NoReferenceResolution {
        project: name("api"),
        language: lang("dart"),
    };
    assert_eq!(response.coverage_gaps, [gap]);
    assert!(
        reasons(&response).contains(&EmptyReason::NoReferenceResolutionForLanguage {
            project: name("api"),
            language: lang("dart"),
        })
    );
}

#[test]
fn rust_test_only_expansion_reports_missing_relation_analysis() {
    let mut query_scope = scope(&["api"]);
    query_scope
        .manifest
        .projects
        .get_mut(&name("api"))
        .unwrap()
        .coverage = ProjectCoverage {
        languages: [lang("rust"), lang("markdown")].into(),
        reference_resolution: Default::default(),
    };
    let lexical = FakeSource::answering(vec![hit(Lexical, 1, "api", "src/probe.rs", (1, 3))]);
    let sources = Sources {
        lexical: Some(&lexical),
        ..Sources::default()
    };
    let path_plan = plan("src/probe.rs", &Glossary::default());
    assert_eq!(path_plan.intent, Intent::PathOrFile);
    let mut config = SearchConfig::default();
    config.expansion.edges.path_or_file = vec![EdgeKind::Test];
    let response = search(&path_plan, &query_scope, &sources, &config).unwrap();
    assert_eq!(response.results.len(), 1);
    assert!(response.expanded.is_empty());
    assert_eq!(
        response.coverage_gaps,
        [CoverageGap::NoReferenceResolution {
            project: name("api"),
            language: lang("rust"),
        }]
    );
    config.expansion.enabled = false;
    assert!(
        search(&path_plan, &query_scope, &sources, &config)
            .unwrap()
            .coverage_gaps
            .is_empty()
    );
}

#[test]
fn document_only_test_expansion_does_not_report_missing_code_analysis() {
    let mut query_scope = scope(&["api"]);
    query_scope
        .manifest
        .projects
        .get_mut(&name("api"))
        .unwrap()
        .coverage = ProjectCoverage {
        languages: [lang("markdown"), lang("toml")].into(),
        reference_resolution: Default::default(),
    };
    let mut document = hit(Lexical, 1, "api", "docs/guide.md", (1, 3));
    document.language = Some(lang("markdown"));
    let lexical = FakeSource::answering(vec![document]);
    let sources = Sources {
        lexical: Some(&lexical),
        ..Sources::default()
    };
    let mut config = SearchConfig::default();
    config.expansion.edges.path_or_file = vec![EdgeKind::Test];
    let response = search(
        &plan("docs/guide.md", &Glossary::default()),
        &query_scope,
        &sources,
        &config,
    )
    .unwrap();
    assert_eq!(response.results.len(), 1);
    assert!(response.coverage_gaps.is_empty());
}

#[test]
fn mixed_document_and_code_languages_emit_only_relevant_reference_gaps() {
    let mut query_scope = scope(&["api"]);
    query_scope
        .manifest
        .projects
        .get_mut(&name("api"))
        .unwrap()
        .coverage = ProjectCoverage {
        languages: [
            "typescript",
            "rust",
            "markdown",
            "toml",
            "yaml",
            "json",
            "css",
            "html",
            "text",
            "protobuf",
            "graphql",
            "xml",
            "hcl",
            "ini",
            "dockerfile",
            "makefile",
            "cmake",
        ]
        .map(lang)
        .into(),
        reference_resolution: [lang("rust")].into(),
    };
    let lexical = FakeSource::answering(vec![hit(Lexical, 1, "api", "src/pay.ts", (1, 9))]);
    let sources = Sources {
        lexical: Some(&lexical),
        ..Sources::default()
    };
    let expected = CoverageGap::NoReferenceResolution {
        project: name("api"),
        language: lang("typescript"),
    };
    let relation_search = search(
        &behaviour(),
        &query_scope,
        &sources,
        &SearchConfig::default(),
    )
    .unwrap();
    assert_eq!(
        relation_search.coverage_gaps.as_slice(),
        std::slice::from_ref(&expected)
    );
    let content_search = search(&behaviour(), &query_scope, &sources, &no_expansion()).unwrap();
    assert!(content_search.coverage_gaps.is_empty());
    // Impact questions still require reference analysis even with graph
    // expansion disabled: suppressing document noise must not hide this gap.
    let impact = plan("who calls submitOrder", &Glossary::default());
    assert_eq!(impact.intent, Intent::Impact);
    let intrinsic = search(&impact, &query_scope, &sources, &no_expansion()).unwrap();
    assert_eq!(intrinsic.coverage_gaps, [expected]);
    query_scope.languages = Some([lang("markdown"), lang("yaml")].into());
    let documents = search(&impact, &query_scope, &sources, &no_expansion()).unwrap();
    assert!(documents.coverage_gaps.is_empty());
}

#[test]
fn callable_language_names_and_aliases_preserve_missing_analysis_warnings() {
    let impact = plan("who calls submitOrder", &Glossary::default());
    for language in [
        "rust",
        "typescript",
        "tsx",
        "javascript",
        "jsx",
        "python",
        "go",
        "java",
        "kotlin",
        "csharp",
        "c#",
        "dart",
        "swift",
        "php",
        "ruby",
        "c",
        "cpp",
        "c++",
        "scala",
        "bash",
        "lua",
        "perl",
        "r",
        "elixir",
        "erlang",
        "haskell",
        "ocaml",
        "clojure",
        "zig",
        "objc",
        "powershell",
        "batch",
        "groovy",
        "vue",
        "svelte",
    ] {
        let mut query_scope = scope(&["api"]);
        query_scope
            .manifest
            .projects
            .get_mut(&name("api"))
            .unwrap()
            .coverage = ProjectCoverage {
            languages: [lang(language)].into(),
            reference_resolution: Default::default(),
        };
        let response = search_with_candidates(
            &impact,
            &query_scope,
            SourceLists {
                exact: SourceStatus::Answered(Vec::new()),
                lexical: SourceStatus::Answered(Vec::new()),
                semantic: SourceStatus::Answered(Vec::new()),
            },
            None,
            None,
            &no_expansion(),
        )
        .unwrap();
        assert_eq!(
            response.coverage_gaps,
            [CoverageGap::NoReferenceResolution {
                project: name("api"),
                language: lang(language),
            }],
            "{language}"
        );
        assert!(
            reasons(&response).contains(&EmptyReason::NoReferenceResolutionForLanguage {
                project: name("api"),
                language: lang(language),
            })
        );
    }
}

#[test]
fn rerank_is_off_by_default_and_reports_when_missing() {
    let lexical = FakeSource::answering(vec![
        hit(Lexical, 1, "api", "src/a.rs", (1, 2)),
        hit(Lexical, 2, "api", "src/b.rs", (1, 2)),
    ]);
    let reranker = FakeReranker {
        scores: Ok(vec![0.1, 0.9]),
        calls: Cell::new(0),
    };
    let sources = Sources {
        lexical: Some(&lexical),
        reranker: Some(&reranker),
        ..Sources::default()
    };
    let response = search(&behaviour(), &scope(&["api"]), &sources, &no_expansion()).unwrap();
    assert_eq!(reranker.calls.get(), 0, "off by default");
    assert!(response.results[0].score.rerank.is_none());

    let mut config = no_expansion();
    config.rerank.enabled = true;
    let without = Sources {
        lexical: Some(&lexical),
        ..Sources::default()
    };
    let response = search(&behaviour(), &scope(&["api"]), &without, &config).unwrap();
    assert!(
        response
            .degraded
            .iter()
            .any(|d| d.to_string() == "rerank: reranker not configured")
    );
}

#[test]
fn rerank_reorders_only_the_short_list() {
    let lexical = FakeSource::answering(vec![
        hit(Lexical, 1, "api", "src/a.rs", (1, 2)),
        hit(Lexical, 2, "api", "src/b.rs", (1, 2)),
        hit(Lexical, 3, "api", "src/c.rs", (1, 2)),
    ]);
    let reranker = FakeReranker {
        scores: Ok(vec![0.1, 0.9]),
        calls: Cell::new(0),
    };
    let sources = Sources {
        lexical: Some(&lexical),
        reranker: Some(&reranker),
        ..Sources::default()
    };
    let mut config = no_expansion();
    config.rerank.enabled = true;
    config.rerank.top_n = 2;
    let response = search(&behaviour(), &scope(&["api"]), &sources, &config).unwrap();
    let order: Vec<(&str, u32)> = response
        .results
        .iter()
        .map(|r| (r.location.path.as_str(), r.rank))
        .collect();
    assert_eq!(order, [("src/b.rs", 1), ("src/a.rs", 2), ("src/c.rs", 3)]);
    let rerank = response.results[0].score.rerank.as_ref().unwrap();
    assert_eq!(rerank.reranker, "fake-reranker@1");
    assert!(response.results[2].score.rerank.is_none());
}

#[test]
fn bad_reranker_output_keeps_fused_order() {
    let lexical = FakeSource::answering(vec![
        hit(Lexical, 1, "api", "src/a.rs", (1, 2)),
        hit(Lexical, 2, "api", "src/b.rs", (1, 2)),
    ]);
    let mut config = no_expansion();
    config.rerank.enabled = true;
    for scores in [
        Ok(vec![0.5]),
        Ok(vec![f64::NAN, 1.0]),
        Err(SourceError::Unavailable(
            "data policy forbids cloud rerank".into(),
        )),
    ] {
        let reranker = FakeReranker {
            scores,
            calls: Cell::new(0),
        };
        let sources = Sources {
            lexical: Some(&lexical),
            reranker: Some(&reranker),
            ..Sources::default()
        };
        let response = search(&behaviour(), &scope(&["api"]), &sources, &config).unwrap();
        assert_eq!(response.results[0].location.path.as_str(), "src/a.rs");
        assert!(response.results.iter().all(|r| r.score.rerank.is_none()));
        assert!(
            response
                .degraded
                .iter()
                .any(|d| d.component == knowell_query::Component::Rerank)
        );
    }
}

#[test]
fn reranker_can_promote_a_candidate_beyond_the_visible_limit() {
    let lexical = FakeSource::answering(vec![
        hit(Lexical, 1, "api", "src/a.rs", (1, 2)),
        hit(Lexical, 2, "api", "src/b.rs", (1, 2)),
        hit(Lexical, 3, "api", "src/c.rs", (1, 2)),
    ]);
    let reranker = FakeReranker {
        scores: Ok(vec![0.1, 0.2, 0.9]),
        calls: Cell::new(0),
    };
    let sources = Sources {
        lexical: Some(&lexical),
        reranker: Some(&reranker),
        ..Sources::default()
    };
    let mut config = no_expansion();
    config.rerank.enabled = true;
    config.rerank.top_n = 3;
    config.fusion.result_limit = 1;
    let response = search(&behaviour(), &scope(&["api"]), &sources, &config).unwrap();
    assert_eq!(response.results[0].location.path.as_str(), "src/c.rs");
    assert_eq!(response.results[0].rank, 1);
    assert_eq!(response.stats.fused, 3);
    assert_eq!(response.stats.truncated_by_limit, 2);
    assert!(
        response
            .degraded
            .iter()
            .all(|d| d.component != knowell_query::Component::Rerank)
    );
}

#[test]
fn final_project_quota_remains_work_conserving_after_rerank() {
    let lexical = FakeSource::answering(vec![
        hit(Lexical, 1, "api", "src/a.rs", (1, 2)),
        hit(Lexical, 2, "api", "src/b.rs", (1, 2)),
        hit(Lexical, 3, "ui", "src/c.rs", (1, 2)),
    ]);
    let reranker = FakeReranker {
        scores: Ok(vec![0.8, 0.9, 0.1]),
        calls: Cell::new(0),
    };
    let sources = Sources {
        lexical: Some(&lexical),
        reranker: Some(&reranker),
        ..Sources::default()
    };
    let mut config = no_expansion();
    config.rerank.enabled = true;
    config.rerank.top_n = 3;
    config.fusion.result_limit = 2;
    config.fusion.result_quota_per_project = Some(1);
    let response = search(&behaviour(), &scope(&["api", "ui"]), &sources, &config).unwrap();
    let order: Vec<_> = response
        .results
        .iter()
        .map(|r| (r.location.path.as_str(), r.rank))
        .collect();
    assert_eq!(order, [("src/b.rs", 1), ("src/c.rs", 2)]);
    assert_eq!(response.stats.deferred_by_quota, 1);
    assert_eq!(response.stats.truncated_by_limit, 1);
    config.fusion.result_limit = 3;
    let response = search(&behaviour(), &scope(&["api", "ui"]), &sources, &config).unwrap();
    assert_eq!(response.results[2].location.path.as_str(), "src/a.rs");
    assert_eq!(response.results[2].rank, 3);
    assert_eq!(response.stats.truncated_by_limit, 0);
}

#[test]
fn disabled_or_empty_expansion_does_not_emit_unrelated_reference_gaps() {
    let mut query_scope = scope(&["api"]);
    query_scope
        .manifest
        .projects
        .get_mut(&name("api"))
        .unwrap()
        .coverage
        .reference_resolution
        .clear();
    let lexical = FakeSource::answering(vec![hit(Lexical, 1, "api", "src/a.rs", (1, 2))]);
    let sources = Sources {
        lexical: Some(&lexical),
        ..Sources::default()
    };
    for mode in 0..3 {
        let mut config = SearchConfig::default();
        match mode {
            0 => config.expansion.enabled = false,
            1 => config.expansion.seeds = 0,
            _ => config.expansion.node_budget = 0,
        }
        let response = search(&behaviour(), &query_scope, &sources, &config).unwrap();
        assert!(response.coverage_gaps.is_empty());
    }
    let empty = FakeSource::answering(vec![]);
    let response = search(
        &behaviour(),
        &query_scope,
        &Sources {
            lexical: Some(&empty),
            ..Sources::default()
        },
        &SearchConfig::default(),
    )
    .unwrap();
    assert!(response.coverage_gaps.is_empty());
    let response = search(
        &behaviour(),
        &query_scope,
        &sources,
        &SearchConfig::default(),
    )
    .unwrap();
    assert_eq!(
        response.coverage_gaps.len(),
        1,
        "requested call expansion still reports absent analysis"
    );
}

#[test]
fn missing_graph_expander_is_reported_when_expansion_runs() {
    let lexical = FakeSource::answering(vec![hit(Lexical, 1, "api", "src/a.rs", (1, 2))]);
    let sources = Sources {
        lexical: Some(&lexical),
        ..Sources::default()
    };
    let response = search(
        &behaviour(),
        &scope(&["api"]),
        &sources,
        &SearchConfig::default(),
    )
    .unwrap();
    assert!(
        response
            .degraded
            .iter()
            .any(|d| d.to_string() == "graph: expander not configured")
    );
}

struct Broken;

impl IntentClassifier for Broken {
    fn id(&self) -> String {
        "broken@0".into()
    }

    fn classify(&self, _q: &str, _p: &QueryPlan) -> Result<Option<Intent>, SourceError> {
        Err(SourceError::Failed("http 503".into()))
    }
}

#[test]
fn classifier_failure_reaches_the_response() {
    let p = plan_with(
        "checkout",
        &Glossary::default(),
        &PlanOptions::default(),
        Some(&Broken),
    );
    let lexical = FakeSource::answering(vec![]);
    let sources = Sources {
        lexical: Some(&lexical),
        ..Sources::default()
    };
    let response = search(&p, &scope(&["api"]), &sources, &no_expansion()).unwrap();
    assert_eq!(
        response.degraded[0].to_string(),
        "classifier: failed: http 503"
    );
}

#[test]
fn invalid_scope_or_config_is_an_error() {
    let mut s = scope(&["api"]);
    s.workspace = name("other");
    let err = search(
        &behaviour(),
        &s,
        &Sources::default(),
        &SearchConfig::default(),
    )
    .unwrap_err();
    assert!(matches!(err, QueryError::WorkspaceMismatch { .. }));

    let mut config = SearchConfig::default();
    config.fusion.result_limit = 0;
    let err = search(&behaviour(), &scope(&["api"]), &Sources::default(), &config).unwrap_err();
    assert!(matches!(
        err,
        QueryError::InvalidConfig {
            field: "fusion.result_limit",
            ..
        }
    ));

    let mut s = scope(&["api"]);
    s.manifest.projects.get_mut(&name("api")).unwrap().overlay = Some(OverlayPin {
        pin: pinned("main", 2),
        shadowed_paths: Default::default(),
    });
    let err = search(
        &behaviour(),
        &s,
        &Sources::default(),
        &SearchConfig::default(),
    )
    .unwrap_err();
    assert!(matches!(err, QueryError::InvalidManifest { .. }));

    let options = PlanOptions {
        include_suggested: false,
        domain: Some(name("billing")),
    };
    let p = plan_with("refund", &Glossary::default(), &options, None);
    let mut s = scope(&["api"]);
    s.domain = Some(name("crm"));
    let err = search(&p, &s, &Sources::default(), &SearchConfig::default()).unwrap_err();
    assert!(matches!(err, QueryError::DomainMismatch { .. }));
    s.domain = Some(name("billing"));
    assert!(search(&p, &s, &Sources::default(), &SearchConfig::default()).is_ok());
}
