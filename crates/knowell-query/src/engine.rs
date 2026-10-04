use serde::{Deserialize, Serialize};

use crate::fusion;
use crate::{
    ABSENCE_NOTE, Candidate, CommitId, Component, CoverageGap, Degradation, EdgeKind,
    EmptyExplanation, EmptyReason, ExactSource, ExpandedItem, GraphExpander, Intent, Language,
    Layer, LexicalSource, Location, QueryError, QueryPlan, QueryScope, Reason, Reranker,
    ScoreBreakdown, SearchConfig, SearchStats, SearchedCoverage, SearchedView, SourceError,
    SourceKind, SourceLists, SourceRequest, SourceStatus, VectorSource, expand, rerank,
};

/// The pluggable parts of the pipeline. Any of them may be missing; the
/// response says which were, and the engine answers from the rest.
#[derive(Clone, Copy, Default)]
pub struct Sources<'a> {
    /// Exact lookups (symbols, paths, contracts, error codes).
    pub exact: Option<&'a dyn ExactSource>,
    /// Lexical BM25 search.
    pub lexical: Option<&'a dyn LexicalSource>,
    /// Semantic vector search.
    pub semantic: Option<&'a dyn VectorSource>,
    /// Graph expansion.
    pub graph: Option<&'a dyn GraphExpander>,
    /// Optional short-list reranker (used only when enabled in the config).
    pub reranker: Option<&'a dyn Reranker>,
}

/// One ranked, explained result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    /// 1-based rank in the response.
    pub rank: u32,
    /// Stable id derived from the location (`project@view#generation:path:range@hash`).
    pub id: String,
    /// Where the evidence is.
    pub location: Location,
    /// Pinned commit of the view, if any.
    pub commit: Option<CommitId>,
    /// Base view or personal overlay.
    pub layer: Layer,
    /// Symbol, if known.
    pub symbol: Option<String>,
    /// Language, if known.
    pub language: Option<Language>,
    /// Score per source, fused score and optional rerank score.
    pub score: ScoreBreakdown,
    /// Why it is in the answer.
    pub why: Vec<Reason>,
    /// Overlapping evidence merged within this same pinned file occurrence.
    /// Other paths, projects and pins remain separate results.
    pub also_at: Vec<Location>,
}

/// The answer to a search: results, graph neighbours, what was searched and
/// what was missing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchResponse {
    /// The plan the search ran.
    pub plan: QueryPlan,
    /// Ranked results.
    pub results: Vec<SearchResult>,
    /// Items reached by graph expansion from the top results.
    pub expanded: Vec<ExpandedItem>,
    /// Missing or failing parts of the pipeline, e.g. `semantic: provider not configured`.
    pub degraded: Vec<Degradation>,
    /// Views and sources actually covered.
    pub searched: SearchedCoverage,
    /// Analysis the searched views lack for this kind of question.
    pub coverage_gaps: Vec<CoverageGap>,
    /// Why there are no results, when there are none.
    pub empty: Option<EmptyExplanation>,
    /// Pipeline counters.
    pub stats: SearchStats,
}

/// Runs the whole pipeline with synchronous sources: consult candidate
/// sources, fuse, rerank (if enabled), expand over the graph, explain.
///
/// Errors only for an invalid scope or configuration; source failures are
/// reported in [`SearchResponse::degraded`].
pub fn search(
    plan: &QueryPlan,
    scope: &QueryScope,
    sources: &Sources<'_>,
    config: &SearchConfig,
) -> Result<SearchResponse, QueryError> {
    validate(plan, scope, config)?;
    let lists = collect_candidates(plan, scope, sources, config);
    Ok(finish(
        plan,
        scope,
        lists,
        sources.graph,
        sources.reranker,
        config,
    ))
}

/// Asks each configured candidate source whose weight for the plan's intent
/// is above zero. Nothing is asked for an empty query or an empty scope.
pub fn collect_candidates(
    plan: &QueryPlan,
    scope: &QueryScope,
    sources: &Sources<'_>,
    config: &SearchConfig,
) -> SourceLists {
    if plan.is_empty() || scope.searched_projects().next().is_none() {
        return SourceLists::not_consulted();
    }
    let weights = config.fusion.weights.for_intent(plan.intent);
    let request = SourceRequest {
        plan,
        scope,
        limit: config.fusion.candidate_limit,
        per_project_limit: config.fusion.candidate_quota_per_project,
    };
    SourceLists {
        exact: consult(
            weights.exact,
            sources.exact.map(|s| move || s.search_exact(&request)),
        ),
        lexical: consult(
            weights.lexical,
            sources.lexical.map(|s| move || s.search_lexical(&request)),
        ),
        semantic: consult(
            weights.semantic,
            sources
                .semantic
                .map(|s| move || s.search_semantic(&request)),
        ),
    }
}

