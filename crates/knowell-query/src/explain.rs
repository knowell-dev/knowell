use std::fmt;
use std::str::FromStr;

use knowell_core::Name;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    CommitId, ExactTarget, ExpansionStats, GraphStep, Language, Layer, Location, SourceKind,
    TermStatus, ViewId,
};

/// One reason a result is in the answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reason {
    /// An exact term matched a symbol, path, contract, error code or text.
    ExactMatch {
        /// The planned term.
        term: String,
        /// What it matched.
        target: ExactTarget,
    },
    /// BM25 found these query terms.
    LexicalTerms {
        /// The matched terms.
        terms: Vec<String>,
    },
    /// A matched lexical term came from a glossary expansion.
    GlossaryExpansion {
        /// Query words that triggered the expansion.
        matched: String,
        /// The expansion that matched.
        expansion: String,
        /// Approval state of the glossary link.
        status: TermStatus,
    },
    /// Vector similarity under an embedding profile.
    SemanticSimilarity {
        /// Cosine similarity reported by the source.
        similarity: f64,
        /// Embedding profile.
        profile: String,
    },
    /// Reached from another result over evidenced graph edges.
    GraphPath {
        /// The result the path starts at.
        seed: Location,
        /// Edges walked, in order.
        steps: Vec<GraphStep>,
    },
    /// This is a test that references the subject.
    TestReferences {
        /// Symbol (or location label) of what the test references.
        subject: String,
    },
    /// Comes from the user's personal worktree layer, which shadows the base view.
    PersonalOverlay {
        /// The overlay view.
        view: ViewId,
    },
}

/// One source's contribution to a fused score.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceScore {
    /// The source.
    pub source: SourceKind,
    /// Best rank this result (or a duplicate merged into it) had in that source.
    pub rank: u32,
    /// The source-native score at that rank.
    pub raw_score: f64,
    /// Fusion weight of the source for the plan's intent.
    pub weight: f64,
    /// `weight / (k + rank)`.
    pub contribution: f64,
}

/// Score of an optional reranker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RerankScore {
    /// Reranker id (name and pinned version).
    pub reranker: String,
    /// Its score; higher is better. Only comparable within one query.
    pub score: f64,
}

/// How a result's score was computed: per source, fused, reranked.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    /// The RRF constant `k` used.
    pub rrf_k: u32,
    /// Contributions, in [`SourceKind`] order.
    pub sources: Vec<SourceScore>,
    /// Sum of the contributions: weighted reciprocal rank fusion.
    pub fused: f64,
    /// Reranker score, when the result was in the reranked short list.
    pub rerank: Option<RerankScore>,
}

/// A part of the pipeline that can be missing or failing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Component {
    /// Exact source.
    Exact,
    /// Lexical source.
    Lexical,
    /// Semantic source (embedding provider + vector index).
    Semantic,
    /// Graph expander.
    Graph,
    /// Reranker.
    Rerank,
    /// External intent classifier.
    Classifier,
}

impl Component {
    /// Stable lowercase label.
    pub fn label(self) -> &'static str {
        match self {
            Component::Exact => "exact",
            Component::Lexical => "lexical",
            Component::Semantic => "semantic",
            Component::Graph => "graph",
            Component::Rerank => "rerank",
            Component::Classifier => "classifier",
        }
    }

    pub(crate) fn of(kind: SourceKind) -> Self {
        match kind {
            SourceKind::Exact => Component::Exact,
            SourceKind::Lexical => Component::Lexical,
            SourceKind::Semantic => Component::Semantic,
        }
    }
}

impl FromStr for Component {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "exact" => Component::Exact,
            "lexical" => Component::Lexical,
            "semantic" => Component::Semantic,
            "graph" => Component::Graph,
            "rerank" => Component::Rerank,
            "classifier" => Component::Classifier,
            _ => {
                return Err(format!(
                    "unknown pipeline component `{}`",
                    crate::error::echo(s)
                ));
            }
        })
    }
}

/// A missing or failing part of the pipeline. The answer is still given from
/// the parts that work; this says what it could not use.
///
/// Serialised as one string, `"<component>: <reason>"`, e.g.
/// `"semantic: provider not configured"`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Degradation {
    /// What is missing or failing.
    pub component: Component,
    /// Why. Never contains secret values.
    pub reason: String,
}

impl Degradation {
    /// Creates a degradation entry.
    pub fn new(component: Component, reason: impl Into<String>) -> Self {
        Self {
            component,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for Degradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.component.label(), self.reason)
    }
}

impl Serialize for Degradation {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Degradation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let (component, reason) = text
            .split_once(": ")
            .ok_or_else(|| serde::de::Error::custom("expected `<component>: <reason>`"))?;
        let component = component.parse().map_err(serde::de::Error::custom)?;
        Ok(Self::new(component, reason))
    }
}

/// One view the query searched.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SearchedView {
    /// Project.
    pub project: Name,
    /// View.
    pub view: ViewId,
    /// Pinned generation.
    pub generation: u64,
    /// Pinned commit, if any.
    pub commit: Option<CommitId>,
    /// Base view or personal overlay.
    pub layer: Layer,
}

/// What the query actually covered.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchedCoverage {
    /// Pinned views inside the scope.
    pub views: Vec<SearchedView>,
    /// Sources that were asked.
    pub consulted: Vec<SourceKind>,
    /// Sources that answered (possibly with nothing).
    pub answered: Vec<SourceKind>,
}

