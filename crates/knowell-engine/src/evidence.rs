//! Turning query results into the MCP evidence contract: result ids,
//! `Evidence` (project, ref, layer, commit, path, lines, content hash, why,
//! freshness, index state) and gaps that explain empty or partial answers.

use knowell_core::{LineRange, Name, RepoPath, TrackTarget};
use knowell_mcp::tools::{HitKind, QueryClass};
use knowell_mcp::{
    CommitId, Evidence, EvidenceType, FreshnessTier, Gap, GapReason, GraphHop, MatchReason,
    RelationKind, Resolution, ResultId, ToolError, ViewLayer,
};
use knowell_query::{
    Component, CoverageGap, Degradation, EdgeKind, EmptyReason, ExactTarget, GraphStep, Intent,
    Location, Reason, ScoreBreakdown, SearchResponse, SourceKind,
};

use crate::ids::source_id;
use crate::search::{Prepared, is_test_path};
use crate::snapshot::{line_count, whole_file};

/// Where a located item lives, resolved against the prepared views.
pub(crate) struct Placed {
    pub(crate) id: ResultId,
    pub(crate) evidence: Evidence,
    pub(crate) language: Option<String>,
}

/// Builds the id and evidence of `location`; `None` when the location is
/// not in a prepared view or has no commit (plain directories cannot be
/// cited by commit).
pub(crate) fn place(
    prepared: &Prepared,
    location: &Location,
    symbol: Option<&str>,
    why: Vec<MatchReason>,
    freshness: FreshnessTier,
) -> Result<Option<Placed>, ToolError> {
    let Some((view, is_overlay)) = prepared.view_of(&location.project, &location.view) else {
        return Ok(None);
    };
    let pinned = &view.pinned;
    let (commit, lines_total, language, target, layer) = if is_overlay {
        let Some(overlay) = &view.overlay else {
            return Ok(None);
        };
        let Some(file) = overlay.overlay.file(&location.path) else {
            return Ok(None);
        };
        let commit = overlay
            .overlay
            .head_commit()
            .map(str::to_owned)
            .or_else(|| pinned.commit.clone());
        (
            commit,
            line_count(&file.text),
            Some(file.parsed.language.as_str().to_owned()),
            TrackTarget::WorktreeHead,
            ViewLayer::Personal,
        )
    } else {
        let Some(file) = view.snapshot.file(&location.path) else {
            return Ok(None);
        };
        (
            pinned.commit.clone(),
            file.line_count,
            file.language.clone(),
            pinned.target.clone(),
            ViewLayer::Shared,
        )
    };
    let Some(commit) = commit.and_then(|c| CommitId::new(c).ok()) else {
        return Ok(None);
    };
    let Some(lines) = location.range.or_else(|| whole_file(lines_total)) else {
        return Ok(None);
    };
    let id = source_id(
        &location.project,
        Some(commit.as_str()),
        &location.content_hash,
        &location.path,
        lines,
    )?;
    Ok(Some(Placed {
        id,
        evidence: Evidence {
            project: location.project.clone(),
            view: target,
            layer,
            commit,
            path: location.path.clone(),
            lines,
            content_hash: location.content_hash,
            symbol: symbol.map(str::to_owned),
            why,
            freshness,
            index_state: pinned.index_state(),
        },
        language,
    }))
}

/// The analysis tier a result's evidence comes from: embeddings when the
/// semantic source contributed, symbols and chunks (T1) when it has a range
/// or an exact symbol, plain text (T0) otherwise.
pub(crate) fn freshness_of(score: &ScoreBreakdown, range: Option<LineRange>) -> FreshnessTier {
    if score
        .sources
        .iter()
        .any(|s| s.source == SourceKind::Semantic)
    {
        FreshnessTier::T2Embeddings
    } else if range.is_some() || score.sources.iter().any(|s| s.source == SourceKind::Exact) {
        FreshnessTier::T1Symbols
    } else {
        FreshnessTier::T0Text
    }
}

fn label(location: &Location, symbol: Option<&str>) -> String {
    match symbol {
        Some(s) => format!("{}:{s}", location.project),
        None => format!("{}:{}", location.project, location.path),
    }
}

fn relation(edge: EdgeKind) -> RelationKind {
    match edge {
        // Caller/callee edges currently come from stored file imports.
        EdgeKind::Caller | EdgeKind::Callee => RelationKind::Imports,
        EdgeKind::Test => RelationKind::Tests,
        EdgeKind::Doc => RelationKind::Documents,
        EdgeKind::Type | EdgeKind::Contract => RelationKind::References,
    }
}

pub(crate) fn evidence_type(e: knowell_query::EvidenceType) -> EvidenceType {
    match e {
        knowell_query::EvidenceType::SemanticallyResolved => EvidenceType::SemanticallyResolved,
        knowell_query::EvidenceType::ContractDerived => EvidenceType::ContractDerived,
        knowell_query::EvidenceType::RuntimeObservation => EvidenceType::RuntimeObservation,
        knowell_query::EvidenceType::SyntacticObservation => EvidenceType::SyntacticObservation,
        knowell_query::EvidenceType::HeuristicMatch => EvidenceType::HeuristicMatch,
        knowell_query::EvidenceType::ModelSuggestion => EvidenceType::ModelSuggestion,
    }
}