fn consult(
    weight: f64,
    call: Option<impl FnOnce() -> Result<Vec<Candidate>, SourceError>>,
) -> SourceStatus {
    if weight <= 0.0 {
        return SourceStatus::NotConsulted;
    }
    match call {
        None => SourceStatus::NotConfigured,
        Some(call) => match call() {
            Ok(candidates) => SourceStatus::Answered(candidates),
            Err(error) => SourceStatus::Failed(error),
        },
    }
}

/// Runs the pipeline after candidate retrieval: for integrations that fetch
/// candidates themselves (for example asynchronously) and then hand the
/// lists over. Graph expansion and reranking use the given traits.
pub fn search_with_candidates(
    plan: &QueryPlan,
    scope: &QueryScope,
    lists: SourceLists,
    graph: Option<&dyn GraphExpander>,
    reranker: Option<&dyn Reranker>,
    config: &SearchConfig,
) -> Result<SearchResponse, QueryError> {
    validate(plan, scope, config)?;
    Ok(finish(plan, scope, lists, graph, reranker, config))
}

fn validate(plan: &QueryPlan, scope: &QueryScope, config: &SearchConfig) -> Result<(), QueryError> {
    config.validate()?;
    scope.validate()?;
    check_domain(plan, scope)
}

/// Fusion, rerank, expansion and explanation over validated inputs.
fn finish(
    plan: &QueryPlan,
    scope: &QueryScope,
    lists: SourceLists,
    graph: Option<&dyn GraphExpander>,
    reranker: Option<&dyn Reranker>,
    config: &SearchConfig,
) -> SearchResponse {
    let fused = fusion::fuse(plan, scope, &lists, &config.fusion);
    let mut degraded = plan.degraded.clone();
    degraded.extend(fused.degraded);
    let mut stats = fused.stats;
    let mut results = fused.results;

    if config.rerank.enabled && !results.is_empty() {
        match reranker {
            None => degraded.push(Degradation::new(
                Component::Rerank,
                "reranker not configured",
            )),
            Some(reranker) => {
                if let Some(problem) =
                    rerank::rerank(&mut results, plan, reranker, config.rerank.top_n)
                {
                    degraded.push(problem);
                }
            }
        }
    }
    results = fusion::finalize_results(results, &config.fusion, &mut stats);

    let edges = config.expansion.edges.for_intent(plan.intent);
    let mut expanded = Vec::new();
    let expansion_wanted = config.expansion.enabled
        && !edges.is_empty()
        && config.expansion.seeds > 0
        && config.expansion.node_budget > 0
        && !results.is_empty();
    if expansion_wanted {
        match graph {
            None => degraded.push(Degradation::new(
                Component::Graph,
                "expander not configured",
            )),
            Some(graph) => {
                let outcome = expand::expand(&mut results, scope, graph, edges, &config.expansion);
                expanded = outcome.items;
                stats.expansion = outcome.stats;
                degraded.extend(outcome.degraded);
            }
        }
    }

    let searched = SearchedCoverage {
        views: searched_views(scope),
        consulted: fused.consulted,
        answered: fused.answered,
    };
    let followed: &[EdgeKind] = if expansion_wanted { edges } else { &[] };
    let coverage_gaps = coverage_gaps(plan.intent, followed, scope);
    let empty = results
        .is_empty()
        .then(|| explain_empty(plan, scope, &stats, &degraded, &searched, &coverage_gaps));

    SearchResponse {
        plan: plan.clone(),
        results,
        expanded,
        degraded,
        searched,
        coverage_gaps,
        empty,
        stats,
    }
}

/// A plan expanded with one domain's glossary must not search another domain.
fn check_domain(plan: &QueryPlan, scope: &QueryScope) -> Result<(), QueryError> {
    match (&plan.domain, &scope.domain) {
        (Some(plan_domain), Some(scope_domain)) if plan_domain != scope_domain => {
            Err(QueryError::DomainMismatch {
                plan: plan_domain.clone(),
                scope: scope_domain.clone(),
            })
        }
        _ => Ok(()),
    }
}

fn searched_views(scope: &QueryScope) -> Vec<SearchedView> {
    let mut views = Vec::new();
    for (project, pin) in scope.searched_projects() {
        views.push(SearchedView {
            project: project.clone(),
            view: pin.base.view.clone(),
            generation: pin.base.generation,
            commit: pin.base.commit.clone(),
            layer: Layer::Base,
        });
        if let Some(overlay) = &pin.overlay {
            views.push(SearchedView {
                project: project.clone(),
                view: overlay.pin.view.clone(),
                generation: overlay.pin.generation,
                commit: overlay.pin.commit.clone(),
                layer: Layer::Overlay,
            });
        }
    }
    views
}

/// Reference resolution matters when the question is about relations
/// (impact) or when expansion follows call or test edges. Empty relation
/// lists do not establish that no caller, callee or exercising test exists.
fn needs_references(intent: Intent, edges: &[EdgeKind]) -> bool {
    intent == Intent::Impact
        || edges
            .iter()
            .any(|e| matches!(e, EdgeKind::Caller | EdgeKind::Callee | EdgeKind::Test))
}

