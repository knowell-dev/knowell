//! Comparing a report with a baseline.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::error::EvalError;
use crate::metrics::round4;
use crate::report::{MetricSummary, Report};

/// One metric that moved by more than the tolerance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricDelta {
    /// Retriever name.
    pub retriever: String,
    /// `overall`, `kind:<kind>` or `lang:<code>`.
    pub scope: String,
    /// `recall@1`, `recall@5`, `recall@10`, `mrr@10`, `ndcg@10` or `abstain_rate`.
    pub metric: String,
    /// Baseline value.
    pub baseline: f64,
    /// Current value.
    pub current: f64,
    /// `current - baseline`, rounded to 4 decimals.
    pub delta: f64,
}

/// Differences between a current report and a baseline that measured the
/// same fixture and query set. Every metric is "higher is better".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    /// Absolute change tolerated before a delta counts.
    pub tolerance: f64,
    /// True when the corpora differ although the fixture is the same (e.g.
    /// the walker's exclusion rules changed); deltas may then reflect the
    /// corpus rather than the retriever.
    pub corpus_changed: bool,
    /// Metrics that dropped by more than the tolerance.
    pub regressions: Vec<MetricDelta>,
    /// Metrics that rose by more than the tolerance.
    pub improvements: Vec<MetricDelta>,
    /// Retrievers present in the baseline but not in the current report.
    pub missing_retrievers: Vec<String>,
    /// Retrievers present only in the current report.
    pub new_retrievers: Vec<String>,
}

impl Comparison {
    /// True when anything got worse, including a retriever that disappeared.
    pub fn has_regressions(&self) -> bool {
        !self.regressions.is_empty() || !self.missing_retrievers.is_empty()
    }

    /// Markdown summary of the changes.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        // Writing to a String cannot fail.
        let _ = writeln!(
            out,
            "# Comparison with baseline (tolerance {})\n",
            self.tolerance
        );
        if self.corpus_changed {
            let _ = writeln!(out, "Note: the corpus differs from the baseline's.\n");
        }
        for (title, deltas) in [
            ("Regressions", &self.regressions),
            ("Improvements", &self.improvements),
        ] {
            let _ = writeln!(out, "## {title}\n");
            if deltas.is_empty() {
                let _ = writeln!(out, "None.\n");
                continue;
            }
            let _ = writeln!(
                out,
                "| Retriever | Scope | Metric | Baseline | Current | Delta |"
            );
            let _ = writeln!(out, "|---|---|---|---:|---:|---:|");
            for d in deltas {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {:.4} | {:.4} | {:+.4} |",
                    d.retriever, d.scope, d.metric, d.baseline, d.current, d.delta
                );
            }
            let _ = writeln!(out);
        }
        for name in &self.missing_retrievers {
            let _ = writeln!(out, "Missing retriever: `{name}`");
        }
        for name in &self.new_retrievers {
            let _ = writeln!(out, "New retriever: `{name}`");
        }
        out
    }
}

/// Compares `current` with `baseline`, metric by metric, for every
/// retriever and scope (overall, per kind, per language).
///
/// Errors when the reports did not measure the same thing: different
/// fixture identity (name, seed, scale, tree hash), different query-set hash,
/// or — for ad-hoc corpora without a fixture — different corpus hash.
pub fn compare(
    current: &Report,
    baseline: &Report,
    tolerance: f64,
) -> Result<Comparison, EvalError> {
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err(EvalError::InvalidTolerance(tolerance));
    }
    match (&current.fixture, &baseline.fixture) {
        (Some(a), Some(b)) if a != b => {
            return Err(EvalError::Incomparable(format!(
                "fixture {}/{}/{} (tree {}) vs baseline {}/{}/{} (tree {})",
                a.name,
                a.seed,
                a.scale,
                a.tree_hash.short(),
                b.name,
                b.seed,
                b.scale,
                b.tree_hash.short()
            )));
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(EvalError::Incomparable(
                "one report was run on a fixture, the other on an ad-hoc corpus".to_owned(),
            ));
        }
        (None, None) if current.corpus_hash != baseline.corpus_hash => {
            return Err(EvalError::Incomparable(
                "ad-hoc corpora differ (corpus hash)".to_owned(),
            ));
        }
        _ => {}
    }
    if current.query_set.hash != baseline.query_set.hash {
        return Err(EvalError::Incomparable(format!(
            "query set {} vs baseline {}",
            current.query_set.hash.short(),
            baseline.query_set.hash.short()
        )));
    }

    let mut comparison = Comparison {
        tolerance,
        corpus_changed: current.corpus_hash != baseline.corpus_hash,
        regressions: Vec::new(),
        improvements: Vec::new(),
        missing_retrievers: Vec::new(),
        new_retrievers: current
            .retrievers
            .iter()
            .filter(|r| baseline.retriever(&r.name).is_none())
            .map(|r| r.name.clone())
            .collect(),
    };

    for base in &baseline.retrievers {
        let Some(cur) = current.retriever(&base.name) else {
            comparison.missing_retrievers.push(base.name.clone());
            continue;
        };
        let mut scopes: Vec<(String, &MetricSummary, Option<&MetricSummary>)> =
            vec![("overall".to_owned(), &base.overall, Some(&cur.overall))];
        for (kind, summary) in &base.by_kind {
            scopes.push((format!("kind:{kind}"), summary, cur.by_kind.get(kind)));
        }
        for (lang, summary) in &base.by_lang {
            scopes.push((format!("lang:{lang}"), summary, cur.by_lang.get(lang)));
        }
        for (scope, base_summary, cur_summary) in scopes {
            let Some(cur_summary) = cur_summary else {
                continue; // impossible with equal query-set hashes
            };
            for ((metric, b), (_, c)) in base_summary
                .metrics()
                .into_iter()
                .zip(cur_summary.metrics())
            {
                let (Some(b), Some(c)) = (b, c) else {
                    continue;
                };
                let delta = round4(c - b);
                let entry = MetricDelta {
                    retriever: base.name.clone(),
                    scope: scope.clone(),
                    metric: metric.to_owned(),
                    baseline: b,
                    current: c,
                    delta,
                };
                if delta < -tolerance {
                    comparison.regressions.push(entry);
                } else if delta > tolerance {
                    comparison.improvements.push(entry);
                }
            }
        }
    }
    Ok(comparison)
}

