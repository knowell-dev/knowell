//! Source-body selection tests. These fixtures establish behavior, not agent
//! performance or complete evidence recall for natural-language questions.

use std::cell::{Cell, RefCell};

use knowell_query::{
    CommitId, EvidenceRole, ExpandedItem, Glossary, Location, OmitReason, Reason, SearchResponse,
    Snippet, SnippetKind, SnippetRequest, SnippetSource, SourceError, SourceKind, Sources,
    TaskPackOptions, TaskSelectionStrategy, pack_task_with, plan, search, task_source_locations,
};

use crate::common::{FakeSnippets, FakeSource, Words, blob, hit, lines, no_expansion, scope};

fn fixture(query: &str, bodies: &[(&str, &str)]) -> (SearchResponse, FakeSnippets) {
    let candidates = bodies
        .iter()
        .enumerate()
        .map(|(index, (path, text))| {
            let end = u32::try_from(text.split_inclusive('\n').count()).unwrap();
            let mut candidate = hit(
                SourceKind::Lexical,
                u32::try_from(index + 1).unwrap(),
                "api",
                path,
                (1, end),
            );
            candidate.symbol = Some(format!("source_{index}"));
            candidate
        })
        .collect();
    let source = FakeSource::answering(candidates);
    let response = search(
        &plan(query, &Glossary::default()),
        &scope(&["api"]),
        &Sources {
            lexical: Some(&source),
            ..Sources::default()
        },
        &no_expansion(),
    )
    .unwrap();
    let mut snippets = FakeSnippets::default();
    for item in &response.results {
        let (_, text) = bodies
            .iter()
            .find(|(path, _)| *path == item.location.path.as_str())
            .unwrap();
        let range = item.location.range.unwrap();
        snippets.set(
            &item.location,
            SnippetKind::Body,
            (range.start(), range.end()),
            text,
        );
    }
    (response, snippets)
}

struct Counted {
    inner: FakeSnippets,
    bodies: Cell<usize>,
    skeletons: Cell<usize>,
}

impl SnippetSource for Counted {
    fn snippet(&self, request: &SnippetRequest<'_>) -> Result<Option<Snippet>, SourceError> {
        match request.kind {
            SnippetKind::Body => self.bodies.set(self.bodies.get() + 1),
            SnippetKind::Skeleton => self.skeletons.set(self.skeletons.get() + 1),
        }
        self.inner.snippet(request)
    }
}

#[test]
fn source_is_default_and_fetches_actual_bodies_once_without_skeletons() {
    let (response, mut snippets) = fixture(
        "payment validation rounding",
        &[
            ("src/validation.rs", "payment validation\n"),
            ("src/rounding.rs", "currency rounding\n"),
        ],
    );
    for item in &response.results {
        snippets.set(
            &item.location,
            SnippetKind::Skeleton,
            (1, 1),
            &"outline ".repeat(200),
        );
    }
    let counted = Counted {
        inner: snippets,
        bodies: Cell::new(0),
        skeletons: Cell::new(0),
    };
    let options = TaskPackOptions::default();
    assert_eq!(options.strategy, TaskSelectionStrategy::Source);
    let selected = pack_task_with(&response, 12, &counted, &Words, &options).unwrap();
    assert_eq!(selected.pack.items.len(), 2);
    assert_eq!(selected.pack.used_tokens, 12);
    assert!(
        selected
            .pack
            .items
            .iter()
            .all(|item| item.kind == SnippetKind::Body)
    );
    assert_eq!(counted.bodies.get(), 2);
    assert_eq!(counted.skeletons.get(), 0);
}

#[test]
fn actual_body_redundancy_leaves_room_for_complementary_source_and_stops() {
    let (response, snippets) = fixture(
        "payment validation rounding",
        &[
            ("src/first.rs", "payment validation\n"),
            ("src/repeated.rs", "payment validation\n"),
            ("src/rounding.rs", "currency rounding\n"),
        ],
    );
    let selected = pack_task_with(
        &response,
        100,
        &snippets,
        &Words,
        &TaskPackOptions::default(),
    )
    .unwrap();
    let paths: Vec<_> = selected
        .pack
        .items
        .iter()
        .map(|item| item.citation.path.as_str())
        .collect();
    assert_eq!(paths, ["src/first.rs", "src/rounding.rs"]);
    assert_eq!(
        selected.pack.used_tokens, 12,
        "positive-gain stop does not fill the budget"
    );
    assert_eq!(
        selected.selection.unselected,
        [response.results[1].location.clone()]
    );
    assert_eq!(
        selected.pack.items[0].covers.len(),
        0,
        "another path was not certified as covered"
    );
    assert_eq!(
        selected,
        pack_task_with(
            &response,
            100,
            &snippets,
            &Words,
            &TaskPackOptions::default()
        )
        .unwrap()
    );
}