fn has_callable_symbols(language: &Language) -> bool {
    // A project's document and configuration languages do not establish a
    // missing call graph. Keep the warning for known executable languages,
    // including component languages with embedded code and public aliases.
    matches!(
        language.as_str(),
        "rust"
            | "typescript"
            | "tsx"
            | "javascript"
            | "jsx"
            | "python"
            | "go"
            | "java"
            | "kotlin"
            | "csharp"
            | "c#"
            | "dart"
            | "swift"
            | "php"
            | "ruby"
            | "c"
            | "cpp"
            | "c++"
            | "scala"
            | "bash"
            | "lua"
            | "perl"
            | "r"
            | "elixir"
            | "erlang"
            | "haskell"
            | "ocaml"
            | "clojure"
            | "zig"
            | "objc"
            | "powershell"
            | "batch"
            | "groovy"
            | "vue"
            | "svelte"
    )
}

fn coverage_gaps(intent: Intent, edges: &[EdgeKind], scope: &QueryScope) -> Vec<CoverageGap> {
    if !needs_references(intent, edges) {
        return Vec::new();
    }
    let mut gaps = Vec::new();
    for (project, pin) in scope.searched_projects() {
        for language in &pin.coverage.languages {
            let selected = scope
                .languages
                .as_ref()
                .is_none_or(|wanted| wanted.contains(language));
            if selected
                && has_callable_symbols(language)
                && !pin.coverage.reference_resolution.contains(language)
            {
                gaps.push(CoverageGap::NoReferenceResolution {
                    project: project.clone(),
                    language: language.clone(),
                });
            }
        }
    }
    gaps
}

fn explain_empty(
    plan: &QueryPlan,
    scope: &QueryScope,
    stats: &SearchStats,
    degraded: &[Degradation],
    searched: &SearchedCoverage,
    gaps: &[CoverageGap],
) -> EmptyExplanation {
    let mut reasons = Vec::new();
    if plan.is_empty() {
        reasons.push(EmptyReason::EmptyQuery);
        return EmptyExplanation {
            reasons,
            note: ABSENCE_NOTE.to_owned(),
        };
    }

    let manifest = &scope.manifest;
    match &scope.projects {
        Some(selected) => {
            for project in selected {
                if manifest.projects.contains_key(project) {
                    continue;
                }
                reasons.push(if manifest.not_indexed.contains(project) {
                    EmptyReason::ProjectNotIndexed {
                        project: project.clone(),
                    }
                } else {
                    EmptyReason::UnknownProject {
                        project: project.clone(),
                    }
                });
            }
        }
        None => reasons.extend(manifest.not_indexed.iter().map(|project| {
            EmptyReason::ProjectNotIndexed {
                project: project.clone(),
            }
        })),
    }
    if searched.views.is_empty() {
        reasons.push(EmptyReason::NoProjectsInScope);
    }

    let unavailable: Vec<Degradation> = degraded
        .iter()
        .filter(|d| {
            SourceKind::ALL
                .iter()
                .any(|kind| Component::of(*kind) == d.component)
        })
        .cloned()
        .collect();
    if !unavailable.is_empty() {
        reasons.push(EmptyReason::SourcesUnavailable {
            sources: unavailable,
        });
    }

    if plan.intent == Intent::Impact {
        for gap in gaps {
            let CoverageGap::NoReferenceResolution { project, language } = gap;
            reasons.push(EmptyReason::NoReferenceResolutionForLanguage {
                project: project.clone(),
                language: language.clone(),
            });
        }
    }

    let received = stats.received.total();
    let dropped = stats.dropped;
    if received == 0 {
        if !searched.answered.is_empty() && !searched.views.is_empty() {
            reasons.push(EmptyReason::NoCandidatesInSelectedRef {
                views: searched.views.clone(),
            });
        }
    } else {
        if dropped.malformed > 0 {
            reasons.push(EmptyReason::MalformedCandidates {
                count: dropped.malformed,
            });
        }
        if dropped.unpinned_project > 0 || dropped.view_mismatch > 0 {
            reasons.push(EmptyReason::AllOutsidePinnedViews {
                unpinned_project: dropped.unpinned_project,
                view_mismatch: dropped.view_mismatch,
            });
        }
        if dropped.project_filter > 0 || dropped.language_filter > 0 || dropped.path_filter > 0 {
            reasons.push(EmptyReason::AllFilteredByScope {
                project_filter: dropped.project_filter,
                language_filter: dropped.language_filter,
                path_filter: dropped.path_filter,
            });
        }
        if dropped.shadowed > 0 {
            reasons.push(EmptyReason::AllShadowedByOverlay {
                shadowed: dropped.shadowed,
            });
        }
    }
    EmptyExplanation {
        reasons,
        note: ABSENCE_NOTE.to_owned(),
    }
}