/// Analysis the searched views lack for this kind of question.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CoverageGap {
    /// References (callers, callees, implementations) are not resolved for
    /// this language in this project; relations there rest on syntax or
    /// heuristics, so missing callers do not mean there are none.
    NoReferenceResolution {
        /// Project.
        project: Name,
        /// Language.
        language: Language,
    },
}

/// Why a search returned no results. Several reasons can hold at once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EmptyReason {
    /// The query had nothing to search for.
    EmptyQuery,
    /// The scope names a project the workspace does not have.
    UnknownProject {
        /// The project.
        project: Name,
    },
    /// The project exists but has no active index yet.
    ProjectNotIndexed {
        /// The project.
        project: Name,
    },
    /// No pinned project is inside the scope.
    NoProjectsInScope,
    /// Some sources could not be asked or failed.
    SourcesUnavailable {
        /// Which, and why.
        sources: Vec<Degradation>,
    },
    /// The question needs references that this language lacks here.
    NoReferenceResolutionForLanguage {
        /// Project.
        project: Name,
        /// Language.
        language: Language,
    },
    /// The sources that answered found nothing in the pinned views.
    NoCandidatesInSelectedRef {
        /// The views searched.
        views: Vec<SearchedView>,
    },
    /// Candidates existed and some were outside the scope's filters. The
    /// `All…` and `MalformedCandidates` reasons together account for every
    /// candidate received.
    AllFilteredByScope {
        /// Dropped by the project filter.
        project_filter: usize,
        /// Dropped by the language filter.
        language_filter: usize,
        /// Dropped by the path filter.
        path_filter: usize,
    },
    /// Candidates existed and some belonged to views or generations that were
    /// not pinned (for example an index generation that activated mid-query).
    AllOutsidePinnedViews {
        /// Dropped because their project is not pinned.
        unpinned_project: usize,
        /// Dropped because their view or generation is not the pinned one.
        view_mismatch: usize,
    },
    /// Candidates existed and some were base-view evidence for paths the
    /// user's worktree changed.
    AllShadowedByOverlay {
        /// How many.
        shadowed: usize,
    },
    /// Some candidates were malformed (wrong source kind, rank 0 or a
    /// non-finite score) and were dropped.
    MalformedCandidates {
        /// How many.
        count: usize,
    },
}

/// The engine never claims that something does not exist; it reports what it
/// searched and why that found nothing.
pub const ABSENCE_NOTE: &str = "no evidence was found within the searched coverage; \
     this does not show that the behaviour does not exist";

/// Explanation of an empty result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmptyExplanation {
    /// Every reason that applies, most fundamental first.
    pub reasons: Vec<EmptyReason>,
    /// Always [`ABSENCE_NOTE`].
    pub note: String,
}

/// Candidates received per source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceCounts {
    /// From the exact source.
    pub exact: usize,
    /// From the lexical source.
    pub lexical: usize,
    /// From the semantic source.
    pub semantic: usize,
}

impl SourceCounts {
    /// Total over all sources.
    pub fn total(&self) -> usize {
        self.exact
            .saturating_add(self.lexical)
            .saturating_add(self.semantic)
    }

    pub(crate) fn add(&mut self, kind: SourceKind, n: usize) {
        let slot = match kind {
            SourceKind::Exact => &mut self.exact,
            SourceKind::Lexical => &mut self.lexical,
            SourceKind::Semantic => &mut self.semantic,
        };
        *slot = slot.saturating_add(n);
    }
}

/// Candidates dropped before fusion, by reason.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropCounts {
    /// Wrong source kind, rank 0 or non-finite score.
    pub malformed: usize,
    /// Project not in the manifest.
    pub unpinned_project: usize,
    /// View or generation not the pinned one.
    pub view_mismatch: usize,
    /// Excluded by the project filter.
    pub project_filter: usize,
    /// Excluded by the language filter.
    pub language_filter: usize,
    /// Excluded by the path filter.
    pub path_filter: usize,
    /// Base-view evidence shadowed by the personal overlay.
    pub shadowed: usize,
    /// Over the per-source, per-project candidate quota.
    pub candidate_quota: usize,
}

impl DropCounts {
    /// Total dropped.
    pub fn total(&self) -> usize {
        [
            self.malformed,
            self.unpinned_project,
            self.view_mismatch,
            self.project_filter,
            self.language_filter,
            self.path_filter,
            self.shadowed,
            self.candidate_quota,
        ]
        .iter()
        .fold(0usize, |acc, n| acc.saturating_add(*n))
    }
}

/// Counters for one search, for the panel and for debugging rankings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchStats {
    /// Candidates received per source.
    pub received: SourceCounts,
    /// Candidates dropped before fusion.
    pub dropped: DropCounts,
    /// Overlapping duplicates merged into a better result.
    pub merged_duplicates: usize,
    /// Distinct results after fusion and de-duplication.
    pub fused: usize,
    /// Results moved behind other projects' results by the result quota.
    pub deferred_by_quota: usize,
    /// Results cut by the result limit.
    pub truncated_by_limit: usize,
    /// Graph expansion counters.
    pub expansion: ExpansionStats,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn degradation_serialises_as_text() {
        let d = Degradation::new(Component::Semantic, "provider not configured");
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, "\"semantic: provider not configured\"");
        assert_eq!(serde_json::from_str::<Degradation>(&json).unwrap(), d);
        assert!(serde_json::from_str::<Degradation>("\"nope\"").is_err());
        assert!(serde_json::from_str::<Degradation>("\"warp: x\"").is_err());
    }
}
