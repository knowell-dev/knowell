//! The whole pipeline on the Small fixture with the grep baseline.

// Shared helpers outside #[test] functions may fail loudly too.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use knowell_eval::{
    FixtureSpec, GrepRetriever, QueryKind, QuerySet, Scale, compare, generate, run,
};

fn small_report() -> (knowell_eval::Fixture, knowell_eval::Report) {
    let fixture = generate(&FixtureSpec {
        seed: 42,
        scale: Scale::Small,
    });
    let queries = QuerySet::builtin().unwrap();
    queries.validate_against(&fixture).unwrap();
    let corpus = fixture.to_corpus();
    let grep = GrepRetriever::new(&corpus);
    let report = run(&corpus, &queries, &[&grep], 10).unwrap();
    (fixture, report)
}

#[test]
fn grep_baseline_runs_end_to_end() {
    let (fixture, report) = small_report();
    let queries = QuerySet::builtin().unwrap();
    assert_eq!(report.fixture.as_ref(), Some(&fixture.info()));
    assert_eq!(report.query_set.hash, queries.hash());
    assert_eq!(report.corpus_docs, Scale::Small.target_files());
    assert!(report.judged_docs_missing_from_corpus.is_empty());

    let grep = report.retriever("grep").unwrap();
    assert_eq!(grep.queries.len(), queries.queries.len());
    let absent = queries
        .queries
        .iter()
        .filter(|q| q.kind == QueryKind::Absent)
        .count();
    assert_eq!(grep.overall.absent_queries, absent);
    assert_eq!(grep.overall.ranked_queries, queries.queries.len() - absent);
    // Smoke check that judgments line up with file contents: exact
    // identifiers are what grep is good at.
    let symbol = &grep.by_kind["symbol"];
    assert!(symbol.recall_at_10.unwrap() >= 0.5, "{symbol:?}");
}

#[test]
fn reports_are_reproducible_and_comparable_with_themselves() {
    let (_, a) = small_report();
    let (_, b) = small_report();
    assert_eq!(a.to_json().unwrap(), b.to_json().unwrap());
    let comparison = compare(&a, &b, 0.0).unwrap();
    assert!(!comparison.has_regressions());
    assert!(comparison.improvements.is_empty());
    assert!(!comparison.corpus_changed);
}

#[test]
fn canaries_never_reach_reports() {
    let (fixture, report) = small_report();
    let json = report.to_json().unwrap();
    let markdown = report.to_markdown();
    for canary in fixture.canaries() {
        assert!(!json.contains(&canary));
        assert!(!markdown.contains(&canary));
    }
}

#[test]
fn different_seeds_are_not_comparable() {
    let (_, a) = small_report();
    let fixture = generate(&FixtureSpec {
        seed: 43,
        scale: Scale::Small,
    });
    let corpus = fixture.to_corpus();
    let grep = GrepRetriever::new(&corpus);
    let b = run(&corpus, &QuerySet::builtin().unwrap(), &[&grep], 10).unwrap();
    assert!(compare(&a, &b, 0.0).is_err());
}

/// Prints the grep baseline for the Small fixture:
/// `cargo test -p knowell-eval --test end_to_end -- --ignored --nocapture`.
#[test]
#[ignore = "prints a report for humans"]
fn print_grep_baseline_small() {
    let (_, report) = small_report();
    println!("{}", report.to_markdown());
}