#[test]
fn a_late_critical_query_match_is_emitted_as_a_truthful_budgeted_excerpt() {
    let text = format!(
        "fn guarded() {{\n{}return quota_exceeded;\n}}\n",
        "let padding = value;\n".repeat(80)
    );
    let (response, snippets) = fixture(
        "where does quota_exceeded stop requests?",
        &[
            ("src/open.rs", "fn open() {\naccept()\n}\n"),
            ("src/guard.rs", &text),
        ],
    );
    let selected = pack_task_with(
        &response,
        32,
        &snippets,
        &Words,
        &TaskPackOptions::default(),
    )
    .unwrap();
    let item = selected
        .pack
        .items
        .iter()
        .find(|item| item.citation.path.as_str() == "src/guard.rs")
        .unwrap();
    assert!(item.text.contains("return quota_exceeded;"));
    assert!(item.citation.range.start() > 1);
    assert_eq!(item.citation.range.end(), 83);
    let shown: String = text
        .split_inclusive('\n')
        .skip(usize::try_from(item.citation.range.start() - 1).unwrap())
        .take(usize::try_from(item.citation.range.end() - item.citation.range.start() + 1).unwrap())
        .collect();
    assert_eq!(item.text, shown);
    assert!(selected.pack.used_tokens <= 32);
    assert!(
        selected
            .pack
            .omitted
            .iter()
            .any(|issue| issue.location.path.as_str() == "src/guard.rs"
                && matches!(issue.reason, OmitReason::OverBudget { .. }))
    );
    assert!(
        selected
            .selection
            .role_evidence
            .iter()
            .filter(|evidence| evidence.role == EvidenceRole::Implementation)
            .flat_map(|evidence| &evidence.citations)
            .all(|citation| citation.path.as_str() != "src/guard.rs"),
        "an excerpt cannot certify the full original symbol"
    );
}

#[test]
fn unicode_long_line_is_never_cut_inside_a_character_or_labeled_as_complete() {
    let text = format!("quota_exceeded {}\n", "çığÖ🙂".repeat(1024));
    let (response, snippets) = fixture("find quota_exceeded", &[("src/guard.rs", &text)]);
    let selected =
        pack_task_with(&response, 2, &snippets, &Words, &TaskPackOptions::default()).unwrap();
    assert!(selected.pack.items.is_empty());
    assert_eq!(selected.pack.used_tokens, 0);
    assert!(
        selected
            .pack
            .omitted
            .iter()
            .any(|item| matches!(item.reason, OmitReason::OverBudget { .. }))
    );
}

#[test]
fn a_logical_last_blank_line_keeps_its_declared_range_when_budget_clips_the_body() {
    let text = "noise noise noise noise noise noise noise noise\nalpha\n";
    let (mut response, mut snippets) = fixture("alpha", &[("src/blank.rs", text)]);
    response.results[0].location.range = Some(lines(1, 3));
    snippets.set(
        &response.results[0].location,
        SnippetKind::Body,
        (1, 3),
        text,
    );
    let selected =
        pack_task_with(&response, 6, &snippets, &Words, &TaskPackOptions::default()).unwrap();
    assert_eq!(selected.pack.items.len(), 1);
    assert_eq!(selected.pack.items[0].citation.range, lines(2, 3));
    assert_eq!(selected.pack.items[0].text, "alpha\n");
    assert_eq!(selected.pack.used_tokens, 5);
}

#[test]
fn an_oversized_first_match_does_not_hide_an_affordable_later_match() {
    let text = format!("{}\nalpha omega\n", "alpha omega ".repeat(80));
    let (response, snippets) = fixture("alpha omega", &[("src/long.rs", &text)]);
    let selected = pack_task_with(
        &response,
        10,
        &snippets,
        &Words,
        &TaskPackOptions::default(),
    )
    .unwrap();
    assert_eq!(selected.pack.items.len(), 1);
    assert_eq!(selected.pack.items[0].citation.range, lines(2, 2));
    assert_eq!(selected.pack.items[0].text, "alpha omega\n");
    assert_eq!(selected.pack.used_tokens, 6);
}

#[test]
fn separated_query_regions_can_share_a_response_without_a_fabricated_contiguous_span() {
    let text = format!(
        "alpha\n{}omega\n",
        "padding padding padding padding\n".repeat(100)
    );
    let (response, snippets) = fixture("alpha omega", &[("src/regions.rs", &text)]);
    let selected = pack_task_with(
        &response,
        90,
        &snippets,
        &Words,
        &TaskPackOptions::default(),
    )
    .unwrap();
    assert_eq!(selected.pack.items.len(), 2);
    assert_eq!(selected.pack.items[0].citation.range, lines(1, 9));
    assert_eq!(selected.pack.items[1].citation.range, lines(94, 102));
    assert_eq!(selected.pack.used_tokens, 74);
    for item in &selected.pack.items {
        let actual: String = text
            .split_inclusive('\n')
            .skip(usize::try_from(item.citation.range.start() - 1).unwrap())
            .take(
                usize::try_from(item.citation.range.end() - item.citation.range.start() + 1)
                    .unwrap(),
            )
            .collect();
        assert_eq!(item.text, actual);
    }
}

