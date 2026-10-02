//! Running retrievers over a query set and reporting the results.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use knowell_core::ContentHash;
use serde::{Deserialize, Serialize};

use crate::corpus::Corpus;
use crate::error::EvalError;
use crate::fixture::FixtureInfo;
use crate::metrics::{
    RANK_CUTOFF, first_relevant_rank, ndcg_at, recall_at, reciprocal_rank_at, round4,
};
use crate::queries::{QueryKind, QuerySet, QuerySetInfo};
use crate::retriever::Retriever;

/// Version of the report format.
pub const REPORT_FORMAT_VERSION: u32 = 1;

/// Number of top result ids kept per query in the report, for debugging.
pub const TOP_IDS_IN_REPORT: usize = 5;

/// Aggregated metrics of one retriever over a group of queries.
///
/// Ranking metrics are macro-averages over the group's non-`absent` queries
/// and are `None` when the group has none; `abstain_rate` is the fraction of
/// the group's `absent` queries for which the retriever returned nothing,
/// `None` when the group has no `absent` query. Values are rounded to 4
/// decimals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricSummary {
    /// Queries scored with ranking metrics.
    pub ranked_queries: usize,
    /// `absent` queries.
    pub absent_queries: usize,
    /// Mean Recall@1.
    pub recall_at_1: Option<f64>,
    /// Mean Recall@5.
    pub recall_at_5: Option<f64>,
    /// Mean Recall@10.
    pub recall_at_10: Option<f64>,
    /// Mean reciprocal rank of the first relevant document within 10.
    pub mrr_at_10: Option<f64>,
    /// Mean nDCG@10 with gains `2^g - 1`.
    pub ndcg_at_10: Option<f64>,
    /// Fraction of `absent` queries answered with no result.
    pub abstain_rate: Option<f64>,
}

impl MetricSummary {
    /// `(name, value)` pairs in a fixed order, as used by comparisons.
    pub fn metrics(&self) -> [(&'static str, Option<f64>); 6] {
        [
            ("recall@1", self.recall_at_1),
            ("recall@5", self.recall_at_5),
            ("recall@10", self.recall_at_10),
            ("mrr@10", self.mrr_at_10),
            ("ndcg@10", self.ndcg_at_10),
            ("abstain_rate", self.abstain_rate),
        ]
    }
}

/// Ranking metrics of one query (rounded to 4 decimals).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryMetrics {
    /// Recall@1.
    pub recall_at_1: f64,
    /// Recall@5.
    pub recall_at_5: f64,
    /// Recall@10.
    pub recall_at_10: f64,
    /// Reciprocal rank within 10.
    pub reciprocal_rank: f64,
    /// nDCG@10.
    pub ndcg_at_10: f64,
    /// 1-based rank of the first relevant result within the returned list.
    pub first_relevant_rank: Option<usize>,
}

/// The outcome of one query for one retriever.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryOutcome {
    /// Query id.
    pub id: String,
    /// Query kind.
    pub kind: QueryKind,
    /// Query language code.
    pub lang: String,
    /// Number of results returned (after truncation to the depth).
    pub returned: usize,
    /// The first few result ids.
    pub top: Vec<String>,
    /// Ranking metrics; `None` for `absent` queries.
    pub metrics: Option<QueryMetrics>,
}

/// Results of one retriever.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrieverReport {
    /// Retriever name.
    pub name: String,
    /// Over all queries.
    pub overall: MetricSummary,
    /// Per query kind (keys as in query files).
    pub by_kind: BTreeMap<String, MetricSummary>,
    /// Per language code.
    pub by_lang: BTreeMap<String, MetricSummary>,
    /// Per query, in query-set order.
    pub queries: Vec<QueryOutcome>,
}

