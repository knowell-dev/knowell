//! `search`, `fetch` and `inspect_symbol`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use knowell_core::{LineRange, Name, RepoPath, TrackTarget};
use knowell_mcp::tools::{
    FetchInput, FetchOutput, FetchedItem, HitKind, InspectSymbolInput, InspectSymbolOutput,
    QueryEmbeddingUsage, SearchDiagnostics, SearchHit, SearchInput, SearchKind, SearchOutput,
    SymbolFacet, SymbolInfo, SymbolKind, SymbolLink, VersionStatus,
};
use knowell_mcp::{
    AnalysisLevel, EvidenceType, FileLocator, FreshnessTier, Gap, GapReason, MatchReason,
    RelationKind, Resolution, ResultId, ToolError, UntrustedText,
};
use knowell_parse::SymbolKind as ParseKind;
use knowell_query::{Language, Origin, PackItem, Snippet};
use knowell_secrets::ExclusionPolicy;
use knowell_store::{GenerationState, OccurrenceRole, content, symbols as stored_symbols, views};

use super::workspace::analysis_level;
use crate::access::Access;
use crate::engine::Engine;
use crate::error::store_tool;
use crate::evidence::{
    coverage_gaps, degradation_gaps, empty_gaps, freshness_of, hit_kind, no_commit_gap, place,
    query_class, reasons, source_omission_gaps,
};
use crate::ids::{SourceRef, parse_source_id, source_id};
use crate::scope::{Pinned, PinnedProject};
use crate::search::{Filters, is_test_path, source_snippets, symbol_strength};
use crate::snapshot::{Snapshot, SymbolEntry, line_count, whole_file};

/// Internal acquisition bounds, independent of each facet's presentation limit.
const INSPECT_OCCURRENCE_LIMIT: usize = 2048;
const INSPECT_EDGE_LIMIT: usize = 4096;
const INSPECT_PAGE_ROWS: u32 = 256;

struct InspectLinks {
    references: Vec<SymbolLink>,
    implementations: Vec<SymbolLink>,
    tests: Vec<SymbolLink>,
    compiler_available: bool,
}

fn inspect_evidence(evidence: knowell_store::EvidenceType) -> EvidenceType {
    match evidence {
        knowell_store::EvidenceType::SemanticResolved => EvidenceType::SemanticallyResolved,
        knowell_store::EvidenceType::ContractDerived => EvidenceType::ContractDerived,
        knowell_store::EvidenceType::Syntactic => EvidenceType::SyntacticObservation,
        knowell_store::EvidenceType::Heuristic => EvidenceType::HeuristicMatch,
        knowell_store::EvidenceType::ModelSuggestion => EvidenceType::ModelSuggestion,
        knowell_store::EvidenceType::RuntimeObserved => EvidenceType::RuntimeObservation,
    }
}

fn inspect_resolution(resolution: knowell_store::Resolution) -> Resolution {
    match resolution {
        knowell_store::Resolution::Resolved => Resolution::Resolved,
        knowell_store::Resolution::Ambiguous => Resolution::Ambiguous,
        knowell_store::Resolution::Unresolved => Resolution::Unresolved,
    }
}

fn inspect_relation(kind: &str) -> Option<RelationKind> {
    match kind {
        "references" => Some(RelationKind::References),
        "calls" => Some(RelationKind::Calls),
        "implements" => Some(RelationKind::Implements),
        "tests" => Some(RelationKind::Tests),
        _ => None,
    }
}

fn inspect_link(
    project: &PinnedProject,
    snapshot: &Snapshot,
    path: &RepoPath,
    lines: LineRange,
    relation: RelationKind,
    evidence_type: EvidenceType,
    resolution: Resolution,
) -> Result<Option<SymbolLink>, ToolError> {
    if project.overlay.as_ref().is_some_and(|overlay| {
        overlay.overlay.file(path).is_some() || overlay.overlay.deleted().contains(path)
    }) {
        return Ok(None);
    }
    let Some(file) = snapshot.file(path) else {
        return Ok(None);
    };
    if lines.end() > file.line_count {
        return Ok(None);
    }
    let Some(commit) = project.commit_id() else {
        return Ok(None);
    };
    Ok(Some(SymbolLink {
        id: source_id(
            &project.entry.name,
            Some(commit.as_str()),
            &file.content_hash,
            path,
            lines,
        )?,
        relation,
        evidence_type,
        resolution,
        evidence: knowell_mcp::Evidence {
            project: project.entry.name.clone(),
            view: project.target.clone(),
            layer: knowell_mcp::ViewLayer::Shared,
            commit,
            path: path.clone(),
            lines,
            content_hash: file.content_hash,
            symbol: snapshot
                .enclosing_symbol(path, lines)
                .map(|symbol| symbol.local.clone()),
            why: Vec::new(),
            freshness: if evidence_type == EvidenceType::SemanticallyResolved {
                FreshnessTier::T3Relations
            } else {
                FreshnessTier::T1Symbols
            },
            index_state: project.index_state(),
        },
    }))
}

fn inspect_link_order(link: &SymbolLink) -> (u8, u8, u8) {
    let evidence = match link.evidence_type {
        EvidenceType::SemanticallyResolved => 0,
        EvidenceType::RuntimeObservation => 1,
        EvidenceType::ContractDerived => 2,
        EvidenceType::SyntacticObservation => 3,
        EvidenceType::HeuristicMatch => 4,
        EvidenceType::ModelSuggestion => 5,
    };
    let resolution = match link.resolution {
        Resolution::Resolved => 0,
        Resolution::Ambiguous => 1,
        Resolution::Unresolved => 2,
    };
    let relation = match link.relation {
        RelationKind::Calls => 0,
        RelationKind::References => 1,
        RelationKind::Imports => 3,
        _ => 2,
    };
    (evidence, resolution, relation)
}

/// An import declaration can carry a symbol reference at the same line anchor,
/// but does not prove that a test exercises the imported symbol. Calls retain
/// their explicit observed relation even when they share an import's line.
fn inspect_test_association(
    import_ranges: &BTreeMap<RepoPath, Vec<LineRange>>,
    path: &RepoPath,
    lines: LineRange,
    relation: RelationKind,
) -> bool {
    is_test_path(path)
        && (relation == RelationKind::Calls
            || !import_ranges.get(path).is_some_and(|ranges| {
                ranges
                    .iter()
                    .any(|range| range.start() <= lines.start() && range.end() >= lines.end())
            }))
}

/// A compiler call also carries a reference at the same span. Keep its strongest
/// observation before charging the facet limit. Module-import navigation remains
/// distinct even when a symbol reference shares its line anchor.
fn finish_inspect_links(
    links: &mut Vec<SymbolLink>,
    limit: usize,
    facet: &str,
    project: &Name,
    gaps: &mut Vec<Gap>,
) {
    links.sort_by(|left, right| {
        left.evidence
            .path
            .cmp(&right.evidence.path)
            .then_with(|| left.evidence.lines.cmp(&right.evidence.lines))
            .then_with(|| {
                (left.relation == RelationKind::Imports)
                    .cmp(&(right.relation == RelationKind::Imports))
            })
            .then_with(|| inspect_link_order(left).cmp(&inspect_link_order(right)))
            .then_with(|| left.id.cmp(&right.id))
    });
    links.dedup_by(|left, right| {
        left.evidence.path == right.evidence.path
            && left.evidence.lines == right.evidence.lines
            && (left.relation == RelationKind::Imports) == (right.relation == RelationKind::Imports)
    });
    links.sort_by(|left, right| {
        inspect_link_order(left)
            .cmp(&inspect_link_order(right))
            .then_with(|| left.evidence.path.cmp(&right.evidence.path))
            .then_with(|| left.evidence.lines.cmp(&right.evidence.lines))
            .then_with(|| left.id.cmp(&right.id))
    });
    if links.len() > limit {
        gaps.push(Gap::for_project(
            GapReason::LimitReached,
            project.clone(),
            format!(
                "{} {facet} links omitted by the per-facet limit; increase limit",
                links.len().saturating_sub(limit)
            ),
        ));
        links.truncate(limit);
    }
}