#[test]
fn partly_overlapping_source_preserves_the_uncovered_tail_without_repeated_lines() {
    let (mut response, mut snippets) = fixture(
        "alpha omega",
        &[
            ("src/first.rs", "alpha\ncommon\n"),
            ("src/second.rs", "common\nomega\n"),
        ],
    );
    response.results[1].location = response.results[0].location.clone();
    response.results[1].location.range = Some(lines(2, 3));
    snippets.set(
        &response.results[1].location,
        SnippetKind::Body,
        (2, 3),
        "common\nomega\n",
    );
    let selected = pack_task_with(
        &response,
        100,
        &snippets,
        &Words,
        &TaskPackOptions::default(),
    )
    .unwrap();
    assert_eq!(selected.pack.items.len(), 2);
    assert_eq!(selected.pack.items[0].citation.range, lines(1, 2));
    assert_eq!(selected.pack.items[1].citation.range, lines(3, 3));
    assert_eq!(selected.pack.items[1].text, "omega\n");
    assert_eq!(selected.pack.used_tokens, 11);
    let exact = pack_task_with(
        &response,
        11,
        &snippets,
        &Words,
        &TaskPackOptions::default(),
    )
    .unwrap();
    assert_eq!(exact.pack.items.len(), 2);
    assert!(
        !exact
            .pack
            .omitted
            .iter()
            .any(|item| matches!(item.reason, OmitReason::OverBudget { .. })),
        "already emitted interval union is not over budget"
    );
}

#[test]
fn explicitly_matched_equal_bodies_keep_each_occurrences_citation_and_cost() {
    let (mut response, snippets) = fixture(
        "locate first::validate and second::validate",
        &[
            ("src/first.rs", "payment validation\n"),
            ("src/second.rs", "payment validation\n"),
        ],
    );
    let terms = ["first::validate", "second::validate"];
    for (item, term) in response.results.iter_mut().zip(terms) {
        item.why.push(Reason::ExactMatch {
            term: term.into(),
            target: knowell_query::ExactTarget::Symbol,
        });
    }
    let selected = pack_task_with(
        &response,
        12,
        &snippets,
        &Words,
        &TaskPackOptions::default(),
    )
    .unwrap();
    assert_eq!(selected.pack.items.len(), 2);
    assert_ne!(
        selected.pack.items[0].citation.path,
        selected.pack.items[1].citation.path
    );
    assert_eq!(selected.pack.used_tokens, 12);
    assert!(
        selected
            .pack
            .items
            .iter()
            .all(|item| item.covers.is_empty())
    );
}

#[test]
fn missing_stale_and_zero_budget_bodies_have_distinct_diagnostics() {
    let (response, mut snippets) = fixture(
        "payment validation",
        &[
            ("src/missing.rs", "missing body\n"),
            ("src/stale.rs", "payment validation\n"),
            ("src/current.rs", "payment validation\n"),
        ],
    );
    snippets
        .bodies
        .remove(&response.results[0].location.label());
    snippets
        .bodies
        .get_mut(&response.results[1].location.label())
        .unwrap()
        .content_hash = blob("changed");
    let selected =
        pack_task_with(&response, 0, &snippets, &Words, &TaskPackOptions::default()).unwrap();
    assert!(selected.pack.items.is_empty());
    assert!(
        selected
            .selection
            .candidate_issues
            .iter()
            .any(|item| matches!(item.reason, OmitReason::NoSnippet))
    );
    assert!(
        selected
            .selection
            .candidate_issues
            .iter()
            .any(|item| matches!(item.reason, OmitReason::StaleContent { .. }))
    );
    assert!(
        selected
            .selection
            .candidate_issues
            .iter()
            .any(|item| matches!(item.reason, OmitReason::OverBudget { .. }))
    );
}

#[test]
fn inconsistent_source_line_counts_are_rejected_without_wrong_citations() {
    let (response, mut snippets) = fixture("alpha", &[("src/bad.rs", "alpha\n")]);
    snippets.set(
        &response.results[0].location,
        SnippetKind::Body,
        (1, 4),
        "alpha\n",
    );
    let selected = pack_task_with(
        &response,
        100,
        &snippets,
        &Words,
        &TaskPackOptions::default(),
    )
    .unwrap();
    assert!(selected.pack.items.is_empty());
    assert!(
        selected
            .selection
            .candidate_issues
            .iter()
            .any(|item| matches!(item.reason, OmitReason::SnippetFailed { .. }))
    );
}