/// What a run measured and how each retriever did. Serialises to stable
/// JSON: fixed field order, sorted maps, floats rounded to 4 decimals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// [`REPORT_FORMAT_VERSION`].
    pub format_version: u32,
    /// The fixture the corpus came from, if known.
    pub fixture: Option<FixtureInfo>,
    /// The query set.
    pub query_set: QuerySetInfo,
    /// Number of documents searched.
    pub corpus_docs: usize,
    /// [`Corpus::hash`].
    pub corpus_hash: ContentHash,
    /// Results requested per query.
    pub depth: usize,
    /// Judged documents that are not in the corpus (e.g. excluded by the
    /// walker); they still count as relevant, so recall cannot reach 1.
    pub judged_docs_missing_from_corpus: Vec<String>,
    /// One entry per retriever, in the order given to [`run`].
    pub retrievers: Vec<RetrieverReport>,
}

/// Runs every retriever on every query and computes the metrics.
///
/// `depth` is the number of results requested per query and must be at
/// least [`RANK_CUTOFF`]. Retrievers must not return duplicate ids (an
/// error, since it would inflate nDCG); results beyond `depth` are ignored.
pub fn run(
    corpus: &Corpus,
    queries: &QuerySet,
    retrievers: &[&dyn Retriever],
    depth: usize,
) -> Result<Report, EvalError> {
    if depth < RANK_CUTOFF {
        return Err(EvalError::DepthTooSmall {
            depth,
            min: RANK_CUTOFF,
        });
    }
    let mut names = BTreeSet::new();
    for retriever in retrievers {
        if !names.insert(retriever.name()) {
            return Err(EvalError::DuplicateRetriever(retriever.name().to_owned()));
        }
    }

    let missing: BTreeSet<String> = queries
        .queries
        .iter()
        .flat_map(|q| q.relevant.iter())
        .filter(|j| !corpus.contains(&j.doc))
        .map(|j| j.doc.clone())
        .collect();

    let mut reports = Vec::with_capacity(retrievers.len());
    for retriever in retrievers {
        reports.push(run_one(*retriever, queries, depth)?);
    }

    Ok(Report {
        format_version: REPORT_FORMAT_VERSION,
        fixture: corpus.fixture().cloned(),
        query_set: queries.info(),
        corpus_docs: corpus.len(),
        corpus_hash: corpus.hash(),
        depth,
        judged_docs_missing_from_corpus: missing.into_iter().collect(),
        retrievers: reports,
    })
}

/// Unrounded per-query values used for averaging.
struct Raw {
    kind: QueryKind,
    lang: &'static str,
    ranked: Option<[f64; 5]>,
    abstained: Option<bool>,
}

fn run_one(
    retriever: &dyn Retriever,
    queries: &QuerySet,
    depth: usize,
) -> Result<RetrieverReport, EvalError> {
    let mut outcomes = Vec::with_capacity(queries.queries.len());
    let mut raws = Vec::with_capacity(queries.queries.len());
    for query in &queries.queries {
        let mut results = retriever.search(&query.text, depth)?;
        results.truncate(depth);
        let ids: Vec<String> = results.into_iter().map(|r| r.id).collect();
        let mut seen = BTreeSet::new();
        if let Some(dup) = ids.iter().find(|id| !seen.insert(id.as_str())) {
            return Err(EvalError::Retriever {
                retriever: retriever.name().to_owned(),
                message: format!("returned `{dup}` twice for query `{}`", query.id),
            });
        }

        let (metrics, raw_ranked, abstained) = if query.kind == QueryKind::Absent {
            (None, None, Some(ids.is_empty()))
        } else {
            let values = [
                recall_at(&ids, &query.relevant, 1),
                recall_at(&ids, &query.relevant, 5),
                recall_at(&ids, &query.relevant, 10),
                reciprocal_rank_at(&ids, &query.relevant, RANK_CUTOFF),
                ndcg_at(&ids, &query.relevant, RANK_CUTOFF),
            ];
            let [r1, r5, r10, rr, ndcg] = values;
            let metrics = QueryMetrics {
                recall_at_1: round4(r1),
                recall_at_5: round4(r5),
                recall_at_10: round4(r10),
                reciprocal_rank: round4(rr),
                ndcg_at_10: round4(ndcg),
                first_relevant_rank: first_relevant_rank(&ids, &query.relevant),
            };
            (Some(metrics), Some(values), None)
        };
        raws.push(Raw {
            kind: query.kind,
            lang: query.lang.as_str(),
            ranked: raw_ranked,
            abstained,
        });
        outcomes.push(QueryOutcome {
            id: query.id.clone(),
            kind: query.kind,
            lang: query.lang.as_str().to_owned(),
            returned: ids.len(),
            top: ids.iter().take(TOP_IDS_IN_REPORT).cloned().collect(),
            metrics,
        });
    }

    let overall = summarize(raws.iter());
    let mut by_kind = BTreeMap::new();
    let kinds: BTreeSet<QueryKind> = raws.iter().map(|r| r.kind).collect();
    for kind in kinds {
        by_kind.insert(
            kind.as_str().to_owned(),
            summarize(raws.iter().filter(|r| r.kind == kind)),
        );
    }
    let mut by_lang = BTreeMap::new();
    let langs: BTreeSet<&str> = raws.iter().map(|r| r.lang).collect();
    for lang in langs {
        by_lang.insert(
            lang.to_owned(),
            summarize(raws.iter().filter(|r| r.lang == lang)),
        );
    }

    Ok(RetrieverReport {
        name: retriever.name().to_owned(),
        overall,
        by_kind,
        by_lang,
        queries: outcomes,
    })
}