#[cfg(test)]
mod tests {
    use knowell_core::ContentHash;

    use super::*;
    use crate::fixture::{FixtureInfo, Scale};
    use crate::queries::QuerySetInfo;
    use crate::report::{REPORT_FORMAT_VERSION, RetrieverReport};

    fn summary(recall: f64) -> MetricSummary {
        MetricSummary {
            ranked_queries: 2,
            absent_queries: 1,
            recall_at_1: Some(recall),
            recall_at_5: Some(recall),
            recall_at_10: Some(recall),
            mrr_at_10: Some(0.5),
            ndcg_at_10: Some(0.5),
            abstain_rate: Some(0.0),
        }
    }

    fn report(name: &str, recall: f64) -> Report {
        Report {
            format_version: REPORT_FORMAT_VERSION,
            fixture: Some(FixtureInfo {
                name: "acme-goods".into(),
                seed: 42,
                scale: Scale::Small,
                tree_hash: ContentHash::of(b"tree"),
            }),
            query_set: QuerySetInfo {
                fixture: "acme-goods".into(),
                hash: ContentHash::of(b"queries"),
                queries: 3,
                ranked: 2,
                absent: 1,
            },
            corpus_docs: 10,
            corpus_hash: ContentHash::of(b"corpus"),
            depth: 10,
            judged_docs_missing_from_corpus: vec![],
            retrievers: vec![RetrieverReport {
                name: name.into(),
                overall: summary(recall),
                by_kind: [("symbol".to_owned(), summary(recall))].into(),
                by_lang: [("en".to_owned(), summary(recall))].into(),
                queries: vec![],
            }],
        }
    }

    #[test]
    fn detects_regressions_and_improvements_beyond_tolerance() {
        let base = report("grep", 0.5);
        let worse = compare(&report("grep", 0.4), &base, 0.01).unwrap();
        assert!(worse.has_regressions());
        // 3 recall metrics x 3 scopes.
        assert_eq!(worse.regressions.len(), 9);
        assert_eq!(worse.regressions[0].scope, "overall");
        assert_eq!(worse.regressions[0].metric, "recall@1");
        assert_eq!(worse.regressions[0].delta, -0.1);
        assert!(worse.improvements.is_empty());

        let better = compare(&report("grep", 0.6), &base, 0.01).unwrap();
        assert!(!better.has_regressions());
        assert_eq!(better.improvements.len(), 9);

        let within = compare(&report("grep", 0.51), &base, 0.01).unwrap();
        assert!(within.regressions.is_empty() && within.improvements.is_empty());
        assert!(!within.corpus_changed);
        assert!(within.to_markdown().contains("None."));
    }

    #[test]
    fn reports_missing_and_new_retrievers() {
        let c = compare(&report("bm25", 0.5), &report("grep", 0.5), 0.0).unwrap();
        assert_eq!(c.missing_retrievers, ["grep"]);
        assert_eq!(c.new_retrievers, ["bm25"]);
        assert!(c.has_regressions());
        assert!(c.to_markdown().contains("Missing retriever: `grep`"));
    }

    #[test]
    fn refuses_incomparable_reports() {
        let base = report("grep", 0.5);
        let mut other_seed = report("grep", 0.5);
        if let Some(f) = other_seed.fixture.as_mut() {
            f.seed = 1;
        }
        assert!(matches!(
            compare(&other_seed, &base, 0.0),
            Err(EvalError::Incomparable(_))
        ));

        let mut other_queries = report("grep", 0.5);
        other_queries.query_set.hash = ContentHash::of(b"other");
        assert!(matches!(
            compare(&other_queries, &base, 0.0),
            Err(EvalError::Incomparable(_))
        ));

        let mut adhoc = report("grep", 0.5);
        adhoc.fixture = None;
        assert!(compare(&adhoc, &base, 0.0).is_err());

        assert!(matches!(
            compare(&base, &base, -1.0),
            Err(EvalError::InvalidTolerance(_))
        ));
        assert!(compare(&base, &base, f64::NAN).is_err());

        let mut walker = report("grep", 0.5);
        walker.corpus_hash = ContentHash::of(b"walked");
        assert!(compare(&walker, &base, 0.0).unwrap().corpus_changed);
    }
}