fn symbol_kind(kind: ParseKind) -> SymbolKind {
    match kind {
        ParseKind::Function => SymbolKind::Function,
        ParseKind::Method | ParseKind::Constructor => SymbolKind::Method,
        ParseKind::Class => SymbolKind::Class,
        ParseKind::Struct | ParseKind::Message => SymbolKind::Struct,
        ParseKind::Enum => SymbolKind::Enum,
        ParseKind::Interface => SymbolKind::Interface,
        ParseKind::Trait => SymbolKind::Trait,
        ParseKind::TypeAlias | ParseKind::Schema => SymbolKind::Type,
        ParseKind::Constant => SymbolKind::Constant,
        ParseKind::Variable | ParseKind::Field | ParseKind::Column | ParseKind::Key => {
            SymbolKind::Variable
        }
        ParseKind::Module => SymbolKind::Module,
        ParseKind::Endpoint => SymbolKind::Endpoint,
        ParseKind::Test => SymbolKind::Test,
        _ => SymbolKind::Other,
    }
}

/// Lines `range` widened by `context` on both sides, within the file.
fn widen(range: LineRange, context: u32, total: u32) -> LineRange {
    let start = range.start().saturating_sub(context).max(1);
    let end = range
        .end()
        .saturating_add(context)
        .min(total.max(range.end()));
    LineRange::new(start, end.max(start)).unwrap_or(range)
}

/// Requested source takes precedence over optional surrounding context. In
/// particular, fetching a continuation must not return its preceding page.
fn widen_for_fetch(range: LineRange, context: u32, total: u32, max: u32) -> LineRange {
    let spare = max.saturating_sub(range.line_count());
    widen(range, context.min(spare / 2), total)
}

/// `range` clamped to `max` lines; `true` when it was cut.
fn cap(range: LineRange, max: u32) -> (LineRange, bool) {
    if range.line_count() <= max {
        return (range, false);
    }
    let end = range.start().saturating_add(max.saturating_sub(1));
    (LineRange::new(range.start(), end).unwrap_or(range), true)
}

enum SearchEmission<'a> {
    Ranked(&'a knowell_query::SearchResult),
    Packed(&'a PackItem),
}

/// Resolves both bytes and provenance. Multiple retained commits sharing the
/// short id prefix are ambiguous even when their file contents are identical.
fn historical_source<'a>(
    source: &SourceRef,
    history: &'a [content::FileVersion],
    generations: &'a [views::ViewGeneration],
) -> Option<(&'a content::FileVersion, &'a views::ViewGeneration)> {
    let mut selected: Option<(&content::FileVersion, &views::ViewGeneration)> = None;
    for generation in generations {
        if !matches!(
            generation.state,
            GenerationState::Active | GenerationState::Retired
        ) || generation.resolved_commit.is_none()
            || !source.names_commit(generation.resolved_commit.as_deref())
        {
            continue;
        }
        let Some(version) = history.iter().find(|version| {
            version.view == generation.view
                && source.names_version(&version.content_hash)
                && version.valid_from <= generation.generation
                && version
                    .valid_to
                    .is_none_or(|end| generation.generation < end)
        }) else {
            continue;
        };
        if let Some((_, previous)) = selected {
            if previous.resolved_commit != generation.resolved_commit {
                return None;
            }
            if previous.generation >= generation.generation {
                continue;
            }
        }
        selected = Some((version, generation));
    }
    selected
}