fn summarize<'a>(raws: impl Iterator<Item = &'a Raw>) -> MetricSummary {
    let mut sums = [0.0f64; 5];
    let mut ranked = 0usize;
    let mut absent = 0usize;
    let mut abstained = 0usize;
    for raw in raws {
        if let Some(values) = raw.ranked {
            ranked += 1;
            for (sum, value) in sums.iter_mut().zip(values) {
                *sum += value;
            }
        }
        if let Some(did_abstain) = raw.abstained {
            absent += 1;
            if did_abstain {
                abstained += 1;
            }
        }
    }
    let mean = |sum: f64| (ranked > 0).then(|| round4(sum / ranked as f64));
    let [r1, r5, r10, rr, ndcg] = sums;
    MetricSummary {
        ranked_queries: ranked,
        absent_queries: absent,
        recall_at_1: mean(r1),
        recall_at_5: mean(r5),
        recall_at_10: mean(r10),
        mrr_at_10: mean(rr),
        ndcg_at_10: mean(ndcg),
        abstain_rate: (absent > 0).then(|| round4(abstained as f64 / absent as f64)),
    }
}

impl Report {
    /// Pretty JSON with a trailing newline.
    pub fn to_json(&self) -> Result<String, EvalError> {
        serde_json::to_string_pretty(self)
            .map(|json| json + "\n")
            .map_err(|e| EvalError::Serialize {
                what: "report",
                message: e.to_string(),
            })
    }

    /// Parses a report written by [`Report::to_json`].
    pub fn from_json(text: &str) -> Result<Report, EvalError> {
        serde_json::from_str(text).map_err(|e| EvalError::ReportParse(e.to_string()))
    }

    /// The retriever row named `name`.
    pub fn retriever(&self, name: &str) -> Option<&RetrieverReport> {
        self.retrievers.iter().find(|r| r.name == name)
    }

    /// Markdown summary: header with what was measured, then overall, per
    /// kind and per language tables.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        // Writing to a String cannot fail; results are ignored below.
        let _ = writeln!(out, "# Knowell retrieval evaluation\n");
        match &self.fixture {
            Some(f) => {
                let _ = writeln!(
                    out,
                    "- Fixture: `{}` seed {} scale {} (tree `{}`)",
                    f.name,
                    f.seed,
                    f.scale,
                    f.tree_hash.short()
                );
            }
            None => {
                let _ = writeln!(out, "- Fixture: none (ad-hoc corpus)");
            }
        }
        let q = &self.query_set;
        let _ = writeln!(
            out,
            "- Query set: `{}`, {} queries ({} ranked, {} absent), hash `{}`",
            q.fixture,
            q.queries,
            q.ranked,
            q.absent,
            q.hash.short()
        );
        let _ = writeln!(
            out,
            "- Corpus: {} documents, hash `{}`; depth {}",
            self.corpus_docs,
            self.corpus_hash.short(),
            self.depth
        );
        if !self.judged_docs_missing_from_corpus.is_empty() {
            let _ = writeln!(
                out,
                "- Judged documents missing from the corpus: {}",
                self.judged_docs_missing_from_corpus.len()
            );
        }