pub(crate) fn resolution(r: knowell_query::Resolution) -> Resolution {
    match r {
        knowell_query::Resolution::Resolved => Resolution::Resolved,
        knowell_query::Resolution::Ambiguous => Resolution::Ambiguous,
        knowell_query::Resolution::Unresolved => Resolution::Unresolved,
    }
}

/// Graph steps as MCP hops, starting at `seed`.
pub(crate) fn hops(
    seed: &Location,
    seed_symbol: Option<&str>,
    steps: &[GraphStep],
) -> Vec<GraphHop> {
    let mut from = label(seed, seed_symbol);
    steps
        .iter()
        .map(|step| {
            let to = label(&step.to, step.symbol.as_deref());
            let hop = GraphHop {
                from: from.clone(),
                relation: relation(step.edge),
                to: to.clone(),
                evidence_type: evidence_type(step.evidence),
                resolution: resolution(step.resolution),
            };
            from = to;
            hop
        })
        .collect()
}

/// Query reasons as MCP match reasons (signals stay separate).
pub(crate) fn reasons(
    why: &[Reason],
    score: Option<&ScoreBreakdown>,
    symbol: Option<&str>,
    location: &Location,
) -> Vec<MatchReason> {
    let rank_of = |kind: SourceKind| {
        score
            .and_then(|s| s.sources.iter().find(|x| x.source == kind))
            .map_or(1, |x| x.rank)
    };
    let mut out = Vec::new();
    for reason in why {
        let mapped = match reason {
            Reason::ExactMatch { term, target } => match target {
                ExactTarget::Symbol | ExactTarget::ErrorCode | ExactTarget::Text => {
                    MatchReason::ExactSymbol {
                        symbol: symbol.map_or_else(|| term.clone(), str::to_owned),
                    }
                }
                ExactTarget::Path => MatchReason::ExactPath,
                ExactTarget::Contract => MatchReason::Contract {
                    contract: term.clone(),
                },
            },
            Reason::LexicalTerms { terms } => MatchReason::Lexical {
                terms: terms.clone(),
                rank: rank_of(SourceKind::Lexical),
            },
            Reason::SemanticSimilarity { profile, .. } => MatchReason::Semantic {
                profile: profile.clone(),
                rank: rank_of(SourceKind::Semantic),
            },
            Reason::GraphPath { seed, steps } => MatchReason::GraphPath {
                hops: hops(seed, None, steps),
            },
            Reason::TestReferences { .. } => MatchReason::TestReference {
                test: location.path.to_string(),
            },
            Reason::GlossaryExpansion { .. } | Reason::PersonalOverlay { .. } => continue,
        };
        if !out.contains(&mapped) {
            out.push(mapped);
        }
    }
    out
}

/// The kind of a hit, from its path, language and match.
pub(crate) fn hit_kind(path: &RepoPath, language: Option<&str>, why: &[MatchReason]) -> HitKind {
    if why
        .iter()
        .any(|r| matches!(r, MatchReason::Contract { .. }))
    {
        return HitKind::Contract;
    }
    if is_test_path(path) {
        return HitKind::Test;
    }
    match language {
        Some("markdown" | "text") => return HitKind::Doc,
        Some("yaml" | "json" | "toml" | "ini" | "dockerfile" | "hcl" | "xml") => {
            return HitKind::Config;
        }
        _ => {}
    }
    if why
        .iter()
        .any(|r| matches!(r, MatchReason::ExactSymbol { .. }))
    {
        HitKind::Symbol
    } else {
        HitKind::Code
    }
}

/// The MCP class of a planned intent.
pub(crate) fn query_class(intent: Intent) -> QueryClass {
    match intent {
        Intent::ExactSymbol => QueryClass::ExactSymbol,
        Intent::PathOrFile => QueryClass::Path,
        Intent::Endpoint => QueryClass::Contract,
        Intent::ErrorTrace => QueryClass::ErrorTrace,
        Intent::Behavior => QueryClass::Behavior,
        Intent::Impact => QueryClass::Impact,
        Intent::Why => QueryClass::Rationale,
    }
}

/// Gaps for degraded parts of the pipeline.
pub(crate) fn degradation_gaps(degraded: &[Degradation]) -> Vec<Gap> {
    degraded
        .iter()
        .map(|d| {
            let reason = match d.component {
                Component::Semantic => GapReason::EmbeddingsNotReady,
                Component::Graph => GapReason::RelationsNotReady,
                Component::Lexical | Component::Exact => GapReason::ProjectNotIndexed,
                Component::Rerank | Component::Classifier => GapReason::NoMatches,
            };
            Gap::new(reason, d.to_string())
        })
        .collect()
}

/// Gaps for analysis the searched views lack.
pub(crate) fn coverage_gaps(gaps: &[CoverageGap]) -> Vec<Gap> {
    gaps.iter()
        .map(|CoverageGap::NoReferenceResolution { project, language }| {
            Gap::for_project(
                GapReason::NoReferenceResolutionForLanguage,
                project.clone(),
                format!(
                    "references in {language} are matched structurally (imports) only; missing callers do not mean there are none"
                ),
            )
        })
        .collect()
}

