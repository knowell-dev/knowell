//! Synthetic comparisons of opt-in evidence objectives; these are correctness
//! cases, not evidence of improved agent performance on real repositories.

use std::cell::Cell;

use knowell_query::{
    EdgeKind, EvidenceRole, EvidenceType, ExpandedItem, Glossary, GraphStep, OmitReason, Reason,
    Resolution, SearchResponse, Snippet, SnippetKind, SnippetRequest, SnippetSource, SourceError,
    SourceKind, Sources, TaskPackOptions, TaskSelectionStrategy, Uncertainty, pack_task_with,
    pack_with, plan, search,
};

use crate::common::{FakeSnippets, FakeSource, Words, blob, hit, lang, lines, no_expansion, scope};

/// Two high-ranked alternatives compete with a lower-ranked implementation
/// and its complementary test. Every body costs six tokens including its citation.
fn fixture() -> (SearchResponse, FakeSnippets) {
    let candidates = ["alternative", "similar", "subject"]
        .into_iter()
        .enumerate()
        .map(|(i, name)| {
            let mut candidate = hit(
                SourceKind::Lexical,
                u32::try_from(i + 1).unwrap(),
                "api",
                &format!("src/{name}.rs"),
                (1, 4),
            );
            candidate.symbol = Some(name.into());
            candidate
        })
        .collect();
    let source = FakeSource::answering(candidates);
    let mut response = search(
        &plan("where are payments validated?", &Glossary::default()),
        &scope(&["api"]),
        &Sources {
            lexical: Some(&source),
            ..Sources::default()
        },
        &no_expansion(),
    )
    .unwrap();
    let subject = &response.results[2];
    let test = hit(SourceKind::Lexical, 1, "api", "tests/validation.rs", (1, 4)).location();
    let steps = vec![GraphStep {
        edge: EdgeKind::Test,
        evidence: EvidenceType::SemanticallyResolved,
        resolution: Resolution::Resolved,
        to: test.clone(),
        symbol: Some("checks_validation".into()),
    }];
    response.expanded.push(ExpandedItem {
        location: test,
        commit: subject.commit.clone(),
        layer: subject.layer,
        symbol: Some("checks_validation".into()),
        language: subject.language.clone(),
        seed_rank: subject.rank,
        depth: 1,
        score: subject.score.fused * 0.5,
        path: steps.clone(),
        why: vec![Reason::GraphPath {
            seed: subject.location.clone(),
            steps,
        }],
    });
    let mut snippets = FakeSnippets::default();
    for result in &response.results {
        snippets.set(&result.location, SnippetKind::Body, (1, 4), "source body");
    }
    for item in &response.expanded {
        snippets.set(&item.location, SnippetKind::Body, (1, 4), "test body");
    }
    (response, snippets)
}

fn options(strategy: TaskSelectionStrategy) -> TaskPackOptions {
    TaskPackOptions {
        strategy,
        desired_roles: vec![EvidenceRole::Implementation, EvidenceRole::Test],
        ..TaskPackOptions::default()
    }
}

#[test]
fn rank_comparator_preserves_the_existing_pack_exactly() {
    let (response, snippets) = fixture();
    let mut config = options(TaskSelectionStrategy::Rank);
    config.candidate_limit = 1;
    let selected = pack_task_with(&response, 12, &snippets, &Words, &config).unwrap();
    assert_eq!(selected.pack, pack_with(&response, 12, &snippets, &Words));
    assert_eq!(selected.selection.considered_candidates, 4);
    assert_eq!(selected.selection.omitted_by_candidate_limit, 0);
    assert!(
        selected
            .selection
            .missing_roles
            .contains(&EvidenceRole::Test)
    );
}