impl Engine {
    pub(crate) async fn tool_search(
        &self,
        access: Access,
        input: SearchInput,
    ) -> Result<SearchOutput, ToolError> {
        let started = Instant::now();
        let mut diagnostics = None;
        let pinned = self
            .resolve_target_for_projects(&access, &input.target, &input.projects)
            .await?;
        let limit = usize::try_from(input.limit.unwrap_or(10)).unwrap_or(10);
        let requested = input.token_budget.unwrap_or(4000);
        let include_snippets = input.include_snippets.unwrap_or(true);
        let mut used_tokens = 0u32;
        let wants = |kind: SearchKind| input.kinds.is_empty() || input.kinds.contains(&kind);
        let languages = if input.languages.is_empty() {
            None
        } else {
            Some(
                input
                    .languages
                    .iter()
                    .map(|l| {
                        Language::new(l).map_err(|_| {
                            ToolError::invalid_input("`languages` holds an invalid name")
                        })
                    })
                    .collect::<Result<BTreeSet<_>, _>>()?,
            )
        };
        let filters = Filters {
            projects: (!input.projects.is_empty())
                .then(|| input.projects.iter().cloned().collect()),
            languages,
            path_prefixes: input.path_prefixes.clone(),
            kinds: input.kinds.clone(),
            sections: Vec::new(),
        };
        let mut gaps = pinned.gaps.clone();
        gaps.extend(pinned.not_indexed_gaps(&input.projects));
        let mut hits = Vec::new();
        let mut more_available = false;
        let mut query_class_out =
            query_class(knowell_query::plan(&input.query, &self.inner.glossary).intent);
        if wants(SearchKind::Code) || wants(SearchKind::Docs) || wants(SearchKind::Contracts) {
            let run = if include_snippets {
                self.run_source_search(
                    &pinned,
                    &filters,
                    &input.query,
                    limit.saturating_mul(4).clamp(16, 64),
                )
                .await?
            } else {
                self.run_locator_search(&pinned, &filters, &input.query, limit)
                    .await?
            };
            query_class_out = query_class(run.response.plan.intent);
            if input.include_diagnostics.unwrap_or(false) {
                let work = &run.retrieval;
                let usage = work.embedding_usage;
                let count = |value: usize| u64::try_from(value).unwrap_or(u64::MAX);
                diagnostics = Some(SearchDiagnostics {
                    elapsed_ms: 0,
                    preparation_ms: work.preparation_ms,
                    prepared_views: count(work.prepared_views),
                    prepared_file_occurrences: count(work.prepared_file_occurrences),
                    exact_ms: work.exact_ms,
                    lexical_ms: work.lexical_ms,
                    semantic_ms: work.semantic_ms,
                    fusion_expansion_ms: work.fusion_expansion_ms,
                    snippet_read_ms: 0,
                    source_hydrated_paths: count(work.source_hydrated_paths),
                    source_hydration_skipped_paths: count(work.source_hydration_skipped_paths),
                    locator_only: work.locator_only,
                    lexical_queries: count(work.lexical_queries),
                    lexical_file_hits: count(work.lexical_file_hits_examined),
                    lexical_spans: count(work.lexical_candidates),
                    semantic_queries: count(work.semantic_queries),
                    semantic_neighbors: count(work.semantic_neighbors_examined),
                    embedding_calls: count(work.embedding_calls),
                    embedding_failures: count(work.embedding_failures),
                    exact_path_embedding_bypassed: work.exact_path_embedding_bypass,
                    embedding: (usage.requests > 0 || usage.input_tokens > 0).then_some(
                        QueryEmbeddingUsage {
                            input_tokens: usage.input_tokens,
                            tokens_estimated: usage.tokens_estimated,
                            requests: usage.requests,
                            retries: usage.retries,
                            operation_ms: u64::try_from(usage.latency.as_millis())
                                .unwrap_or(u64::MAX),
                        },
                    ),
                });
            }
            let snippet_started = Instant::now();
            let texts = if include_snippets {
                self.source_texts_for(&run).await?
            } else {
                Default::default()
            };
            let source = source_snippets(&run, texts, self.inner.settings.max_fetch_lines);
            let selected = if include_snippets {
                Some(
                    knowell_query::pack_task_with(
                        &run.response,
                        crate::source_region::source_budget(requested),
                        &source,
                        &crate::source_region::SourceBudgetCounter,
                        &knowell_query::TaskPackOptions {
                            strategy: knowell_query::TaskSelectionStrategy::Source,
                            desired_roles: Vec::new(),
                            candidate_limit: 64,
                            ..Default::default()
                        },
                    )
                    .map_err(|error| ToolError::internal(error.to_string()))?,
                )
            } else {
                None
            };
            let emissions: Vec<_> = match &selected {
                Some(selected) => {
                    let omissions = source_omission_gaps(&selected.pack.omitted);
                    if selected.pack.items.is_empty()
                        && !run.response.results.is_empty()
                        && omissions.is_empty()
                        && !selected.pack.omitted.iter().any(|omission| {
                            matches!(
                                omission.reason,
                                knowell_query::OmitReason::OverBudget { .. }
                            )
                        })
                    {
                        gaps.push(Gap::new(GapReason::NotFound,
                            "retrieval found source candidates, but no source regions were emitted; inspect the pinned index or increase token_budget"));
                    }
                    gaps.extend(omissions);
                    more_available |= selected.pack.items.len() > limit
                        || !selected.selection.unselected.is_empty()
                        || !selected.pack.omitted.is_empty();
                    if selected.pack.omitted.iter().any(|omission| {
                        matches!(
                            omission.reason,
                            knowell_query::OmitReason::OverBudget { .. }
                        )
                    }) {
                        gaps.push(Gap::new(GapReason::BudgetExhausted,
                            "some source regions did not fit; increase token_budget or narrow the query"));
                    }
                    selected
                        .pack
                        .items
                        .iter()
                        .take(limit)
                        .map(SearchEmission::Packed)
                        .collect()
                }
                None => run
                    .response
                    .results
                    .iter()
                    .take(limit)
                    .map(SearchEmission::Ranked)
                    .collect(),
            };
            if let Some(work) = &mut diagnostics
                && include_snippets
            {
                work.snippet_read_ms = run.retrieval.source_hydration_ms.saturating_add(
                    u64::try_from(snippet_started.elapsed().as_millis()).unwrap_or(u64::MAX),
                );
            }
            let mut no_commit = BTreeSet::new();
            for emission in emissions {
                let (location, symbol, source_why, score, body) = match emission {
                    SearchEmission::Ranked(result) => (
                        &result.location,
                        result.symbol.as_deref(),
                        result.why.as_slice(),
                        Some(&result.score),
                        None,
                    ),
                    SearchEmission::Packed(item) => {
                        used_tokens = used_tokens.saturating_add(item.tokens);
                        let body = Some(Snippet {
                            text: item.text.clone(),
                            range: item.citation.range,
                            content_hash: item.citation.content_hash,
                        });
                        match item.origin {
                            Origin::Result { rank } => {
                                let Some(result) = run
                                    .response
                                    .results
                                    .iter()
                                    .find(|result| result.rank == rank)
                                else {
                                    continue;
                                };
                                (
                                    &result.location,
                                    item.symbol.as_deref(),
                                    item.why.as_slice(),
                                    Some(&result.score),
                                    body,
                                )
                            }
                            Origin::Expanded { seed_rank, depth } => {
                                let Some(expanded) =
                                    run.response.expanded.iter().find(|expanded| {
                                        expanded.seed_rank == seed_rank
                                            && expanded.depth == depth
                                            && expanded.location.project == item.citation.project
                                            && expanded.location.path == item.citation.path
                                            && expanded.location.content_hash
                                                == item.citation.content_hash
                                            && expanded.location.view == item.citation.view
                                            && expanded.location.generation
                                                == item.citation.generation
                                    })
                                else {
                                    continue;
                                };
                                (
                                    &expanded.location,
                                    item.symbol.as_deref(),
                                    item.why.as_slice(),
                                    None,
                                    body,
                                )
                            }
                        }
                    }
                };
                let why = reasons(source_why, score, symbol, location);
                let freshness = score.map_or(FreshnessTier::T1Symbols, |score| {
                    freshness_of(score, location.range)
                });
                let Some(placed) = place(&run.prepared, location, symbol, why.clone(), freshness)?
                else {
                    no_commit.insert(location.project.clone());
                    continue;
                };
                let kind = hit_kind(&location.path, placed.language.as_deref(), &why);
                let wanted = match kind {
                    HitKind::Doc => wants(SearchKind::Docs),
                    HitKind::Contract => wants(SearchKind::Contracts),
                    _ => wants(SearchKind::Code),
                };
                if !wanted {
                    continue;
                }
                let shown_lines = body.as_ref().map(|snippet| snippet.range);
                let snippet_id = if let Some(lines) = shown_lines
                    && lines != placed.evidence.lines
                {
                    Some(source_id(
                        &placed.evidence.project,
                        Some(placed.evidence.commit.as_str()),
                        &placed.evidence.content_hash,
                        &placed.evidence.path,
                        lines,
                    )?)
                } else {
                    None
                };
                let extent = source
                    .source_extent(location, symbol)
                    .unwrap_or(placed.evidence.lines);
                let truncated = shown_lines.is_some_and(|lines| {
                    lines.start() > extent.start() || lines.end() < extent.end()
                });
                let continuation_ids = match shown_lines {
                    Some(lines) => crate::ids::source_continuations(
                        &placed.evidence.project,
                        Some(placed.evidence.commit.as_str()),
                        &placed.evidence.content_hash,
                        &placed.evidence.path,
                        lines,
                        extent,
                    )?,
                    None => Vec::new(),
                };
                let snippet = body.map(|snippet| UntrustedText::repository(snippet.text));
                let title = symbol
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("{}:{}", location.path, placed.evidence.lines));
                hits.push(SearchHit {
                    id: placed.id,
                    kind,
                    title,
                    evidence: placed.evidence,
                    snippet_id,
                    snippet_lines: shown_lines,
                    snippet_truncated: truncated,
                    snippet,
                    continuation_ids,
                });
            }
            for project in no_commit {
                gaps.push(no_commit_gap(&project));
            }
            gaps.extend(degradation_gaps(&run.response.degraded));
            gaps.extend(coverage_gaps(&run.response.coverage_gaps));
            if run.response.stats.truncated_by_limit > 0 {
                more_available = true;
                gaps.push(Gap::new(
                    GapReason::LimitReached,
                    format!(
                        "{} more hits exist; raise `limit`",
                        run.response.stats.truncated_by_limit
                    ),
                ));
            }
            if hits.is_empty() {
                gaps.extend(empty_gaps(&run.response));
            }
        }
        let memory_hits = if wants(SearchKind::Memory) {
            self.memory_hits(
                &access,
                &pinned,
                &input.projects,
                &input.query,
                limit,
                &mut gaps,
            )
            .await?
        } else {
            Vec::new()
        };
        if hits.is_empty() && memory_hits.is_empty() && gaps.is_empty() {
            gaps.push(Gap::new(GapReason::NoMatches, knowell_query::ABSENCE_NOTE));
        }
        dedupe(&mut gaps);
        if input.include_diagnostics.unwrap_or(false) {
            let work = diagnostics.get_or_insert_with(SearchDiagnostics::default);
            work.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        }
        Ok(SearchOutput {
            include_handles: input.include_handles.unwrap_or(false),
            query_class: query_class_out,
            hits,
            memory_hits,
            more_available,
            gaps,
            diagnostics,
            budget: Some(knowell_mcp::tools::TokenBudget {
                requested,
                used: used_tokens.min(requested),
            }),
        })
    }

    /// The text of a file version of a pinned project, by full hash.
    pub(crate) async fn text_of(
        &self,
        hash: &knowell_core::ContentHash,
    ) -> Result<Option<Arc<str>>, ToolError> {
        self.inner
            .texts
            .load(&self.inner.store, self.inner.organization, hash)
            .await
            .map_err(ToolError::from)
    }

    pub(crate) async fn tool_fetch(
        &self,
        access: Access,
        input: FetchInput,
    ) -> Result<FetchOutput, ToolError> {
        let pinned = self.resolve_target(&access, &input.target).await?;
        let context = input.context_lines.unwrap_or(0);
        let max_lines = self.inner.settings.max_fetch_lines;
        let mut items = Vec::new();
        let mut gaps = Vec::new();
        for id in &input.ids {
            match self.fetch_id(&pinned, id, context, max_lines).await? {
                Ok(item) => items.push(item),
                Err(gap) => gaps.push(gap),
            }
        }
        for locator in &input.paths {
            match self
                .fetch_path(&pinned, locator, context, max_lines)
                .await?
            {
                Ok(item) => items.push(item),
                Err(gap) => gaps.push(gap),
            }
        }
        Ok(FetchOutput { items, gaps })
    }

    #[allow(clippy::too_many_arguments)]
    fn fetched(
        project: &PinnedProject,
        path: &RepoPath,
        hash: knowell_core::ContentHash,
        language: Option<String>,
        text: &str,
        lines: LineRange,
        status: VersionStatus,
        current_id: Option<ResultId>,
        personal: bool,
        max_lines: u32,
    ) -> Result<Result<FetchedItem, Gap>, ToolError> {
        let total = line_count(text);
        if lines.start() > total {
            return Ok(Err(Gap::for_project(
                GapReason::NotFound,
                project.entry.name.clone(),
                format!("{path}: the requested range starts beyond this source version"),
            )));
        }
        let lines = LineRange::new(
            lines.start().min(total.max(1)),
            lines
                .end()
                .min(total.max(1))
                .max(lines.start().min(total.max(1))),
        )
        .unwrap_or(lines);
        let requested_lines = lines;
        let (lines, truncated) = cap(lines, max_lines);
        let commit = match (personal, &project.overlay) {
            (true, Some(o)) => o
                .overlay
                .head_commit()
                .map(str::to_owned)
                .or_else(|| project.commit.clone()),
            _ => project.commit.clone(),
        };
        let Some(commit_id) = commit
            .as_deref()
            .and_then(|c| knowell_mcp::CommitId::new(c).ok())
        else {
            return Ok(Err(no_commit_gap(&project.entry.name)));
        };
        let id = source_id(&project.entry.name, commit.as_deref(), &hash, path, lines)?;
        let continuation_ids = if truncated {
            let remaining = LineRange::new(lines.end().saturating_add(1), requested_lines.end())
                .map_err(|error| ToolError::internal(error.to_string()))?;
            vec![source_id(
                &project.entry.name,
                commit.as_deref(),
                &hash,
                path,
                remaining,
            )?]
        } else {
            Vec::new()
        };
        Ok(Ok(FetchedItem {
            id,
            evidence: knowell_mcp::Evidence {
                project: project.entry.name.clone(),
                view: if personal {
                    knowell_core::TrackTarget::WorktreeHead
                } else {
                    project.target.clone()
                },
                layer: if personal {
                    knowell_mcp::ViewLayer::Personal
                } else {
                    knowell_mcp::ViewLayer::Shared
                },
                commit: commit_id,
                path: path.clone(),
                lines,
                content_hash: hash,
                symbol: None,
                why: Vec::new(),
                freshness: FreshnessTier::T0Text,
                index_state: project.index_state(),
            },
            language,
            content: UntrustedText::repository(crate::source_region::slice_source_lines(
                text, lines,
            )),
            truncated,
            continuation_ids,
            status,
            current_id,
        }))
    }

    /// A short commit prefix cannot select one of two retained source pins.
    /// Check before current/personal fast paths, without loading source bodies.
    async fn retained_commit_collision(
        &self,
        project: &PinnedProject,
        source: &SourceRef,
        path: &RepoPath,
        expected: &str,
    ) -> Result<bool, ToolError> {
        let mut conn = self.inner.store.acquire().await.map_err(store_tool)?;
        content::retained_source_commit_collision(
            &mut conn,
            self.inner.organization,
            project.view,
            path,
            &source.hash16,
            &source.commit12,
            expected,
        )
        .await
        .map_err(store_tool)
    }

    async fn fetch_id(
        &self,
        pinned: &Pinned,
        id: &ResultId,
        context: u32,
        max_lines: u32,
    ) -> Result<Result<FetchedItem, Gap>, ToolError> {
        let not_found = || {
            Gap::new(
                GapReason::NotFound,
                format!("result id {id} does not resolve to a source range in this context"),
            )
        };
        let Some(source) = parse_source_id(id) else {
            return Ok(Err(not_found()));
        };
        let Some(project) = pinned.projects.get(&source.project) else {
            return Ok(Err(not_found()));
        };
        // The caller's personal layer first.
        if let Some(overlay) = &project.overlay
            && let Some(file) = overlay
                .overlay
                .files()
                .find(|f| source.path.matches(&f.path))
            && source.names_version(&file.content_hash)
            && source.names_commit(overlay.overlay.head_commit().or(project.commit.as_deref()))
        {
            let expected = overlay.overlay.head_commit().or(project.commit.as_deref());
            if let Some(expected) = expected
                && self
                    .retained_commit_collision(project, &source, &file.path, expected)
                    .await?
            {
                return Ok(Err(Gap::for_project(
                    GapReason::NotFound,
                    source.project.clone(),
                    "the source id is ambiguous across retained commits; fetch the pinned file by path instead",
                )));
            }
            let total = line_count(&file.text);
            return Self::fetched(
                project,
                &file.path,
                file.content_hash,
                Some(file.parsed.language.as_str().to_owned()),
                &file.text,
                widen_for_fetch(source.lines, context, total, max_lines),
                VersionStatus::Current,
                None,
                true,
                max_lines,
            );
        }
        let snapshot = self.metadata_snapshot_of(project).await?;
        let path = snapshot
            .files
            .keys()
            .find(|p| source.path.matches(p))
            .cloned();
        let current = path
            .as_ref()
            .and_then(|p| snapshot.file(p).map(|f| (p.clone(), f.clone())));
        if let Some((path, file)) = &current
            && source.names_version(&file.content_hash)
            && source.names_commit(project.commit.as_deref())
        {
            if let Some(expected) = project.commit.as_deref()
                && self
                    .retained_commit_collision(project, &source, path, expected)
                    .await?
            {
                return Ok(Err(Gap::for_project(
                    GapReason::NotFound,
                    source.project.clone(),
                    "the source id is ambiguous across retained commits; fetch the pinned file by path instead",
                )));
            }
            let Some(text) = self.text_of(&file.content_hash).await? else {
                return Ok(Err(not_found()));
            };
            let total = line_count(&text);
            return Self::fetched(
                project,
                path,
                file.content_hash,
                file.language.clone(),
                &text,
                widen_for_fetch(source.lines, context, total, max_lines),
                VersionStatus::Current,
                None,
                false,
                max_lines,
            );
        }
        // Another version: find it in the path's history.
        let Some(path) = path.or_else(|| match &source.path {
            crate::ids::PathRef::Plain(p) => Some(p.clone()),
            crate::ids::PathRef::Hashed(_) => None,
        }) else {
            return Ok(Err(not_found()));
        };
        let mut conn = self.inner.store.acquire().await.map_err(store_tool)?;
        let history = content::file_history(&mut conn, project.view, &path, 200)
            .await
            .map_err(store_tool)?;
        let generations = views::list_generations(&mut conn, project.view)
            .await
            .map_err(store_tool)?;
        drop(conn);
        let Some((version, generation)) = historical_source(&source, &history, &generations) else {
            return Ok(Err(Gap::for_project(
                GapReason::NotFound,
                source.project.clone(),
                "the requested source snapshot is unavailable or ambiguous; its historical commit cannot be verified",
            )));
        };
        let Some(text) = self.text_of(&version.content_hash).await? else {
            return Ok(Err(not_found()));
        };
        let total = line_count(&text);
        let (status, current_id) = match &current {
            Some((cur_path, cur_file)) => {
                // Legacy metadata has no line count. Resolve only this current
                // occurrence instead of parsing unrelated files for a new id.
                let current_total = if cur_file.line_count > 0 {
                    Some(cur_file.line_count)
                } else {
                    self.text_of(&cur_file.content_hash)
                        .await?
                        .as_deref()
                        .map(line_count)
                };
                let lines = current_total.and_then(|total| {
                    LineRange::new(
                        source.lines.start().min(total.max(1)),
                        source.lines.end().min(total.max(1)),
                    )
                    .ok()
                });
                let id = match lines {
                    Some(lines) => Some(source_id(
                        &project.entry.name,
                        project.commit.as_deref(),
                        &cur_file.content_hash,
                        cur_path,
                        lines,
                    )?),
                    None => None,
                };
                (VersionStatus::Changed, id)
            }
            None => (VersionStatus::Deleted, None),
        };
        let Some(commit) = generation.resolved_commit.clone() else {
            return Ok(Err(not_found()));
        };
        let mut historical = project.clone();
        historical.commit = Some(commit.clone());
        historical.target = TrackTarget::Commit(commit);
        historical.generation = generation.generation;
        historical.latest_seen = project.commit.clone();
        historical.building = None;
        historical.overlay = None;
        Self::fetched(
            &historical,
            &version.path,
            version.content_hash,
            version.language.clone(),
            &text,
            widen_for_fetch(source.lines, context, total, max_lines),
            status,
            current_id,
            false,
            max_lines,
        )
    }

    async fn fetch_path(
        &self,
        pinned: &Pinned,
        locator: &FileLocator,
        context: u32,
        max_lines: u32,
    ) -> Result<Result<FetchedItem, Gap>, ToolError> {
        let Some(project) = pinned.projects.get(&locator.project) else {
            let reason = if pinned.not_indexed.contains(&locator.project) {
                GapReason::ProjectNotIndexed
            } else {
                GapReason::NotFound
            };
            return Ok(Err(Gap::for_project(
                reason,
                locator.project.clone(),
                format!("{} is not available in this context", locator.project),
            )));
        };
        if let Some(overlay) = &project.overlay {
            if let Some(file) = overlay.overlay.file(&locator.path) {
                let total = line_count(&file.text);
                let lines = locator
                    .lines
                    .map(|l| widen_for_fetch(l, context, total, max_lines))
                    .or_else(|| whole_file(total))
                    .ok_or_else(|| ToolError::internal("empty range"))?;
                return Self::fetched(
                    project,
                    &file.path,
                    file.content_hash,
                    Some(file.parsed.language.as_str().to_owned()),
                    &file.text,
                    lines,
                    VersionStatus::Current,
                    None,
                    true,
                    max_lines,
                );
            }
            if overlay.overlay.deleted().contains(&locator.path) {
                return Ok(Err(Gap::for_project(
                    GapReason::NotFound,
                    locator.project.clone(),
                    format!("{} was deleted in your worktree", locator.path),
                )));
            }
        }
        let snapshot = self.metadata_snapshot_of(project).await?;
        let Some(file) = snapshot.file(&locator.path).cloned() else {
            let reason = if ExclusionPolicy::builtin().check(&locator.path).is_some() {
                GapReason::ExcludedByPolicy
            } else {
                GapReason::NotFound
            };
            return Ok(Err(Gap::for_project(
                reason,
                locator.project.clone(),
                if reason == GapReason::ExcludedByPolicy {
                    format!(
                        "{} is excluded by the sensitive-file policy; its content is never read",
                        locator.path
                    )
                } else {
                    format!(
                        "{} does not exist in {}@{}",
                        locator.path, locator.project, project.target
                    )
                },
            )));
        };
        let Some(text) = self.text_of(&file.content_hash).await? else {
            return Ok(Err(Gap::for_project(
                GapReason::ExcludedByPolicy,
                locator.project.clone(),
                format!("{} has no stored text (binary or too large)", locator.path),
            )));
        };
        let total = line_count(&text);
        let lines = locator
            .lines
            .map(|l| widen_for_fetch(l, context, total, max_lines))
            .or_else(|| whole_file(total))
            .ok_or_else(|| ToolError::internal("empty range"))?;
        Self::fetched(
            project,
            &locator.path,
            file.content_hash,
            file.language.clone(),
            &text,
            lines,
            VersionStatus::Current,
            None,
            false,
            max_lines,
        )
    }

    /// A result id selects only its exact pinned source version. File-based
    /// graph starts must use the same guard as symbol-based starts.
    pub(crate) async fn source_path_in_snapshot(
        &self,
        project: &PinnedProject,
        snapshot: &Snapshot,
        source: &SourceRef,
    ) -> Result<Option<RepoPath>, ToolError> {
        if source.project != project.entry.name || !source.names_commit(project.commit.as_deref()) {
            return Ok(None);
        }
        let mut matching = snapshot
            .files
            .keys()
            .filter(|path| source.path.matches(path));
        let Some(path) = matching.next() else {
            return Ok(None);
        };
        if matching.next().is_some()
            || project.overlay.as_ref().is_some_and(|overlay| {
                overlay.overlay.file(path).is_some() || overlay.overlay.deleted().contains(path)
            })
        {
            return Ok(None);
        }
        let Some(file) = snapshot.file(path) else {
            return Ok(None);
        };
        if !source.names_version(&file.content_hash) || source.lines.end() > file.line_count {
            return Ok(None);
        }
        if let Some(commit) = project.commit.as_deref()
            && self
                .retained_commit_collision(project, source, path, commit)
                .await?
        {
            return Ok(None);
        }
        Ok(Some(path.clone()))
    }

    /// Symbols matching a name across the pinned projects (strongest
    /// first), or the symbol a result id points at.
    pub(crate) async fn find_symbols(
        &self,
        pinned: &Pinned,
        id: Option<&ResultId>,
        name: Option<&str>,
        project: Option<&Name>,
    ) -> Result<Vec<(PinnedProject, Arc<Snapshot>, SymbolEntry, u8)>, ToolError> {
        let mut found = Vec::new();
        if let Some(id) = id {
            let Some(source) = parse_source_id(id) else {
                return Ok(found);
            };
            let Some(pinned_project) = pinned.projects.get(&source.project) else {
                return Ok(found);
            };
            if project.is_some_and(|name| name != &source.project) {
                return Ok(found);
            }
            let snapshot = self.snapshot_of(pinned_project).await?;
            let Some(path) = self
                .source_path_in_snapshot(pinned_project, &snapshot, &source)
                .await?
            else {
                return Ok(found);
            };
            let mut candidates = snapshot
                .symbols_in(&path)
                .filter(|symbol| symbol.lines == source.lines)
                .cloned()
                .collect::<Vec<_>>();
            if candidates.is_empty() {
                candidates = snapshot
                    .symbols_in(&path)
                    .filter(|symbol| {
                        symbol.lines.start() <= source.lines.start()
                            && symbol.lines.end() >= source.lines.end()
                    })
                    .cloned()
                    .collect();
                if let Some(innermost) = candidates
                    .iter()
                    .map(|symbol| symbol.lines.line_count())
                    .min()
                {
                    candidates.retain(|symbol| symbol.lines.line_count() == innermost);
                }
            }
            // Source ids identify line ranges rather than logical symbols or
            // byte positions. Same-line declarations must remain equally valid.
            candidates.sort_by(|left, right| {
                left.lines
                    .cmp(&right.lines)
                    .then_with(|| left.local.cmp(&right.local))
                    .then_with(|| left.key.cmp(&right.key))
            });
            for symbol in candidates {
                found.push((pinned_project.clone(), Arc::clone(&snapshot), symbol, 6));
            }
            return Ok(found);
        }
        let Some(name) = name else {
            return Ok(found);
        };
        let short = name
            .replace("::", ".")
            .replace('#', ".")
            .rsplit('.')
            .next()
            .unwrap_or(name)
            .to_lowercase();
        for (project_name, pinned_project) in &pinned.projects {
            if project.is_some_and(|p| p != project_name) {
                continue;
            }
            let snapshot = self.snapshot_of(pinned_project).await?;
            for index in snapshot.by_name.get(&short).into_iter().flatten() {
                let Some(symbol) = snapshot.symbols.get(*index) else {
                    continue;
                };
                if let Some(strength) = symbol_strength(symbol, name) {
                    found.push((
                        pinned_project.clone(),
                        Arc::clone(&snapshot),
                        symbol.clone(),
                        strength,
                    ));
                }
            }
        }
        found.sort_by(|a, b| {
            b.3.cmp(&a.3)
                .then_with(|| a.0.entry.name.cmp(&b.0.entry.name))
                .then_with(|| a.2.path.cmp(&b.2.path))
                .then_with(|| a.2.lines.start().cmp(&b.2.lines.start()))
        });
        if let Some(best) = found.first().map(|f| f.3) {
            // Only the strongest matches: a qualified exact match hides
            // case-insensitive short-name matches.
            found.retain(|f| f.3 >= best.min(3));
        }
        Ok(found)
    }

    async fn inspect_compiler_available(
        &self,
        project: &PinnedProject,
        snapshot: &Snapshot,
        path: &RepoPath,
        cache: &mut BTreeMap<RepoPath, bool>,
    ) -> Result<bool, ToolError> {
        if project.overlay.is_some() {
            return Ok(false);
        }
        if let Some(available) = cache.get(path) {
            return Ok(*available);
        }
        let Some(file) = snapshot.file(path) else {
            return Ok(false);
        };
        let Some(commit) = project.commit.as_deref() else {
            return Ok(false);
        };
        let mut conn = self.inner.store.acquire().await.map_err(store_tool)?;
        let coverage = knowell_store::analysis::coverage_at(
            &mut conn,
            self.inner.organization,
            project.pin(),
            path,
            &file.content_hash,
        )
        .await
        .map_err(store_tool)?;
        let available = coverage.iter().any(|record| {
            crate::precise::admits_scip_coverage(record, project.pin(), commit, file.content_hash)
        });
        cache.insert(path.clone(), available);
        Ok(available)
    }

    async fn inspect_symbol_links(
        &self,
        project: &PinnedProject,
        snapshot: &Snapshot,
        symbol: &SymbolEntry,
        input: &InspectSymbolInput,
        gaps: &mut Vec<Gap>,
    ) -> Result<InspectLinks, ToolError> {
        let wants = |facet: SymbolFacet| input.include.is_empty() || input.include.contains(&facet);
        let mut cache = BTreeMap::new();
        let compiler_available = self
            .inspect_compiler_available(project, snapshot, &symbol.path, &mut cache)
            .await?;
        let mut output = InspectLinks {
            references: Vec::new(),
            implementations: Vec::new(),
            tests: Vec::new(),
            compiler_available,
        };
        if !wants(SymbolFacet::References)
            && !wants(SymbolFacet::Implementations)
            && !wants(SymbolFacet::Tests)
        {
            return Ok(output);
        }
        // Index import spans once; checking each occurrence against the entire
        // project's import list would make test association work quadratic.
        let mut test_import_ranges: BTreeMap<RepoPath, Vec<LineRange>> = BTreeMap::new();
        if wants(SymbolFacet::Tests) {
            for import in &snapshot.imports {
                if is_test_path(&import.from)
                    && let Some(lines) = import.lines
                {
                    test_import_ranges
                        .entry(import.from.clone())
                        .or_default()
                        .push(lines);
                }
            }
        }
        let mut acquisition_truncated = false;
        if let Some(id) = symbol.store_id {
            let edges = snapshot
                .edges_into
                .get(&id)
                .map(Vec::as_slice)
                .unwrap_or_default();
            acquisition_truncated |= edges.len() > INSPECT_EDGE_LIMIT;
            for edge in edges
                .iter()
                .take(INSPECT_EDGE_LIMIT)
                .filter_map(|index| snapshot.other_edges.get(*index))
            {
                let Some(relation) = inspect_relation(&edge.kind) else {
                    continue;
                };
                if relation == RelationKind::Implements && !wants(SymbolFacet::Implementations) {
                    continue;
                }
                if edge.evidence == knowell_store::EvidenceType::SemanticResolved
                    && !self
                        .inspect_compiler_available(project, snapshot, &edge.origin, &mut cache)
                        .await?
                {
                    continue;
                }
                let Some(file) = snapshot.file(&edge.origin) else {
                    continue;
                };
                let Some(lines) = edge.lines.or_else(|| whole_file(file.line_count)) else {
                    continue;
                };
                let Some(link) = inspect_link(
                    project,
                    snapshot,
                    &edge.origin,
                    lines,
                    relation,
                    inspect_evidence(edge.evidence),
                    inspect_resolution(edge.resolution),
                )?
                else {
                    continue;
                };
                match relation {
                    RelationKind::Implements => output.implementations.push(link),
                    RelationKind::Tests => {
                        if wants(SymbolFacet::Tests) {
                            output.tests.push(link);
                        }
                    }
                    _ => {
                        if wants(SymbolFacet::Tests)
                            && inspect_test_association(
                                &test_import_ranges,
                                &edge.origin,
                                lines,
                                relation,
                            )
                        {
                            output.tests.push(link.clone());
                        }
                        if wants(SymbolFacet::References) {
                            output.references.push(link);
                        }
                    }
                }
            }

            let mut after = None;
            let mut scanned = 0usize;
            loop {
                let remaining = INSPECT_OCCURRENCE_LIMIT
                    .saturating_sub(scanned)
                    .saturating_add(1);
                let page_rows =
                    INSPECT_PAGE_ROWS.min(u32::try_from(remaining).unwrap_or(INSPECT_PAGE_ROWS));
                let mut conn = self.inner.store.acquire().await.map_err(store_tool)?;
                let page = stored_symbols::occurrences_of_page(
                    &mut conn,
                    self.inner.organization,
                    id,
                    &[project.pin()],
                    after,
                    page_rows,
                )
                .await
                .map_err(store_tool)?;
                drop(conn);
                if page.is_empty() {
                    break;
                }
                after = page.last().map(|row| row.id);
                let exhausted = page.len() < usize::try_from(page_rows).unwrap_or(usize::MAX);
                for row in page {
                    scanned = scanned.saturating_add(1);
                    if scanned > INSPECT_OCCURRENCE_LIMIT {
                        acquisition_truncated = true;
                        break;
                    }
                    if row.occurrence.role != OccurrenceRole::Reference {
                        continue;
                    }
                    let occurrence = &row.occurrence;
                    let Some(file) = snapshot.file(&occurrence.path) else {
                        continue;
                    };
                    if file.content_hash != occurrence.content_hash {
                        continue;
                    }
                    let (evidence, resolution) = if row.origin == "syntax" {
                        // Stored syntax occurrences do not carry a compiler binding proof.
                        (EvidenceType::SyntacticObservation, Resolution::Unresolved)
                    } else if row.origin == format!("scip:{}", occurrence.path)
                        && row.valid_from == project.generation
                        && self
                            .inspect_compiler_available(
                                project,
                                snapshot,
                                &occurrence.path,
                                &mut cache,
                            )
                            .await?
                    {
                        (EvidenceType::SemanticallyResolved, Resolution::Resolved)
                    } else {
                        continue;
                    };
                    let Some(link) = inspect_link(
                        project,
                        snapshot,
                        &occurrence.path,
                        occurrence.lines,
                        RelationKind::References,
                        evidence,
                        resolution,
                    )?
                    else {
                        continue;
                    };
                    if wants(SymbolFacet::Tests)
                        && inspect_test_association(
                            &test_import_ranges,
                            &occurrence.path,
                            occurrence.lines,
                            RelationKind::References,
                        )
                    {
                        output.tests.push(link.clone());
                    }
                    if wants(SymbolFacet::References) {
                        output.references.push(link);
                    }
                }
                if exhausted || scanned > INSPECT_OCCURRENCE_LIMIT {
                    break;
                }
            }
        }
        if wants(SymbolFacet::References) {
            let mut import_count = 0usize;
            for edge in snapshot.imports_of(&symbol.path) {
                import_count = import_count.saturating_add(1);
                if import_count > INSPECT_EDGE_LIMIT {
                    acquisition_truncated = true;
                    break;
                }
                if edge.evidence == knowell_store::EvidenceType::SemanticResolved
                    && !self
                        .inspect_compiler_available(project, snapshot, &edge.from, &mut cache)
                        .await?
                {
                    continue;
                }
                let Some(file) = snapshot.file(&edge.from) else {
                    continue;
                };
                let Some(lines) = edge.lines.or_else(|| whole_file(file.line_count)) else {
                    continue;
                };
                if let Some(link) = inspect_link(
                    project,
                    snapshot,
                    &edge.from,
                    lines,
                    RelationKind::Imports,
                    inspect_evidence(edge.evidence),
                    inspect_resolution(edge.resolution),
                )? {
                    // A test importing the module is not proof it exercises this symbol.
                    output.references.push(link);
                }
            }
        }
        if acquisition_truncated {
            gaps.push(Gap::for_project(GapReason::LimitReached, project.entry.name.clone(),
                "symbol link acquisition reached its bounded occurrence or edge limit; the returned lists are partial"));
        }
        let limit = usize::try_from(input.limit.unwrap_or(20)).unwrap_or(20);
        finish_inspect_links(
            &mut output.references,
            limit,
            "reference",
            &project.entry.name,
            gaps,
        );
        finish_inspect_links(
            &mut output.implementations,
            limit,
            "implementation",
            &project.entry.name,
            gaps,
        );
        finish_inspect_links(
            &mut output.tests,
            limit,
            "test association",
            &project.entry.name,
            gaps,
        );
        let precise_files = cache.values().filter(|available| **available).count();
        gaps.push(Gap::for_project(
            if precise_files > 0 { GapReason::RelationsNotReady } else { GapReason::NoReferenceResolutionForLanguage },
            project.entry.name.clone(),
            if project.overlay.is_some() {
                "compiler links were excluded because this personal context has no matching compiler-input analysis; unchanged file text does not validate changed dependency inputs".to_owned()
            } else {
                format!("reference, call and implementation inventory is partial: {precise_files} inspected files have exact compiler coverage; source observations and file-import navigation do not establish an exhaustive inventory")
            },
        ));
        Ok(output)
    }

    pub(crate) async fn tool_inspect_symbol(
        &self,
        access: Access,
        input: InspectSymbolInput,
    ) -> Result<InspectSymbolOutput, ToolError> {
        let pinned = self.resolve_target(&access, &input.target).await?;
        if let Some(project) = &input.symbol.project
            && !pinned.projects.contains_key(project)
            && !pinned.not_indexed.contains(project)
        {
            return Err(ToolError::not_found(format!(
                "project {project} does not exist"
            )));
        }
        let wants = |facet: SymbolFacet| input.include.is_empty() || input.include.contains(&facet);
        let found = self
            .find_symbols(
                &pinned,
                input.symbol.id.as_ref(),
                input.symbol.symbol.as_deref(),
                input.symbol.project.as_ref(),
            )
            .await?;
        let mut gaps = pinned.gaps.clone();
        let mut symbols = Vec::new();
        if input.symbol.id.is_some() && found.len() > 1 {
            gaps.push(Gap::new(GapReason::LimitReached,
                format!("{} definitions share this source anchor; a source id identifies lines, not a unique symbol; choose a qualified symbol and project", found.len())));
        }
        if found.len() > 10 {
            gaps.push(Gap::new(GapReason::LimitReached,
                format!("{} matching symbols were omitted by the symbol acquisition limit; use a project or qualified symbol", found.len().saturating_sub(10))));
        }
        for (project, snapshot, symbol, _) in found.into_iter().take(10) {
            let Some(file) = snapshot.file(&symbol.path) else {
                continue;
            };
            let language = file.language.clone().unwrap_or_else(|| "text".to_owned());
            let Some(commit) = project.commit_id() else {
                gaps.push(no_commit_gap(&project.entry.name));
                continue;
            };
            if project.overlay.as_ref().is_some_and(|overlay| {
                overlay.overlay.file(&symbol.path).is_some()
                    || overlay.overlay.deleted().contains(&symbol.path)
            }) {
                gaps.push(Gap::for_project(GapReason::NotFound, project.entry.name.clone(),
                    "the indexed definition is shadowed by personal source changes; search and read the personal definition before inspecting shared symbol links"));
                continue;
            }
            if let Some(source) = input.symbol.id.as_ref().and_then(parse_source_id)
                && (!source.names_commit(Some(commit.as_str()))
                    || !source.names_version(&file.content_hash)
                    || self
                        .retained_commit_collision(&project, &source, &symbol.path, commit.as_str())
                        .await?)
            {
                gaps.push(Gap::for_project(GapReason::NotFound, project.entry.name.clone(),
                    "the symbol id does not identify an unambiguous source version in this pinned context; fetch the retained source or inspect an explicit pinned symbol"));
                continue;
            }
            let id = source_id(
                &project.entry.name,
                Some(commit.as_str()),
                &file.content_hash,
                &symbol.path,
                symbol.lines,
            )?;
            let definition = knowell_mcp::Evidence {
                project: project.entry.name.clone(),
                view: project.target.clone(),
                layer: knowell_mcp::ViewLayer::Shared,
                commit,
                path: symbol.path.clone(),
                lines: symbol.lines,
                content_hash: file.content_hash,
                symbol: Some(symbol.local.clone()),
                why: vec![MatchReason::ExactSymbol {
                    symbol: symbol.local.clone(),
                }],
                freshness: FreshnessTier::T1Symbols,
                index_state: project.index_state(),
            };
            let links = self
                .inspect_symbol_links(&project, &snapshot, &symbol, &input, &mut gaps)
                .await?;
            symbols.push(SymbolInfo {
                id,
                name: symbol.name.clone(),
                qualified_name: symbol.local.clone(),
                kind: symbol_kind(symbol.kind),
                analysis: if links.compiler_available {
                    AnalysisLevel::Semantic
                } else {
                    analysis_level(&language)
                },
                language,
                definition,
                signature: wants(SymbolFacet::Signature)
                    .then(|| UntrustedText::repository(symbol.signature.clone())),
                doc: if wants(SymbolFacet::Signature) {
                    symbol.doc.clone().map(UntrustedText::repository)
                } else {
                    None
                },
                references: if wants(SymbolFacet::References) {
                    links.references
                } else {
                    Vec::new()
                },
                implementations: links.implementations,
                tests: if wants(SymbolFacet::Tests) {
                    links.tests
                } else {
                    Vec::new()
                },
                // Current syntax and precise providers report partial inventories.
                // A bounded or facet-filtered result never proves complete references.
                references_complete: false,
            });
        }
        if symbols.is_empty() {
            gaps.extend(pinned.not_indexed_gaps(&[]));
            gaps.push(Gap::new(
                GapReason::NoCandidatesInSelectedRef,
                "no symbol with this name or id in the pinned views",
            ));
        }
        dedupe(&mut gaps);
        Ok(InspectSymbolOutput { symbols, gaps })
    }
}

