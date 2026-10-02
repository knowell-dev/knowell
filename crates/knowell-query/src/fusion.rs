//! Weighted reciprocal rank fusion with per-project quotas, overlay
//! shadowing and overlap de-duplication.

use std::cmp::Ordering;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use knowell_core::{ContentHash, Name, RepoPath};

use crate::scope::Rejection;
use crate::text::fold;
use crate::{
    Candidate, Component, Degradation, DropCounts, FusionConfig, Language, Layer, Location,
    MatchDetail, QueryPlan, QueryScope, Reason, ScoreBreakdown, SearchResult, SearchStats,
    SourceKind, SourceLists, SourceScore, SourceStatus, SourceWeights,
};

/// What fusion produced, before reranking and expansion.
pub(crate) struct Fused {
    pub(crate) results: Vec<SearchResult>,
    pub(crate) degraded: Vec<Degradation>,
    pub(crate) consulted: Vec<SourceKind>,
    pub(crate) answered: Vec<SourceKind>,
    pub(crate) stats: SearchStats,
}

/// The best hit one source contributed to a group.
#[derive(Clone, Debug)]
struct Hit {
    rank: u32,
    raw_score: f64,
    detail: MatchDetail,
}

/// Candidates for one location, from any number of sources.
#[derive(Clone, Debug)]
struct Group {
    location: Location,
    layer: Layer,
    symbol: Option<String>,
    language: Option<Language>,
    hits: BTreeMap<SourceKind, Hit>,
    also_at: Vec<Location>,
}

impl Group {
    fn best_rank(&self) -> u32 {
        self.hits.values().map(|h| h.rank).min().unwrap_or(u32::MAX)
    }
}

/// `weight / (k + rank)`; the denominator is at least 1 because ranks start at 1.
pub(crate) fn rrf(weight: f64, k: u32, rank: u32) -> f64 {
    weight / (f64::from(k) + f64::from(rank))
}

fn fused_score(group: &Group, weights: &SourceWeights, k: u32) -> f64 {
    group
        .hits
        .iter()
        .map(|(kind, hit)| rrf(weights.get(*kind), k, hit.rank))
        .sum()
}

/// Ranking order: fused score, then agreement (number of sources), then best
/// single rank, then location — so equal scores never depend on input order.
fn sort_groups(groups: Vec<Group>, weights: &SourceWeights, k: u32) -> Vec<Group> {
    let mut scored: Vec<(f64, Group)> = groups
        .into_iter()
        .map(|g| (fused_score(&g, weights, k), g))
        .collect();
    scored.sort_by(|(sa, a), (sb, b)| {
        sb.total_cmp(sa)
            .then_with(|| b.hits.len().cmp(&a.hits.len()))
            .then_with(|| a.best_rank().cmp(&b.best_rank()))
            .then_with(|| a.location.cmp(&b.location))
    });
    scored.into_iter().map(|(_, g)| g).collect()
}

fn count_rejection(rejection: Rejection, dropped: &mut DropCounts) {
    let slot = match rejection {
        Rejection::UnpinnedProject => &mut dropped.unpinned_project,
        Rejection::ViewMismatch => &mut dropped.view_mismatch,
        Rejection::ProjectFilter => &mut dropped.project_filter,
        Rejection::LanguageFilter => &mut dropped.language_filter,
        Rejection::PathFilter => &mut dropped.path_filter,
    };
    *slot = slot.saturating_add(1);
}

pub(crate) fn not_configured_reason(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Semantic => "provider not configured",
        SourceKind::Exact | SourceKind::Lexical => "source not configured",
    }
}

fn candidate_order(a: &Candidate, b: &Candidate) -> Ordering {
    a.source_rank
        .cmp(&b.source_rank)
        .then_with(|| b.raw_score.total_cmp(&a.raw_score))
        .then_with(|| a.location().cmp(&b.location()))
        .then_with(|| a.id.cmp(&b.id))
}