#[test]
fn pair_lookahead_can_choose_complementary_bodies_over_two_ranked_alternatives() {
    let (response, snippets) = fixture();
    let greedy = pack_task_with(
        &response,
        12,
        &snippets,
        &Words,
        &options(TaskSelectionStrategy::RoleCoverage),
    )
    .unwrap();
    assert!(greedy.selection.missing_roles.contains(&EvidenceRole::Test));
    let selected = pack_task_with(
        &response,
        12,
        &snippets,
        &Words,
        &options(TaskSelectionStrategy::BoundedBundles),
    )
    .unwrap();
    let paths: Vec<_> = selected
        .pack
        .items
        .iter()
        .map(|item| item.citation.path.as_str())
        .collect();
    assert_eq!(paths, ["src/subject.rs", "tests/validation.rs"]);
    assert_eq!(selected.pack.used_tokens, 12);
    assert_eq!(
        selected.selection.covered_roles,
        [EvidenceRole::Implementation, EvidenceRole::Test]
    );
    let test = selected
        .selection
        .role_evidence
        .iter()
        .find(|e| e.role == EvidenceRole::Test)
        .unwrap();
    assert_eq!(
        test.citations.len(),
        2,
        "both endpoints must be emitted bodies"
    );
    assert!(test.citations.iter().all(|citation| {
        selected
            .pack
            .items
            .iter()
            .any(|item| item.kind == SnippetKind::Body && &item.citation == citation)
    }));
    assert_eq!(
        selected,
        pack_task_with(
            &response,
            12,
            &snippets,
            &Words,
            &options(TaskSelectionStrategy::BoundedBundles)
        )
        .unwrap()
    );
}

#[test]
fn metadata_mmr_can_choose_a_less_redundant_ranked_alternative() {
    let (mut response, snippets) = fixture();
    response.expanded.clear();
    let repeated =
        "payment validation duplicate prevention check account balance client request status";
    response.results[0].symbol = Some(repeated.into());
    response.results[1].symbol = Some(repeated.into());
    response.results[2].symbol = Some("routing dispatch".into());
    let selected = pack_task_with(
        &response,
        12,
        &snippets,
        &Words,
        &options(TaskSelectionStrategy::Mmr),
    )
    .unwrap();
    let paths: Vec<_> = selected
        .pack
        .items
        .iter()
        .map(|item| item.citation.path.as_str())
        .collect();
    assert_eq!(paths, ["src/alternative.rs", "src/subject.rs"]);
}

#[test]
fn unproven_roles_and_weak_paths_stay_missing_even_when_text_is_available() {
    let (mut response, snippets) = fixture();
    let item = &mut response.expanded[0];
    item.path[0].evidence = EvidenceType::HeuristicMatch;
    if let Reason::GraphPath { steps, .. } = &mut item.why[0] {
        steps[0].evidence = EvidenceType::HeuristicMatch;
    }
    let config = TaskPackOptions {
        desired_roles: vec![
            EvidenceRole::Test,
            EvidenceRole::Entry,
            EvidenceRole::Config,
        ],
        ..options(TaskSelectionStrategy::BoundedBundles)
    };
    let selected = pack_task_with(&response, 100, &snippets, &Words, &config).unwrap();
    assert!(selected.selection.covered_roles.is_empty());
    assert_eq!(
        selected.selection.missing_roles,
        [
            EvidenceRole::Entry,
            EvidenceRole::Test,
            EvidenceRole::Config
        ]
    );
    assert!(
        selected
            .pack
            .uncertainties
            .iter()
            .any(|u| matches!(u, Uncertainty::WeakGraphEvidence { .. }))
    );
}

#[test]
fn a_skeleton_never_certifies_an_implementation_body() {
    let (mut response, mut snippets) = fixture();
    response.results.truncate(1);
    response.expanded.clear();
    snippets.set(
        &response.results[0].location,
        SnippetKind::Skeleton,
        (1, 1),
        "signature",
    );
    let selected = pack_task_with(
        &response,
        5,
        &snippets,
        &Words,
        &options(TaskSelectionStrategy::BoundedBundles),
    )
    .unwrap();
    assert_eq!(selected.pack.items.len(), 1);
    assert_eq!(selected.pack.items[0].kind, SnippetKind::Skeleton);
    assert!(selected.selection.covered_roles.is_empty());
    assert!(selected.pack.used_tokens <= 5);
}

#[test]
fn a_truncated_body_does_not_certify_the_requested_symbol_range() {
    let (mut response, mut snippets) = fixture();
    response.results.truncate(1);
    response.expanded.clear();
    snippets.set(
        &response.results[0].location,
        SnippetKind::Body,
        (1, 2),
        "partial body",
    );
    let selected = pack_task_with(
        &response,
        100,
        &snippets,
        &Words,
        &options(TaskSelectionStrategy::BoundedBundles),
    )
    .unwrap();
    assert_eq!(selected.pack.items.len(), 1);
    assert!(selected.selection.covered_roles.is_empty());
    assert!(
        selected
            .selection
            .missing_roles
            .contains(&EvidenceRole::Implementation)
    );
}