        let _ = writeln!(out, "\n## Overall\n");
        table_header(&mut out, "Retriever");
        for r in &self.retrievers {
            table_row(&mut out, &r.name, &r.overall);
        }

        let _ = writeln!(out, "\n## By kind\n");
        table_header(&mut out, "Retriever / kind");
        for r in &self.retrievers {
            for (kind, summary) in &r.by_kind {
                table_row(&mut out, &format!("{} / {kind}", r.name), summary);
            }
        }

        let _ = writeln!(out, "\n## By language\n");
        table_header(&mut out, "Retriever / lang");
        for r in &self.retrievers {
            for (lang, summary) in &r.by_lang {
                table_row(&mut out, &format!("{} / {lang}", r.name), summary);
            }
        }
        out
    }
}

fn table_header(out: &mut String, first: &str) {
    let _ = writeln!(
        out,
        "| {first} | Ranked | R@1 | R@5 | R@10 | MRR@10 | nDCG@10 | Absent | Abstain |"
    );
    let _ = writeln!(out, "|---|---:|---:|---:|---:|---:|---:|---:|---:|");
}

fn table_row(out: &mut String, label: &str, s: &MetricSummary) {
    let _ = writeln!(
        out,
        "| {label} | {} | {} | {} | {} | {} | {} | {} | {} |",
        s.ranked_queries,
        cell(s.recall_at_1),
        cell(s.recall_at_5),
        cell(s.recall_at_10),
        cell(s.mrr_at_10),
        cell(s.ndcg_at_10),
        s.absent_queries,
        cell(s.abstain_rate)
    );
}

