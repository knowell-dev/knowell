use serde::{Deserialize, Serialize};

use crate::{EdgeKind, Intent, QueryError, SourceKind};

/// Fusion weight per source. A weight of `0` means the source is not asked.
///
/// Weights multiply each source's reciprocal-rank term; only their ratios
/// matter. The defaults are starting points, **to be tuned by measurement**
/// on the evaluation set (per language and query type), not tuned values.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceWeights {
    /// Exact source weight (≥ 0).
    pub exact: f64,
    /// Lexical source weight (≥ 0).
    pub lexical: f64,
    /// Semantic source weight (≥ 0).
    pub semantic: f64,
}

impl SourceWeights {
    /// Weights from `(exact, lexical, semantic)`.
    pub const fn new(exact: f64, lexical: f64, semantic: f64) -> Self {
        Self {
            exact,
            lexical,
            semantic,
        }
    }

    /// The weight of `kind`.
    pub fn get(&self, kind: SourceKind) -> f64 {
        match kind {
            SourceKind::Exact => self.exact,
            SourceKind::Lexical => self.lexical,
            SourceKind::Semantic => self.semantic,
        }
    }

    fn validate(&self, field: &'static str) -> Result<(), QueryError> {
        let ok = |w: f64| w.is_finite() && w >= 0.0;
        if ok(self.exact) && ok(self.lexical) && ok(self.semantic) {
            Ok(())
        } else {
            Err(QueryError::InvalidConfig {
                field,
                reason: "weights must be finite and not negative",
            })
        }
    }
}

/// Source weights per intent.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WeightPresets {
    /// For [`Intent::ExactSymbol`].
    pub exact_symbol: SourceWeights,
    /// For [`Intent::PathOrFile`].
    pub path_or_file: SourceWeights,
    /// For [`Intent::Endpoint`].
    pub endpoint: SourceWeights,
    /// For [`Intent::ErrorTrace`].
    pub error_trace: SourceWeights,
    /// For [`Intent::Behavior`].
    pub behavior: SourceWeights,
    /// For [`Intent::Impact`].
    pub impact: SourceWeights,
    /// For [`Intent::Why`].
    pub why: SourceWeights,
}

impl Default for WeightPresets {
    fn default() -> Self {
        // Exact-shaped intents trust exact and lexical evidence; natural
        // language trusts semantic similarity. To be tuned by measurement.
        Self {
            exact_symbol: SourceWeights::new(1.0, 0.8, 0.3),
            path_or_file: SourceWeights::new(1.0, 0.7, 0.2),
            endpoint: SourceWeights::new(1.0, 0.8, 0.4),
            error_trace: SourceWeights::new(0.9, 1.0, 0.4),
            behavior: SourceWeights::new(0.4, 0.6, 1.0),
            impact: SourceWeights::new(1.0, 0.6, 0.4),
            why: SourceWeights::new(0.5, 0.8, 0.8),
        }
    }
}

impl WeightPresets {
    /// The weights for `intent`.
    pub fn for_intent(&self, intent: Intent) -> SourceWeights {
        match intent {
            Intent::ExactSymbol => self.exact_symbol,
            Intent::PathOrFile => self.path_or_file,
            Intent::Endpoint => self.endpoint,
            Intent::ErrorTrace => self.error_trace,
            Intent::Behavior => self.behavior,
            Intent::Impact => self.impact,
            Intent::Why => self.why,
        }
    }

    fn validate(&self) -> Result<(), QueryError> {
        Intent::ALL
            .iter()
            .try_for_each(|i| self.for_intent(*i).validate("fusion.weights"))
    }
}

/// Reciprocal rank fusion and fairness settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FusionConfig {
    /// RRF constant `k` in `weight / (k + rank)`; larger flattens rank
    /// differences. Default 60.
    pub rrf_k: u32,
    /// Source weights per intent.
    pub weights: WeightPresets,
    /// Candidates requested from each source (1-10 000). Default 100.
    pub candidate_limit: usize,
    /// Per source, at most this many candidates per project enter fusion.
    /// Default 40.
    pub candidate_quota_per_project: Option<usize>,
    /// Results returned (1-1 000). Default 20.
    pub result_limit: usize,
    /// At most this many results per project before other projects'
    /// results; work-conserving (over-quota results fill remaining slots
    /// when no other project has results left). Default 10.
    pub result_quota_per_project: Option<usize>,
}

impl Default for FusionConfig {
    fn default() -> Self {
        Self {
            rrf_k: 60,
            weights: WeightPresets::default(),
            candidate_limit: 100,
            candidate_quota_per_project: Some(40),
            result_limit: 20,
            result_quota_per_project: Some(10),
        }
    }
}

impl FusionConfig {
    /// Checks every value is in range.
    pub fn validate(&self) -> Result<(), QueryError> {
        self.weights.validate()?;
        if !(1..=10_000).contains(&self.candidate_limit) {
            return Err(QueryError::InvalidConfig {
                field: "fusion.candidate_limit",
                reason: "must be between 1 and 10000",
            });
        }
        if !(1..=1_000).contains(&self.result_limit) {
            return Err(QueryError::InvalidConfig {
                field: "fusion.result_limit",
                reason: "must be between 1 and 1000",
            });
        }
        if self.candidate_quota_per_project == Some(0) {
            return Err(QueryError::InvalidConfig {
                field: "fusion.candidate_quota_per_project",
                reason: "must be at least 1 (or unset)",
            });
        }
        if self.result_quota_per_project == Some(0) {
            return Err(QueryError::InvalidConfig {
                field: "fusion.result_quota_per_project",
                reason: "must be at least 1 (or unset)",
            });
        }
        Ok(())
    }
}