#[test]
fn headings_schema_keys_and_unknown_languages_do_not_count_as_implementation() {
    for language in [
        "markdown", "json", "yaml", "toml", "protobuf", "text", "unknown",
    ] {
        let (mut response, snippets) = fixture();
        response.results.truncate(1);
        response.expanded.clear();
        response.results[0].language = Some(lang(language));
        response.results[0].symbol = Some("Overview".into());
        let selected = pack_task_with(
            &response,
            100,
            &snippets,
            &Words,
            &options(TaskSelectionStrategy::BoundedBundles),
        )
        .unwrap();
        assert_eq!(selected.pack.items.len(), 1, "text remains usable evidence");
        assert!(
            selected
                .selection
                .missing_roles
                .contains(&EvidenceRole::Implementation),
            "{language}"
        );
    }
}

#[test]
fn resolved_syntax_or_contract_edges_are_surroundings_not_call_or_test_proof() {
    for edge in [EdgeKind::Caller, EdgeKind::Callee, EdgeKind::Test] {
        for evidence_type in [
            EvidenceType::SyntacticObservation,
            EvidenceType::ContractDerived,
        ] {
            let (mut response, snippets) = fixture();
            let item = &mut response.expanded[0];
            item.path[0].edge = edge;
            item.path[0].evidence = evidence_type;
            if let Reason::GraphPath { steps, .. } = &mut item.why[0] {
                steps[0].edge = edge;
                steps[0].evidence = evidence_type;
            }
            let config = TaskPackOptions {
                desired_roles: vec![
                    EvidenceRole::Caller,
                    EvidenceRole::Callee,
                    EvidenceRole::Test,
                    EvidenceRole::Surroundings,
                ],
                ..options(TaskSelectionStrategy::BoundedBundles)
            };
            let selected = pack_task_with(&response, 100, &snippets, &Words, &config).unwrap();
            assert_eq!(
                selected.selection.covered_roles,
                [EvidenceRole::Surroundings]
            );
            assert_eq!(
                selected.selection.missing_roles,
                [
                    EvidenceRole::Caller,
                    EvidenceRole::Callee,
                    EvidenceRole::Test
                ]
            );
            assert_eq!(selected.selection.role_evidence[0].citations.len(), 2);
        }
    }
}

#[test]
fn even_a_resolved_import_is_not_a_call_or_test_when_target_is_a_file() {
    let (mut response, mut snippets) = fixture();
    let item = &mut response.expanded[0];
    item.location.range = None;
    item.symbol = None;
    item.path[0].to = item.location.clone();
    item.path[0].symbol = None;
    if let Reason::GraphPath { steps, .. } = &mut item.why[0] {
        steps[0].to = item.location.clone();
        steps[0].symbol = None;
    }
    snippets.set(&item.location, SnippetKind::Body, (1, 4), "import only");
    let config = TaskPackOptions {
        desired_roles: vec![EvidenceRole::Test, EvidenceRole::Surroundings],
        ..options(TaskSelectionStrategy::BoundedBundles)
    };
    let selected = pack_task_with(&response, 100, &snippets, &Words, &config).unwrap();
    assert_eq!(
        selected.selection.covered_roles,
        [EvidenceRole::Surroundings]
    );
    assert_eq!(selected.selection.missing_roles, [EvidenceRole::Test]);
}

#[test]
fn stale_candidates_and_no_budget_are_explicit() {
    let (mut response, mut snippets) = fixture();
    response.results.truncate(1);
    response.expanded.clear();
    snippets
        .bodies
        .get_mut(&response.results[0].location.label())
        .unwrap()
        .content_hash = blob("other version");
    let selected = pack_task_with(
        &response,
        100,
        &snippets,
        &Words,
        &options(TaskSelectionStrategy::BoundedBundles),
    )
    .unwrap();
    assert!(selected.pack.items.is_empty());
    assert_eq!(
        selected.selection.unselected,
        [response.results[0].location.clone()]
    );
    assert!(
        selected
            .selection
            .candidate_issues
            .iter()
            .any(|issue| matches!(issue.reason, OmitReason::StaleContent { .. }))
    );

    let (response, snippets) = fixture();
    let selected = pack_task_with(
        &response,
        0,
        &snippets,
        &Words,
        &options(TaskSelectionStrategy::BoundedBundles),
    )
    .unwrap();
    assert_eq!(selected.pack.used_tokens, 0);
    assert!(selected.pack.items.is_empty());
    assert!(
        selected
            .selection
            .candidate_issues
            .iter()
            .any(|issue| matches!(issue.reason, OmitReason::OverBudget { .. }))
    );
}