#[test]
fn a_different_commit_is_not_certified_by_an_equal_location_in_the_emitted_commit() {
    let (mut response, mut snippets) = fixture(
        "payment validation",
        &[
            ("src/first.rs", "payment validation\n"),
            ("src/second.rs", "payment validation\n"),
        ],
    );
    response.results[1].location = response.results[0].location.clone();
    response.results[1].commit =
        Some(CommitId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap());
    snippets.set(
        &response.results[1].location,
        SnippetKind::Body,
        (1, 1),
        "payment validation\n",
    );
    let selected = pack_task_with(
        &response,
        100,
        &snippets,
        &Words,
        &TaskPackOptions::default(),
    )
    .unwrap();
    assert_eq!(selected.pack.items.len(), 1);
    assert_eq!(selected.selection.selected_candidates, 1);
    assert_eq!(
        selected.selection.unselected,
        [response.results[1].location.clone()]
    );
    assert_eq!(
        selected.pack.items[0].citation.commit,
        response.results[0].commit
    );
}

#[test]
fn candidate_and_cheap_assessment_work_bounds_remain_explicit() {
    let (response, snippets) = fixture(
        "payment validation rounding",
        &[
            ("src/first.rs", "payment validation\n"),
            ("src/second.rs", "currency rounding\n"),
            ("src/third.rs", "unique constraint\n"),
        ],
    );
    let options = TaskPackOptions {
        candidate_limit: 2,
        evaluation_budget: 1,
        ..TaskPackOptions::default()
    };
    let selected = pack_task_with(&response, 100, &snippets, &Words, &options).unwrap();
    assert_eq!(selected.selection.considered_candidates, 2);
    assert_eq!(selected.selection.omitted_by_candidate_limit, 1);
    assert_eq!(selected.selection.evaluations, 1);
    assert!(selected.selection.evaluation_budget_exhausted);
}

struct RequestedLocations {
    snippets: FakeSnippets,
    locations: RefCell<Vec<Location>>,
}

impl SnippetSource for RequestedLocations {
    fn snippet(&self, request: &SnippetRequest<'_>) -> Result<Option<Snippet>, SourceError> {
        assert_eq!(request.kind, SnippetKind::Body);
        self.locations.borrow_mut().push(request.location.clone());
        self.snippets.snippet(request)
    }
}

#[test]
fn admitted_source_locations_match_exact_body_requests_before_hydration() {
    let (mut response, mut snippets) = fixture(
        "payment validation rounding",
        &[
            ("src/first.rs", "payment validation\n"),
            ("src/second.rs", "currency rounding\n"),
            ("src/third.rs", "unique constraint\n"),
            ("src/fourth.rs", "balance settlement\n"),
            ("src/fifth.rs", "transfer reference\n"),
        ],
    );
    let seed = response.results[0].clone();
    for index in 0..4 {
        let location = hit(
            SourceKind::Lexical,
            1,
            "api",
            &format!("tests/linked_{index}.rs"),
            (1, 1),
        )
        .location();
        snippets.set(&location, SnippetKind::Body, (1, 1), "assert validation\n");
        response.expanded.push(ExpandedItem {
            location,
            commit: seed.commit.clone(),
            layer: seed.layer,
            symbol: Some(format!("linked_{index}")),
            language: seed.language.clone(),
            seed_rank: seed.rank,
            depth: 1,
            score: 0.1,
            path: Vec::new(),
            why: Vec::new(),
        });
    }
    let admitted: Vec<_> = task_source_locations(&response, 4)
        .unwrap()
        .into_iter()
        .cloned()
        .collect();
    assert_eq!(
        admitted,
        [
            response.results[0].location.clone(),
            response.expanded[0].location.clone(),
            response.results[1].location.clone(),
            response.expanded[1].location.clone(),
        ]
    );
    let source = RequestedLocations {
        snippets,
        locations: RefCell::new(Vec::new()),
    };
    let options = TaskPackOptions {
        candidate_limit: 4,
        ..TaskPackOptions::default()
    };
    let selected = pack_task_with(&response, 100, &source, &Words, &options).unwrap();
    assert_eq!(*source.locations.borrow(), admitted);
    assert_eq!(selected.selection.considered_candidates, 4);
    assert_eq!(selected.selection.omitted_by_candidate_limit, 5);
    assert_eq!(
        task_source_locations(&response, 1).unwrap(),
        [&response.results[0].location]
    );
    assert_eq!(task_source_locations(&response, 64).unwrap().len(), 9);
    for invalid in [0, 65, usize::MAX] {
        assert!(matches!(
            task_source_locations(&response, invalid),
            Err(knowell_query::QueryError::InvalidConfig {
                field: "task_pack.candidate_limit",
                ..
            })
        ));
    }
}
