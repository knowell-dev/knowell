use crate::{Component, Degradation, Location, QueryPlan, RerankScore, SearchResult, SourceError};

/// One short-list item handed to a reranker.
#[derive(Clone, Copy, Debug)]
pub struct RerankItem<'a> {
    /// Where the item is; the reranker fetches the text it needs.
    pub location: &'a Location,
    /// Its symbol, if any.
    pub symbol: Option<&'a str>,
}

/// Optional reranker for the short list (a local model or a hosted rerank
/// API). Off by default; see [`RerankConfig`](crate::RerankConfig). Content
/// sent to a hosted reranker is subject to the project's data-egress policy,
/// which the adapter enforces.
pub trait Reranker {
    /// Reranker id: name and pinned version.
    fn id(&self) -> String;

    /// One finite score per item, in item order; higher is better.
    fn rerank(&self, plan: &QueryPlan, items: &[RerankItem<'_>]) -> Result<Vec<f64>, SourceError>;
}

/// Reorders the top `top_n` results by reranker score (stable: ties keep the
/// fused order) and renumbers ranks. On any failure the order is kept and the
/// failure is returned as a degradation.
pub(crate) fn rerank(
    results: &mut Vec<SearchResult>,
    plan: &QueryPlan,
    reranker: &dyn Reranker,
    top_n: usize,
) -> Option<Degradation> {
    let n = results.len().min(top_n);
    if n == 0 {
        return None;
    }
    let items: Vec<RerankItem<'_>> = results
        .iter()
        .take(n)
        .map(|r| RerankItem {
            location: &r.location,
            symbol: r.symbol.as_deref(),
        })
        .collect();
    let scores = match reranker.rerank(plan, &items) {
        Ok(scores) => scores,
        Err(error) => return Some(Degradation::new(Component::Rerank, error.to_string())),
    };
    if scores.len() != n || scores.iter().any(|s| !s.is_finite()) {
        return Some(Degradation::new(
            Component::Rerank,
            format!(
                "reranker returned {} scores for {n} items or a non-finite score; fused order kept",
                scores.len()
            ),
        ));
    }
    let id = reranker.id();
    let mut head: Vec<SearchResult> = results.drain(..n).collect();
    for (result, score) in head.iter_mut().zip(scores) {
        result.score.rerank = Some(RerankScore {
            reranker: id.clone(),
            score,
        });
    }
    let key = |r: &SearchResult| r.score.rerank.as_ref().map_or(f64::MIN, |s| s.score);
    head.sort_by(|a, b| key(b).total_cmp(&key(a)));
    results.splice(0..0, head);
    for (rank, result) in (1u32..).zip(results.iter_mut()) {
        result.rank = rank;
    }
    None
}