fn cell(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |v| format!("{v:.4}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::CorpusDoc;
    use crate::retriever::RankedDoc;

    /// Returns a fixed list per query text.
    struct Scripted {
        name: &'static str,
        answers: Vec<(&'static str, Vec<&'static str>)>,
    }

    impl Retriever for Scripted {
        fn name(&self) -> &str {
            self.name
        }

        fn search(&self, query: &str, k: usize) -> Result<Vec<RankedDoc>, EvalError> {
            let ids = self
                .answers
                .iter()
                .find(|(q, _)| *q == query)
                .map(|(_, ids)| ids.clone())
                .unwrap_or_default();
            Ok(ids
                .into_iter()
                .take(k + 3) // deliberately more than asked; run() truncates
                .map(|id| RankedDoc {
                    id: id.to_owned(),
                    score: 1.0,
                })
                .collect())
        }
    }

    const SET: &str = r#"
version = 1
fixture = "acme-goods"

[[query]]
id = "q1"
lang = "en"
kind = "symbol"
text = "one"
relevant = [{ doc = "p/a", grade = 3 }, { doc = "p/b", grade = 1 }, { doc = "p/c", grade = 2 }]

[[query]]
id = "q2"
lang = "tr"
kind = "behavior"
text = "two"
relevant = [{ doc = "p/z", grade = 2 }]

[[query]]
id = "q3"
lang = "en"
kind = "absent"
text = "three"

[[query]]
id = "q4"
lang = "tr"
kind = "absent"
text = "four"
"#;

    fn corpus() -> Corpus {
        Corpus::from_docs(
            ["p/a", "p/b", "p/x", "p/z", "p/y"]
                .iter()
                .map(|id| CorpusDoc {
                    id: (*id).to_owned(),
                    text: String::new(),
                }),
        )
        .unwrap()
    }

    fn scripted() -> Scripted {
        Scripted {
            name: "scripted",
            answers: vec![
                ("one", vec!["p/a", "p/x", "p/b"]),
                ("two", vec!["p/x", "p/y", "p/a", "p/z"]),
                ("three", vec![]),
                ("four", vec!["p/x"]),
            ],
        }
    }

    #[test]
    fn aggregates_hand_computed_metrics() {
        let set = QuerySet::from_toml_str(SET).unwrap();
        let s = scripted();
        let report = run(&corpus(), &set, &[&s], 10).unwrap();
        let r = &report.retrievers[0];

        // q1: R@1 = 1/3, R@5 = 2/3, RR = 1, nDCG = 0.798486…
        // q2: R@1 = 0, R@5 = 1, RR = 1/4, nDCG = 1/log2(5) = 0.430677…
        let o = &r.overall;
        assert_eq!(o.ranked_queries, 2);
        assert_eq!(o.absent_queries, 2);
        assert_eq!(o.recall_at_1, Some(round4((1.0 / 3.0) / 2.0)));
        assert_eq!(o.recall_at_5, Some(round4((2.0 / 3.0 + 1.0) / 2.0)));
        assert_eq!(o.mrr_at_10, Some(0.625));
        let ndcg_q1 = 7.5 / (7.0 + 3.0 / 3f64.log2() + 0.5);
        let ndcg_q2 = 1.0 / 5f64.log2();
        assert_eq!(o.ndcg_at_10, Some(round4((ndcg_q1 + ndcg_q2) / 2.0)));
        assert_eq!(o.abstain_rate, Some(0.5));

        let symbol = &r.by_kind["symbol"];
        assert_eq!(symbol.ranked_queries, 1);
        assert_eq!(symbol.abstain_rate, None);
        let absent = &r.by_kind["absent"];
        assert_eq!(absent.ranked_queries, 0);
        assert_eq!(absent.recall_at_10, None);
        assert_eq!(absent.abstain_rate, Some(0.5));
        assert_eq!(r.by_lang["en"].abstain_rate, Some(1.0));
        assert_eq!(r.by_lang["tr"].abstain_rate, Some(0.0));
        assert_eq!(r.by_lang["tr"].mrr_at_10, Some(0.25));

        assert_eq!(
            r.queries[1].metrics.as_ref().unwrap().first_relevant_rank,
            Some(4)
        );
        assert_eq!(r.queries[2].metrics, None);
        assert_eq!(r.queries[3].returned, 1);
        assert_eq!(report.judged_docs_missing_from_corpus, ["p/c"]);
        assert!(report.fixture.is_none());
    }

    #[test]
    fn json_is_stable_and_round_trips() {
        let set = QuerySet::from_toml_str(SET).unwrap();
        let s = scripted();
        let a = run(&corpus(), &set, &[&s], 10).unwrap();
        let b = run(&corpus(), &set, &[&s], 10).unwrap();
        let json = a.to_json().unwrap();
        assert_eq!(json, b.to_json().unwrap());
        assert!(json.ends_with('\n'));
        assert!(json.find("\"format_version\"").unwrap() < json.find("\"retrievers\"").unwrap());
        assert_eq!(Report::from_json(&json).unwrap(), a);
        assert!(Report::from_json("{").is_err());
    }

    #[test]
    fn markdown_lists_every_group() {
        let set = QuerySet::from_toml_str(SET).unwrap();
        let s = scripted();
        let md = run(&corpus(), &set, &[&s], 10).unwrap().to_markdown();
        assert!(md.contains("| scripted | 2 | 0.1667 | 0.8333 |"), "{md}");
        assert!(md.contains("| scripted / absent | 0 | n/a |"));
        assert!(md.contains("| scripted / tr |"));
        assert!(md.contains("Judged documents missing from the corpus: 1"));
    }

    #[test]
    fn rejects_bad_runs() {
        let set = QuerySet::from_toml_str(SET).unwrap();
        let s = scripted();
        assert!(matches!(
            run(&corpus(), &set, &[&s], 9),
            Err(EvalError::DepthTooSmall { depth: 9, min: 10 })
        ));
        assert!(matches!(
            run(&corpus(), &set, &[&s, &s], 10),
            Err(EvalError::DuplicateRetriever(_))
        ));
        let dup = Scripted {
            name: "dup",
            answers: vec![("one", vec!["p/a", "p/a"])],
        };
        assert!(matches!(
            run(&corpus(), &set, &[&dup], 10),
            Err(EvalError::Retriever { .. })
        ));
    }
}
