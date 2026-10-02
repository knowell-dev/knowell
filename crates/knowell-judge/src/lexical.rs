//! Deterministic, offline baselines: token-overlap reranker and a keyword
//! query classifier.

use std::collections::BTreeSet;

use async_trait::async_trait;

use crate::common::{HttpOptions, finish, prepare};
use crate::{Candidate, DataPolicy, Descriptor, JudgeError, QueryClassifier, Reranker, Scored};

/// Splits text into lowercase alphanumeric tokens, also breaking
/// `camelCase` and `snake_case` so identifiers match their words.
pub(crate) fn tokens(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            if ch.is_uppercase() && prev_lower && !cur.is_empty() {
                out.insert(std::mem::take(&mut cur));
            }
            cur.extend(ch.to_lowercase());
            prev_lower = ch.is_lowercase();
        } else {
            if !cur.is_empty() {
                out.insert(std::mem::take(&mut cur));
            }
            prev_lower = false;
        }
    }
    if !cur.is_empty() {
        out.insert(cur);
    }
    out
}

/// Scores each candidate by the fraction of distinct query tokens it
/// contains (`|Q ∩ D| / |Q|`, in `[0, 1]`).
///
/// No network, no randomness: the same input always gives the same output.
/// Meant for tests and as the baseline a real reranker must beat in
/// evaluation. It ignores the data policy because nothing leaves the process.
#[derive(Clone, Debug, Default)]
pub struct LexicalOverlapReranker {
    options: HttpOptions,
}

impl LexicalOverlapReranker {
    /// Algorithm version recorded in the [`Descriptor`]; bump it when the
    /// scoring changes so old results stay attributable.
    pub const VERSION: &'static str = "1";

    /// Creates the reranker with default limits (`max_documents` 1000).
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Reranker for LexicalOverlapReranker {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            provider: "lexical-overlap".into(),
            model: String::new(),
            version: Self::VERSION.into(),
        }
    }

    async fn rerank(
        &self,
        _policy: DataPolicy,
        query: &str,
        candidates: &[Candidate],
        top_n: usize,
    ) -> Result<Vec<Scored>, JudgeError> {
        let Some(p) = prepare(&self.options, query, candidates, top_n)? else {
            return Ok(Vec::new());
        };
        let q = tokens(&p.query);
        let hits = p
            .texts
            .iter()
            .enumerate()
            .map(|(i, text)| (i, overlap(&q, &tokens(text))))
            .collect();
        finish(candidates, hits, p.top_n)
    }
}

/// `|q ∩ d| / |q|`, or 0 when the query has no tokens.
fn overlap(q: &BTreeSet<String>, d: &BTreeSet<String>) -> f32 {
    if q.is_empty() {
        return 0.0;
    }
    let hits = q.intersection(d).count();
    // Counts are tiny; the f32 conversion is exact.
    hits as f32 / q.len() as f32
}

/// Offline classifier: a label's weight is the share of its tokens found in
/// the query; weights are normalised to probabilities (uniform when nothing
/// matches). A deterministic stand-in until a model-backed classifier exists.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeywordQueryClassifier;

#[async_trait]
impl QueryClassifier for KeywordQueryClassifier {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            provider: "keyword-classifier".into(),
            model: String::new(),
            version: "1".into(),
        }
    }

    async fn classify(
        &self,
        _policy: DataPolicy,
        query: &str,
        labels: &[String],
    ) -> Result<Vec<(String, f32)>, JudgeError> {
        if labels.is_empty() {
            return Ok(Vec::new());
        }
        let q = tokens(query);
        let weights: Vec<f32> = labels
            .iter()
            .map(|l| {
                let lt = tokens(l);
                if lt.is_empty() {
                    0.0
                } else {
                    lt.intersection(&q).count() as f32 / lt.len() as f32
                }
            })
            .collect();
        let total: f32 = weights.iter().sum();
        let n = labels.len() as f32;
        Ok(labels
            .iter()
            .zip(weights)
            .map(|(l, w)| (l.clone(), if total > 0.0 { w / total } else { 1.0 / n }))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: &str, t: &str) -> Candidate {
        Candidate::new(id, t)
    }

    #[test]
    fn tokenizer_splits_identifiers() {
        let t: Vec<String> = tokens("parseHttpRequest").into_iter().collect();
        assert_eq!(t, ["http", "parse", "request"]);
        let u: Vec<String> = tokens("HTTP2 Ünï").into_iter().collect();
        assert_eq!(u, ["http2", "ünï"]);
        assert!(tokens("user_id").contains("user") && tokens("user_id").contains("id"));
        assert!(tokens("   ").is_empty());
    }

    #[tokio::test]
    async fn ranks_by_overlap_and_is_deterministic() {
        let r = LexicalOverlapReranker::new();
        let cands = [
            c("a", "unrelated words only"),
            c("b", "open the database connection pool"),
            c("c", "database connection"),
            c("d", "database connection"),
        ];
        let q = "database connection pool";
        let one = r
            .rerank(DataPolicy::LOCAL_ONLY, q, &cands, 4)
            .await
            .unwrap();
        let two = r
            .rerank(DataPolicy::LOCAL_ONLY, q, &cands, 4)
            .await
            .unwrap();
        assert_eq!(one, two);
        let ids: Vec<_> = one.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["b", "c", "d", "a"]);
        assert!((one[0].score - 1.0).abs() < 1e-6);
        assert!(one[3].score.abs() < 1e-6);
    }

    #[tokio::test]
    async fn top_n_clamps_and_validates() {
        let r = LexicalOverlapReranker::new();
        let cands = [c("a", "x y"), c("b", "x")];
        assert_eq!(
            r.rerank(DataPolicy::default(), "x", &cands, 1)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            r.rerank(DataPolicy::default(), "x", &cands, 99)
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(
            r.rerank(DataPolicy::default(), "x", &cands, 0)
                .await
                .is_err()
        );
        assert!(
            r.rerank(DataPolicy::default(), "", &cands, 1)
                .await
                .is_err()
        );
        assert!(
            r.rerank(DataPolicy::default(), "x", &[], 1)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn query_without_tokens_scores_zero() {
        let r = LexicalOverlapReranker::new();
        let out = r
            .rerank(DataPolicy::default(), "!!!", &[c("a", "x"), c("b", "y")], 2)
            .await
            .unwrap();
        assert_eq!(
            out.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    #[tokio::test]
    async fn classifier_normalises() {
        let k = KeywordQueryClassifier;
        let labels = [
            "find definition".to_owned(),
            "explain usage".to_owned(),
            "other".to_owned(),
        ];
        let out = k
            .classify(
                DataPolicy::default(),
                "where is the definition of foo",
                &labels,
            )
            .await
            .unwrap();
        assert_eq!(out.len(), 3);
        assert!((out.iter().map(|x| x.1).sum::<f32>() - 1.0).abs() < 1e-6);
        assert!(out[0].1 > out[1].1);
        let uniform = k
            .classify(DataPolicy::default(), "zzz", &labels)
            .await
            .unwrap();
        assert!(uniform.iter().all(|x| (x.1 - 1.0 / 3.0).abs() < 1e-6));
        assert!(
            k.classify(DataPolicy::default(), "q", &[])
                .await
                .unwrap()
                .is_empty()
        );
    }
}
