//! Ranking metrics over file-level relevance judgments.
//!
//! All functions take the ranked document ids (best first, no duplicates)
//! and the query's judgments. A document is *relevant* when its grade is at
//! least 1. Cut-offs `k` count positions from 1.

use crate::queries::Judgment;

/// Metric cut-offs reported for recall.
pub const RECALL_CUTOFFS: [usize; 3] = [1, 5, 10];

/// Cut-off of MRR and nDCG, and the minimum retrieval depth of a run.
pub const RANK_CUTOFF: usize = 10;

fn grade_of(judgments: &[Judgment], id: &str) -> u8 {
    judgments
        .iter()
        .find(|j| j.doc == id)
        .map_or(0, |j| j.grade)
}

/// Recall@k: the fraction of relevant documents that appear in the first
/// `k` results. Returns 0 when there are no relevant documents.
pub fn recall_at(ranked: &[String], judgments: &[Judgment], k: usize) -> f64 {
    let relevant = judgments.iter().filter(|j| j.grade >= 1).count();
    if relevant == 0 {
        return 0.0;
    }
    let found = ranked
        .iter()
        .take(k)
        .filter(|id| grade_of(judgments, id) >= 1)
        .count();
    found as f64 / relevant as f64
}

/// 1-based rank of the first relevant document, if any.
pub fn first_relevant_rank(ranked: &[String], judgments: &[Judgment]) -> Option<usize> {
    ranked
        .iter()
        .position(|id| grade_of(judgments, id) >= 1)
        .map(|index| index + 1)
}

/// Reciprocal rank with cut-off: `1 / rank` of the first relevant document
/// when it is within the first `k` results, else 0. Averaged over queries
/// this is MRR@k.
pub fn reciprocal_rank_at(ranked: &[String], judgments: &[Judgment], k: usize) -> f64 {
    match first_relevant_rank(ranked, judgments) {
        Some(rank) if rank <= k => 1.0 / rank as f64,
        _ => 0.0,
    }
}

/// Gain of a grade: `2^grade - 1` (grade 3 → 7, 2 → 3, 1 → 1).
pub fn gain(grade: u8) -> f64 {
    f64::from(2u32.pow(u32::from(grade.min(16)))) - 1.0
}

/// Discount of a 1-based position: `log2(position + 1)`.
pub fn discount(position: usize) -> f64 {
    (position as f64 + 1.0).log2()
}

/// nDCG@k with graded gains `2^g - 1` and a `log2(i + 1)` discount. The
/// ideal ranking orders all judged documents by grade. Returns 0 when there
/// is no relevant document.
pub fn ndcg_at(ranked: &[String], judgments: &[Judgment], k: usize) -> f64 {
    let dcg: f64 = ranked
        .iter()
        .take(k)
        .enumerate()
        .map(|(index, id)| gain(grade_of(judgments, id)) / discount(index + 1))
        .sum();
    let mut ideal: Vec<u8> = judgments
        .iter()
        .map(|j| j.grade)
        .filter(|g| *g >= 1)
        .collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    let idcg: f64 = ideal
        .iter()
        .take(k)
        .enumerate()
        .map(|(index, grade)| gain(*grade) / discount(index + 1))
        .sum();
    if idcg > 0.0 { dcg / idcg } else { 0.0 }
}

/// Rounds to 4 decimals so reports are stable and diff-friendly.
pub fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranked(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| (*s).to_owned()).collect()
    }

    fn judged(pairs: &[(&str, u8)]) -> Vec<Judgment> {
        pairs
            .iter()
            .map(|(doc, grade)| Judgment {
                doc: (*doc).to_owned(),
                grade: *grade,
            })
            .collect()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn hand_computed_example() {
        // Ranking a, x, b; judgments a=3, b=1, c=2 (c never retrieved).
        let r = ranked(&["a", "x", "b"]);
        let j = judged(&[("a", 3), ("b", 1), ("c", 2)]);
        assert!(close(recall_at(&r, &j, 1), 1.0 / 3.0));
        assert!(close(recall_at(&r, &j, 5), 2.0 / 3.0));
        assert!(close(recall_at(&r, &j, 10), 2.0 / 3.0));
        assert!(close(reciprocal_rank_at(&r, &j, 10), 1.0));
        assert_eq!(first_relevant_rank(&r, &j), Some(1));
        // DCG = 7/log2(2) + 0/log2(3) + 1/log2(4) = 7.5
        // IDCG = 7/1 + 3/log2(3) + 1/2 = 9.392789...
        let expected = 7.5 / (7.0 + 3.0 / 3f64.log2() + 0.5);
        assert!(close(ndcg_at(&r, &j, 10), expected));
        assert!(close(round4(ndcg_at(&r, &j, 10)), 0.7985));
    }

    #[test]
    fn first_relevant_late_or_missing() {
        let j = judged(&[("z", 2)]);
        let r = ranked(&["a", "b", "c", "z"]);
        assert!(close(reciprocal_rank_at(&r, &j, 10), 0.25));
        assert!(close(reciprocal_rank_at(&r, &j, 3), 0.0));
        assert_eq!(first_relevant_rank(&r, &j), Some(4));
        assert!(close(recall_at(&r, &j, 1), 0.0));
        assert!(close(recall_at(&r, &j, 5), 1.0));
        // DCG = 3/log2(5); IDCG = 3/1.
        assert!(close(ndcg_at(&r, &j, 10), 1.0 / 5f64.log2()));
        assert!(close(ndcg_at(&r, &j, 3), 0.0));
    }

    #[test]
    fn perfect_and_empty_rankings() {
        let j = judged(&[("a", 3), ("b", 2)]);
        assert!(close(ndcg_at(&ranked(&["a", "b"]), &j, 10), 1.0));
        assert!(ndcg_at(&ranked(&["b", "a"]), &j, 10) < 1.0);
        assert!(close(ndcg_at(&[], &j, 10), 0.0));
        assert!(close(recall_at(&[], &j, 10), 0.0));
        assert!(close(reciprocal_rank_at(&[], &j, 10), 0.0));
        assert_eq!(first_relevant_rank(&[], &j), None);
        // No relevant documents: defined as 0, never NaN.
        assert!(close(recall_at(&ranked(&["a"]), &[], 10), 0.0));
        assert!(close(ndcg_at(&ranked(&["a"]), &[], 10), 0.0));
    }

    #[test]
    fn gains_discounts_and_rounding() {
        assert!(close(gain(0), 0.0));
        assert!(close(gain(1), 1.0));
        assert!(close(gain(3), 7.0));
        assert!(close(discount(1), 1.0));
        assert!(close(discount(3), 2.0));
        assert!(close(round4(0.123_449), 0.1234));
        assert!(close(round4(0.123_45), 0.1235) || close(round4(0.123_45), 0.1234));
        assert!(close(round4(2.0 / 3.0), 0.6667));
    }
}