/// Removes repeated gaps, keeping the first.
pub(crate) fn dedupe(gaps: &mut Vec<Gap>) {
    let mut seen = Vec::new();
    gaps.retain(|g| {
        if seen.contains(g) {
            false
        } else {
            seen.push(g.clone());
            true
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn historical_fixture() -> (SourceRef, content::FileVersion, views::ViewGeneration) {
        let view = knowell_store::ViewId(uuid::Uuid::from_u128(1));
        let hash = knowell_core::ContentHash::of(b"old source");
        let project = Name::new("synthetic").unwrap();
        let path = RepoPath::new("src/history.rs").unwrap();
        let commit = "a".repeat(40);
        let id = source_id(
            &project,
            Some(&commit),
            &hash,
            &path,
            LineRange::new(1, 3).unwrap(),
        )
        .unwrap();
        let source = parse_source_id(&id).unwrap();
        let version = content::FileVersion {
            view,
            path,
            content_hash: hash,
            language: Some("rust".to_owned()),
            renamed_from: None,
            valid_from: 2,
            valid_to: Some(4),
        };
        let generation = views::ViewGeneration {
            view,
            generation: 3,
            resolved_commit: Some(commit),
            state: GenerationState::Retired,
            error: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            activated_at: Some(time::OffsetDateTime::UNIX_EPOCH),
            finished_at: Some(time::OffsetDateTime::UNIX_EPOCH),
        };
        (source, version, generation)
    }

    #[test]
    fn historical_source_requires_retained_commit_and_valid_file_interval() {
        let (source, version, generation) = historical_fixture();
        let history = vec![version.clone()];
        let generations = vec![generation.clone()];
        let (found, pin) = historical_source(&source, &history, &generations).unwrap();
        assert_eq!(found.content_hash, version.content_hash);
        assert_eq!(pin.resolved_commit, generation.resolved_commit);
        assert!(historical_source(&source, &history, &[]).is_none());

        for number in [1, 4] {
            let mut outside = generation.clone();
            outside.generation = number;
            assert!(historical_source(&source, &history, &[outside]).is_none());
        }
        let mut different = generation.clone();
        different.resolved_commit = Some("b".repeat(40));
        assert!(historical_source(&source, &history, &[different]).is_none());
        let mut building = generation.clone();
        building.state = GenerationState::Building;
        assert!(historical_source(&source, &history, &[building]).is_none());
        let mut different_body = version;
        different_body.content_hash = knowell_core::ContentHash::of(b"current source");
        assert!(historical_source(&source, &[different_body], &generations).is_none());
    }

    #[test]
    fn historical_source_rejects_commit_prefix_collision() {
        let (source, version, generation) = historical_fixture();
        let mut collision = generation.clone();
        collision.resolved_commit = Some(format!("{}{}", "a".repeat(12), "b".repeat(28)));
        let generations = vec![generation, collision];
        assert!(historical_source(&source, &[version], &generations).is_none());
    }

    #[test]
    fn historical_source_reuses_same_commit_across_retained_generations() {
        let (source, version, generation) = historical_fixture();
        let mut earlier = generation.clone();
        earlier.generation = 2;
        let history = vec![version];
        let generations = vec![earlier, generation];
        let (_, pin) = historical_source(&source, &history, &generations).unwrap();
        assert_eq!(pin.generation, 3);
    }

    #[test]
    fn widen_and_cap_stay_in_bounds() {
        let r = LineRange::new(5, 8).unwrap();
        assert_eq!(widen(r, 10, 20), LineRange::new(1, 18).unwrap());
        assert_eq!(widen(r, 2, 9), LineRange::new(3, 9).unwrap());
        let (c, cut) = cap(LineRange::new(1, 100).unwrap(), 10);
        assert!(cut);
        assert_eq!(c, LineRange::new(1, 10).unwrap());
        assert!(!cap(r, 10).1);
    }

    #[test]
    fn optional_fetch_context_cannot_hide_the_requested_continuation_start() {
        let requested = LineRange::new(11, 100).unwrap();
        let widened = widen_for_fetch(requested, 200, 100, 10);
        assert_eq!(widened, requested);
        assert_eq!(cap(widened, 10).0, LineRange::new(11, 20).unwrap());
        let short = LineRange::new(15, 16).unwrap();
        let widened = widen_for_fetch(short, 200, 100, 10);
        assert!(widened.start() <= short.start() && short.end() <= widened.end());
        assert!(widened.line_count() <= 10);
    }

    #[test]
    fn symbol_kinds_map() {
        assert_eq!(symbol_kind(ParseKind::Method), SymbolKind::Method);
        assert_eq!(symbol_kind(ParseKind::Heading), SymbolKind::Other);
    }
}