/// Fuses the source lists. Steps, in order:
///
/// 1. drop malformed candidates and candidates outside the scope / pinned views;
/// 2. drop base-view candidates shadowed by the personal overlay;
/// 3. keep at most `candidate_quota_per_project` per source and project;
/// 4. group identical locations and score them with weighted RRF;
/// 5. merge overlapping duplicates (same content, overlapping lines) into the
///    better-ranked group, keeping each source's best rank;
/// 6. order, apply the work-conserving result quota and the result limit.
pub(crate) fn fuse(
    plan: &QueryPlan,
    scope: &QueryScope,
    lists: &SourceLists,
    config: &FusionConfig,
) -> Fused {
    let weights = config.weights.for_intent(plan.intent);
    let k = config.rrf_k;
    let mut stats = SearchStats::default();
    let mut degraded = Vec::new();
    let mut consulted = Vec::new();
    let mut answered = Vec::new();

    // 1. Validation and admission, per source in rank order.
    let mut admitted: Vec<(SourceKind, &Candidate, Layer)> = Vec::new();
    for kind in SourceKind::ALL {
        match lists.get(kind) {
            SourceStatus::NotConsulted => {}
            SourceStatus::NotConfigured => degraded.push(Degradation::new(
                Component::of(kind),
                not_configured_reason(kind),
            )),
            SourceStatus::Failed(error) => {
                consulted.push(kind);
                degraded.push(Degradation::new(Component::of(kind), error.to_string()));
            }
            SourceStatus::Answered(candidates) => {
                consulted.push(kind);
                answered.push(kind);
                stats.received.add(kind, candidates.len());
                let mut valid: Vec<&Candidate> = Vec::with_capacity(candidates.len());
                for candidate in candidates {
                    if candidate.is_well_formed(kind) {
                        valid.push(candidate);
                    } else {
                        stats.dropped.malformed = stats.dropped.malformed.saturating_add(1);
                    }
                }
                valid.sort_by(|a, b| candidate_order(a, b));
                for candidate in valid {
                    match scope.admit(
                        &candidate.project,
                        &candidate.view,
                        candidate.generation,
                        &candidate.path,
                        candidate.language.as_ref(),
                    ) {
                        Ok(layer) => admitted.push((kind, candidate, layer)),
                        Err(rejection) => count_rejection(rejection, &mut stats.dropped),
                    }
                }
            }
        }
    }

    // 2. Overlay shadowing: the user's worktree replaces base-view evidence
    //    for every path it has a candidate for or declares as changed.
    let overlay_paths: BTreeSet<(&Name, &RepoPath)> = admitted
        .iter()
        .filter(|&&(_, _, layer)| layer == Layer::Overlay)
        .map(|&(_, c, _)| (&c.project, &c.path))
        .collect();
    admitted.retain(|&(_, c, layer)| {
        let shadowed = layer == Layer::Base
            && (overlay_paths.contains(&(&c.project, &c.path))
                || scope.declared_shadowed(&c.project, &c.path));
        if shadowed {
            stats.dropped.shadowed = stats.dropped.shadowed.saturating_add(1);
        }
        !shadowed
    });

    // 3. Per-source, per-project candidate quota.
    if let Some(quota) = config.candidate_quota_per_project {
        let mut taken: BTreeMap<(SourceKind, &Name), usize> = BTreeMap::new();
        admitted.retain(|&(kind, c, _)| {
            let n = taken.entry((kind, &c.project)).or_insert(0);
            *n = n.saturating_add(1);
            let keep = *n <= quota;
            if !keep {
                stats.dropped.candidate_quota = stats.dropped.candidate_quota.saturating_add(1);
            }
            keep
        });
    }

    // 4. Group identical locations; each source keeps its best rank.
    let mut groups: BTreeMap<Location, Group> = BTreeMap::new();
    for &(kind, candidate, layer) in &admitted {
        let location = candidate.location();
        let group = groups.entry(location.clone()).or_insert_with(|| Group {
            location,
            layer,
            symbol: None,
            language: None,
            hits: BTreeMap::new(),
            also_at: Vec::new(),
        });
        group.hits.entry(kind).or_insert_with(|| Hit {
            rank: candidate.source_rank,
            raw_score: candidate.raw_score,
            detail: candidate.detail.clone(),
        });
        if group.symbol.is_none() {
            group.symbol.clone_from(&candidate.symbol);
        }
        if group.language.is_none() {
            group.language.clone_from(&candidate.language);
        }
    }
    let ordered = sort_groups(groups.into_values().collect(), &weights, k);

    // 5. Merge overlapping duplicates into the better-ranked group.
    let mut kept: Vec<Group> = Vec::new();
    let mut by_hash: BTreeMap<ContentHash, Vec<usize>> = BTreeMap::new();
    for group in ordered {
        let target = by_hash
            .get(&group.location.content_hash)
            .and_then(|indices| {
                indices.iter().copied().find(|i| {
                    kept.get(*i)
                        .is_some_and(|keeper| keeper.location.overlaps(&group.location))
                })
            });
        match target.and_then(|i| kept.get_mut(i)) {
            Some(keeper) => {
                merge(keeper, group);
                stats.merged_duplicates = stats.merged_duplicates.saturating_add(1);
            }
            None => {
                by_hash
                    .entry(group.location.content_hash)
                    .or_default()
                    .push(kept.len());
                kept.push(group);
            }
        }
    }
    let kept = sort_groups(kept, &weights, k);
    stats.fused = kept.len();

    // 6. Result quota and limit.
    let selected = apply_result_quota(kept, config, &mut stats);
    let results = (1u32..)
        .zip(selected)
        .map(|(rank, group)| build_result(rank, group, plan, scope, &weights, k))
        .collect();

    Fused {
        results,
        degraded,
        consulted,
        answered,
        stats,
    }
}