/// Gaps explaining an empty search.
pub(crate) fn empty_gaps(response: &SearchResponse) -> Vec<Gap> {
    let Some(empty) = &response.empty else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for reason in &empty.reasons {
        let gap = match reason {
            EmptyReason::EmptyQuery => {
                Gap::new(GapReason::NoMatches, "the query has nothing to search for")
            }
            EmptyReason::UnknownProject { project } => Gap::for_project(
                GapReason::NotFound,
                project.clone(),
                format!("project {project} does not exist in this workspace"),
            ),
            EmptyReason::ProjectNotIndexed { project } => Gap::for_project(
                GapReason::ProjectNotIndexed,
                project.clone(),
                format!("{project} has no index yet"),
            ),
            EmptyReason::NoProjectsInScope => Gap::new(
                GapReason::FiltersExcludedAll,
                "no indexed project is inside the requested scope",
            ),
            EmptyReason::SourcesUnavailable { .. } => continue,
            EmptyReason::NoReferenceResolutionForLanguage { project, language } => {
                Gap::for_project(
                    GapReason::NoReferenceResolutionForLanguage,
                    project.clone(),
                    format!("no reference resolution for {language}"),
                )
            }
            EmptyReason::NoCandidatesInSelectedRef { views } => {
                let projects: Vec<String> = views
                    .iter()
                    .map(|v| v.project.to_string())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                Gap::new(
                    GapReason::NoCandidatesInSelectedRef,
                    format!(
                        "searched {} and nothing matched in the pinned refs",
                        projects.join(", ")
                    ),
                )
            }
            EmptyReason::AllFilteredByScope { .. } => Gap::new(
                GapReason::FiltersExcludedAll,
                "project, language or path filters excluded every candidate",
            ),
            EmptyReason::AllOutsidePinnedViews { .. } => Gap::new(
                GapReason::NoCandidatesInSelectedRef,
                "candidates belonged to other generations than the pinned ones; call open_workspace again",
            ),
            EmptyReason::AllShadowedByOverlay { .. } => Gap::new(
                GapReason::NoCandidatesInSelectedRef,
                "the only candidates were shared-view versions of files your worktree changed",
            ),
            EmptyReason::MalformedCandidates { .. } => Gap::new(
                GapReason::NoMatches,
                "candidates were malformed and dropped",
            ),
        };
        if !out.contains(&gap) {
            out.push(gap);
        }
    }
    if out.is_empty() {
        out.push(Gap::new(GapReason::NoMatches, empty.note.clone()));
    }
    out
}

/// A gap for a project whose results cannot be cited by commit.
pub(crate) fn no_commit_gap(project: &Name) -> Gap {
    Gap::for_project(
        GapReason::ExcludedByPolicy,
        project.clone(),
        format!(
            "{project} is a plain directory: its results have no commit to cite and were left out"
        ),
    )
}

#[cfg(test)]
mod tests {
    use knowell_core::ContentHash;

    use super::*;

    fn location(path: &str) -> Location {
        Location {
            project: Name::new("api").unwrap(),
            path: RepoPath::new(path).unwrap(),
            range: None,
            view: knowell_query::ViewId::new("v").unwrap(),
            generation: 1,
            content_hash: ContentHash::of(b"x"),
        }
    }

    #[test]
    fn hit_kinds_follow_paths_and_languages() {
        let p = |s: &str| RepoPath::new(s).unwrap();
        assert_eq!(
            hit_kind(&p("src/a.spec.ts"), Some("typescript"), &[]),
            HitKind::Test
        );
        assert_eq!(
            hit_kind(&p("docs/a.md"), Some("markdown"), &[]),
            HitKind::Doc
        );
        assert_eq!(
            hit_kind(&p("k8s/a.yaml"), Some("yaml"), &[]),
            HitKind::Config
        );
        assert_eq!(
            hit_kind(
                &p("src/a.ts"),
                Some("typescript"),
                &[MatchReason::ExactSymbol { symbol: "A".into() }]
            ),
            HitKind::Symbol
        );
        assert_eq!(
            hit_kind(&p("src/a.ts"), Some("typescript"), &[]),
            HitKind::Code
        );
    }

    #[test]
    fn graph_steps_become_named_hops() {
        let seed = location("src/a.ts");
        let step = GraphStep {
            edge: EdgeKind::Caller,
            evidence: knowell_query::EvidenceType::SyntacticObservation,
            resolution: knowell_query::Resolution::Resolved,
            to: location("src/b.ts"),
            symbol: None,
        };
        let hops = hops(&seed, Some("A.f"), &[step]);
        assert_eq!(hops.len(), 1);
        assert_eq!(hops[0].from, "api:A.f");
        assert_eq!(hops[0].to, "api:src/b.ts");
        assert_eq!(hops[0].relation, RelationKind::Imports);
        assert_eq!(hops[0].evidence_type, EvidenceType::SyntacticObservation);
    }
}