/// Graph edge kinds to follow per intent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EdgePresets {
    /// For [`Intent::ExactSymbol`].
    pub exact_symbol: Vec<EdgeKind>,
    /// For [`Intent::PathOrFile`].
    pub path_or_file: Vec<EdgeKind>,
    /// For [`Intent::Endpoint`].
    pub endpoint: Vec<EdgeKind>,
    /// For [`Intent::ErrorTrace`].
    pub error_trace: Vec<EdgeKind>,
    /// For [`Intent::Behavior`].
    pub behavior: Vec<EdgeKind>,
    /// For [`Intent::Impact`].
    pub impact: Vec<EdgeKind>,
    /// For [`Intent::Why`].
    pub why: Vec<EdgeKind>,
}

impl Default for EdgePresets {
    fn default() -> Self {
        use EdgeKind::{Callee, Caller, Contract, Doc, Test, Type};
        Self {
            exact_symbol: vec![Caller, Callee, Type, Test],
            path_or_file: vec![Test, Doc],
            endpoint: vec![Contract, Caller, Test],
            error_trace: vec![Caller, Test],
            behavior: vec![Callee, Test, Doc],
            impact: vec![Caller, Contract, Test],
            why: vec![Doc],
        }
    }
}

impl EdgePresets {
    /// The edge kinds for `intent`.
    pub fn for_intent(&self, intent: Intent) -> &[EdgeKind] {
        match intent {
            Intent::ExactSymbol => &self.exact_symbol,
            Intent::PathOrFile => &self.path_or_file,
            Intent::Endpoint => &self.endpoint,
            Intent::ErrorTrace => &self.error_trace,
            Intent::Behavior => &self.behavior,
            Intent::Impact => &self.impact,
            Intent::Why => &self.why,
        }
    }
}

/// Bounds for graph expansion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExpansionConfig {
    /// Expand at all. Default on.
    pub enabled: bool,
    /// How many top results seed the expansion. Default 5.
    pub seeds: usize,
    /// Maximum hops from a seed (1-5). Default 1; impact queries usually
    /// want 2.
    pub max_depth: u32,
    /// Maximum expanded items in total (≤ 1 000). Default 20.
    pub node_budget: usize,
    /// Maximum neighbours requested per node (1-1 000). Default 10.
    pub fanout: usize,
    /// Score of an expanded item: seed score × `score_decay` ^ depth, in (0, 1]. Default 0.5.
    pub score_decay: f64,
    /// Edge kinds to follow per intent.
    pub edges: EdgePresets,
}

impl Default for ExpansionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            seeds: 5,
            max_depth: 1,
            node_budget: 20,
            fanout: 10,
            score_decay: 0.5,
            edges: EdgePresets::default(),
        }
    }
}

impl ExpansionConfig {
    /// Checks every value is in range.
    pub fn validate(&self) -> Result<(), QueryError> {
        if !(1..=5).contains(&self.max_depth) {
            return Err(QueryError::InvalidConfig {
                field: "expansion.max_depth",
                reason: "must be between 1 and 5",
            });
        }
        if self.node_budget > 1_000 {
            return Err(QueryError::InvalidConfig {
                field: "expansion.node_budget",
                reason: "must be at most 1000",
            });
        }
        if !(1..=1_000).contains(&self.fanout) {
            return Err(QueryError::InvalidConfig {
                field: "expansion.fanout",
                reason: "must be between 1 and 1000",
            });
        }
        if !(self.score_decay > 0.0 && self.score_decay <= 1.0) {
            return Err(QueryError::InvalidConfig {
                field: "expansion.score_decay",
                reason: "must be greater than 0 and at most 1",
            });
        }
        Ok(())
    }
}

/// Optional reranking of the short list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RerankConfig {
    /// Rerank at all. **Off by default**; enable only where measurement shows
    /// it helps.
    pub enabled: bool,
    /// How many top results are reranked (1-200). Default 20.
    pub top_n: usize,
}

impl Default for RerankConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            top_n: 20,
        }
    }
}

/// Every query-engine knob.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    /// Fusion, weights and quotas.
    pub fusion: FusionConfig,
    /// Graph expansion bounds.
    pub expansion: ExpansionConfig,
    /// Optional reranking.
    pub rerank: RerankConfig,
}

impl SearchConfig {
    /// Checks every value is in range.
    pub fn validate(&self) -> Result<(), QueryError> {
        self.fusion.validate()?;
        self.expansion.validate()?;
        if !(1..=200).contains(&self.rerank.top_n) {
            return Err(QueryError::InvalidConfig {
                field: "rerank.top_n",
                reason: "must be between 1 and 200",
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        SearchConfig::default().validate().unwrap();
    }

    #[test]
    fn rejects_out_of_range() {
        let mut c = SearchConfig::default();
        c.fusion.weights.behavior.semantic = f64::NAN;
        assert!(c.validate().is_err());
        let mut c = SearchConfig::default();
        c.fusion.weights.impact.exact = -1.0;
        assert!(c.validate().is_err());
        let mut c = SearchConfig::default();
        c.expansion.max_depth = 6;
        assert!(c.validate().is_err());
        let mut c = SearchConfig::default();
        c.fusion.result_quota_per_project = Some(0);
        assert!(c.validate().is_err());
        let mut c = SearchConfig::default();
        c.expansion.score_decay = 0.0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn config_deserialises_partially() {
        let c: SearchConfig =
            serde_json::from_str(r#"{"fusion":{"rrf_k":10},"rerank":{"enabled":true}}"#).unwrap();
        assert_eq!(c.fusion.rrf_k, 10);
        assert_eq!(c.fusion.result_limit, 20);
        assert!(c.rerank.enabled);
        assert_eq!(c.rerank.top_n, 20);
    }
}