fn merge(keeper: &mut Group, other: Group) {
    for (kind, hit) in other.hits {
        match keeper.hits.entry(kind) {
            Entry::Vacant(slot) => {
                slot.insert(hit);
            }
            Entry::Occupied(mut slot) => {
                if hit.rank < slot.get().rank {
                    slot.insert(hit);
                }
            }
        }
    }
    if keeper.symbol.is_none() {
        keeper.symbol = other.symbol;
    }
    if keeper.language.is_none() {
        keeper.language = other.language;
    }
    keeper.also_at.push(other.location);
    keeper.also_at.extend(other.also_at);
}

/// Work-conserving fairness: each project's first `quota` results keep their
/// place; the rest move behind every other project's results and fill the
/// list only when nothing else is left.
fn apply_result_quota(
    groups: Vec<Group>,
    config: &FusionConfig,
    stats: &mut SearchStats,
) -> Vec<Group> {
    let total = groups.len();
    let mut selected = match config.result_quota_per_project {
        None => groups,
        Some(quota) => {
            let mut per_project: BTreeMap<Name, usize> = BTreeMap::new();
            let mut first = Vec::new();
            let mut deferred = Vec::new();
            for group in groups {
                let n = per_project
                    .entry(group.location.project.clone())
                    .or_insert(0);
                if *n < quota {
                    *n = n.saturating_add(1);
                    first.push(group);
                } else {
                    deferred.push(group);
                }
            }
            stats.deferred_by_quota = deferred.len();
            first.extend(deferred);
            first
        }
    };
    stats.truncated_by_limit = total.saturating_sub(config.result_limit);
    selected.truncate(config.result_limit);
    selected
}

fn build_result(
    rank: u32,
    mut group: Group,
    plan: &QueryPlan,
    scope: &QueryScope,
    weights: &SourceWeights,
    k: u32,
) -> SearchResult {
    let mut sources = Vec::with_capacity(group.hits.len());
    let mut why = Vec::new();
    for (kind, hit) in &group.hits {
        let weight = weights.get(*kind);
        sources.push(SourceScore {
            source: *kind,
            rank: hit.rank,
            raw_score: hit.raw_score,
            weight,
            contribution: rrf(weight, k, hit.rank),
        });
        match &hit.detail {
            MatchDetail::Exact { term, target } => why.push(Reason::ExactMatch {
                term: term.clone(),
                target: *target,
            }),
            MatchDetail::Lexical { terms } => {
                why.push(Reason::LexicalTerms {
                    terms: terms.clone(),
                });
                for term in terms {
                    let folded = fold(term);
                    let expansion = plan
                        .expansions
                        .iter()
                        .find(|e| fold(&e.expansion) == folded);
                    if let Some(expansion) = expansion {
                        why.push(Reason::GlossaryExpansion {
                            matched: expansion.matched.clone(),
                            expansion: expansion.expansion.clone(),
                            status: expansion.status,
                        });
                    }
                }
            }
            MatchDetail::Semantic { profile } => why.push(Reason::SemanticSimilarity {
                similarity: hit.raw_score,
                profile: profile.clone(),
            }),
        }
    }
    if group.layer == Layer::Overlay {
        why.push(Reason::PersonalOverlay {
            view: group.location.view.clone(),
        });
    }
    let fused = sources.iter().map(|s| s.contribution).sum();
    let commit = scope
        .pinned(&group.location.project, &group.location.view)
        .and_then(|(pin, _)| pin.commit.clone());
    group.also_at.sort();
    group.also_at.dedup();
    SearchResult {
        rank,
        id: group.location.label(),
        location: group.location,
        commit,
        layer: group.layer,
        symbol: group.symbol,
        language: group.language,
        score: ScoreBreakdown {
            rrf_k: k,
            sources,
            fused,
            rerank: None,
        },
        why,
        also_at: group.also_at,
    }
}