#[test]
fn bounds_are_accounted_for_and_invalid_bounds_are_rejected() {
    let (response, snippets) = fixture();
    let mut config = options(TaskSelectionStrategy::BoundedBundles);
    config.evaluation_budget = 1;
    let selected = pack_task_with(&response, 100, &snippets, &Words, &config).unwrap();
    assert_eq!(selected.selection.evaluations, 1);
    assert!(selected.selection.evaluation_budget_exhausted);
    config.candidate_limit = 1;
    let selected = pack_task_with(&response, 100, &snippets, &Words, &config).unwrap();
    assert_eq!(selected.selection.considered_candidates, 1);
    assert_eq!(selected.selection.omitted_by_candidate_limit, 3);
    config.candidate_limit = 0;
    assert!(pack_task_with(&response, 100, &snippets, &Words, &config).is_err());
    config.candidate_limit = 1;
    config.evaluation_budget = 4097;
    assert!(pack_task_with(&response, 100, &snippets, &Words, &config).is_err());
}

struct CountedSnippets {
    inner: FakeSnippets,
    requests: Cell<usize>,
}
impl SnippetSource for CountedSnippets {
    fn snippet(&self, request: &SnippetRequest<'_>) -> Result<Option<Snippet>, SourceError> {
        self.requests.set(self.requests.get() + 1);
        self.inner.snippet(request)
    }
}

#[test]
fn repeated_trials_fetch_each_exact_snippet_request_only_once() {
    let (response, snippets) = fixture();
    let counted = CountedSnippets {
        inner: snippets,
        requests: Cell::new(0),
    };
    let selected = pack_task_with(
        &response,
        12,
        &counted,
        &Words,
        &options(TaskSelectionStrategy::BoundedBundles),
    )
    .unwrap();
    assert!(selected.selection.evaluations > 4);
    assert!(
        counted.requests.get() <= 8,
        "at most a skeleton and body per candidate"
    );
}

#[test]
fn contained_candidate_bodies_are_sent_once_and_charged_once() {
    let (mut response, mut snippets) = fixture();
    response.results.truncate(2);
    response.expanded.clear();
    response.results[0].location.range = Some(lines(1, 10));
    response.results[1].location = response.results[0].location.clone();
    response.results[1].location.range = Some(lines(3, 4));
    snippets.set(
        &response.results[0].location,
        SnippetKind::Body,
        (1, 10),
        "whole body",
    );
    snippets.set(
        &response.results[1].location,
        SnippetKind::Body,
        (3, 4),
        "body",
    );
    let selected = pack_task_with(
        &response,
        12,
        &snippets,
        &Words,
        &options(TaskSelectionStrategy::BoundedBundles),
    )
    .unwrap();
    assert_eq!(selected.pack.items.len(), 1);
    assert_eq!(selected.pack.used_tokens, 6);
    assert_eq!(selected.selection.selected_candidates, 2);
    assert_eq!(selected.selection.role_evidence[0].citations.len(), 1);
}

#[test]
fn an_omitted_intermediate_body_does_not_certify_a_multi_hop_role() {
    let (mut response, snippets) = fixture();
    let middle = hit(SourceKind::Lexical, 1, "api", "src/middle.rs", (1, 4)).location();
    let step = GraphStep {
        edge: EdgeKind::Callee,
        evidence: EvidenceType::SemanticallyResolved,
        resolution: Resolution::Resolved,
        to: middle,
        symbol: Some("middle".into()),
    };
    response.expanded[0].path.insert(0, step.clone());
    response.expanded[0].depth = 2;
    if let Reason::GraphPath { steps, .. } = &mut response.expanded[0].why[0] {
        steps.insert(0, step);
    }
    let selected = pack_task_with(
        &response,
        100,
        &snippets,
        &Words,
        &options(TaskSelectionStrategy::BoundedBundles),
    )
    .unwrap();
    assert!(
        selected
            .selection
            .missing_roles
            .contains(&EvidenceRole::Test)
    );
}
