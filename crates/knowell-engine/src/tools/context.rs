//! `build_context` and `history`.

use std::collections::{BTreeMap, BTreeSet};

use knowell_core::RepoPath;
use knowell_knowledge::RecordState;
use knowell_mcp::tools::{
    BuildContextInput, BuildContextOutput, CoChange, ContextEntry, ContextRoadmapStep, ContextRole,
    ContextSection, ContextSelectionReport, ContextSelectionStrategy, EntryKind, HistoryFacet,
    HistoryInput, HistoryOutput, ScopeLevel, TokenBudget,
};
use knowell_mcp::{FreshnessTier, Gap, GapReason, ResultId, ToolError, UntrustedText};
use knowell_query::{OmitReason, Origin, PackItem, Reason, SnippetKind, Uncertainty};
use knowell_store::content;
use knowell_store::views::{self, GenerationPin};

use super::code::dedupe;
use super::memory::{conflict_map, memory_error};
use crate::access::Access;
use crate::engine::Engine;
use crate::error::store_tool;
use crate::evidence::{
    degradation_gaps, empty_gaps, hit_kind, place, reasons, source_omission_gaps,
};
use crate::memory::RecordQuery;
use crate::search::{Filters, SnippetAdapter, is_test_path, snippets, source_snippets};

/// Estimated tokens of `text` (four bytes per token, at least one).
fn tokens(text: &str) -> u32 {
    u32::try_from(text.len().div_ceil(4))
        .unwrap_or(u32::MAX)
        .max(1)
}

/// A short, human explanation of why an item is in the pack.
fn why_relevant(why: &[Reason]) -> String {
    let mut parts = Vec::new();
    for reason in why {
        let text = match reason {
            Reason::ExactMatch { term, .. } => format!("names `{term}` exactly"),
            Reason::LexicalTerms { terms } => format!("matches the words {}", terms.join(", ")),
            Reason::GlossaryExpansion {
                matched, expansion, ..
            } => {
                format!("`{matched}` is a glossary synonym of `{expansion}`")
            }
            Reason::SemanticSimilarity { profile, .. } => {
                format!("similar in meaning ({profile})")
            }
            Reason::GraphPath { steps, .. } => match steps.last() {
                Some(step) => format!("{:?} of a top result ({} hop(s))", step.edge, steps.len())
                    .to_lowercase(),
                None => "related to a top result".to_owned(),
            },
            Reason::TestReferences { subject } => format!("a test of {subject}"),
            Reason::PersonalOverlay { .. } => "from your worktree changes".to_owned(),
        };
        if !parts.contains(&text) {
            parts.push(text);
        }
    }
    if parts.is_empty() {
        "related to the task".to_owned()
    } else {
        parts.join("; ")
    }
}

fn uncertainty_text(u: &Uncertainty) -> String {
    match u {
        Uncertainty::Degraded { degradation } => {
            format!("missing part of the search: {degradation}")
        }
        Uncertainty::NoReferenceResolution { project, language } => format!(
            "{project}: {language} references are not resolved; shown structural relations do not prove call or test coverage"
        ),
        Uncertainty::WeakGraphEvidence {
            location,
            evidence,
            resolution,
            ..
        } => format!(
            "{}:{} is linked by {:?} / {:?} evidence",
            location.project, location.path, evidence, resolution
        )
        .to_lowercase(),
        Uncertainty::ExpansionBudgetExhausted { node_budget } => {
            format!("graph expansion stopped at {node_budget} related items; more exist")
        }
        Uncertainty::MoreResults { truncated } => {
            format!("{truncated} more matching results were not considered")
        }
        Uncertainty::NoResults { explanation } => explanation.note.clone(),
    }
}

/// Generic language capability gaps do not qualify an unrelated source body.
/// Concrete weak relations remain visible only when their source is emitted.
fn source_uncertainties(uncertainties: &[Uncertainty], items: &[PackItem]) -> Vec<String> {
    let same_source = |item: &PackItem, location: &knowell_query::Location| {
        item.citation.project == location.project
            && item.citation.view == location.view
            && item.citation.generation == location.generation
            && item.citation.path == location.path
            && item.citation.content_hash == location.content_hash
    };
    let mut result = Vec::new();
    let mut unresolved = false;
    for uncertainty in uncertainties {
        match uncertainty {
            Uncertainty::NoReferenceResolution { project, .. } => {
                unresolved |= items.iter().any(|item| item.citation.project == *project
                    && item.why.iter().any(|reason| matches!(reason,
                        Reason::GraphPath { steps, .. } if steps.iter().any(|step|
                            matches!(step.edge, knowell_query::EdgeKind::Caller
                                | knowell_query::EdgeKind::Callee | knowell_query::EdgeKind::Test)
                                && step.evidence != knowell_query::EvidenceType::SemanticallyResolved
                        ))));
            }
            Uncertainty::WeakGraphEvidence {
                location,
                edge,
                evidence,
                resolution,
            } if !items.iter().any(|item| {
                same_source(item, location)
                    && item.why.iter().any(|reason| {
                        matches!(reason,
                        Reason::GraphPath { steps, .. } if steps.last()
                            .is_some_and(|step| step.to == *location)
                            && steps.iter().any(|step| step.edge == *edge
                                && step.evidence == *evidence && step.resolution == *resolution))
                    })
            }) => {}
            _ => {
                let message = uncertainty_text(uncertainty);
                if !result.contains(&message) {
                    result.push(message);
                }
            }
        }
    }
    if unresolved {
        result.push("selected call/test links lack compiler reference resolution; the shown source does not prove complete call or test coverage".to_owned());
    }
    result
}

fn acquisition_extent(
    run: &crate::search::SearchRun,
    location: &knowell_query::Location,
    symbol: Option<&str>,
    source: Option<&SnippetAdapter<'_>>,
) -> Option<knowell_core::LineRange> {
    source
        .and_then(|adapter| adapter.source_extent(location, symbol))
        .or_else(|| {
            let symbol = symbol?;
            let (view, overlay) = run.prepared.view_of(&location.project, &location.view)?;
            let entry = if overlay {
                view.overlay
                    .as_ref()?
                    .symbols
                    .iter()
                    .find(|entry| entry.path == location.path && entry.local == symbol)
            } else {
                view.snapshot.symbol_by_local(&location.path, symbol)
            }?;
            location
                .range
                .is_none_or(|range| {
                    entry.lines.start() <= range.start() && range.end() <= entry.lines.end()
                })
                .then_some(entry.lines)
        })
        .or(location.range)
        .or_else(|| {
            run.prepared
                .view_of(&location.project, &location.view)
                .and_then(|(view, overlay)| {
                    if overlay {
                        view.overlay
                            .as_ref()?
                            .overlay
                            .file(&location.path)
                            .and_then(|file| {
                                crate::snapshot::whole_file(crate::snapshot::line_count(&file.text))
                            })
                    } else {
                        view.snapshot
                            .file(&location.path)
                            .and_then(|file| crate::snapshot::whole_file(file.line_count))
                    }
                })
        })
}

fn query_role(role: ContextRole) -> knowell_query::EvidenceRole {
    use knowell_query::EvidenceRole as R;
    match role {
        ContextRole::Entry => R::Entry,
        ContextRole::Implementation => R::Implementation,
        ContextRole::Caller => R::Caller,
        ContextRole::Callee => R::Callee,
        ContextRole::Test => R::Test,
        ContextRole::Config => R::Config,
        ContextRole::Contract => R::Contract,
        ContextRole::Doc => R::Doc,
        ContextRole::Surroundings => R::Surroundings,
    }
}

fn mcp_role(role: knowell_query::EvidenceRole) -> ContextRole {
    use knowell_query::EvidenceRole as R;
    match role {
        R::Entry => ContextRole::Entry,
        R::Implementation => ContextRole::Implementation,
        R::Caller => ContextRole::Caller,
        R::Callee => ContextRole::Callee,
        R::Test => ContextRole::Test,
        R::Config => ContextRole::Config,
        R::Contract => ContextRole::Contract,
        R::Doc => ContextRole::Doc,
        R::Surroundings => ContextRole::Surroundings,
    }
}

fn query_strategy(strategy: ContextSelectionStrategy) -> knowell_query::TaskSelectionStrategy {
    use knowell_query::TaskSelectionStrategy as S;
    match strategy {
        ContextSelectionStrategy::Source => S::Source,
        ContextSelectionStrategy::Rank => S::Rank,
        ContextSelectionStrategy::Mmr => S::Mmr,
        ContextSelectionStrategy::RoleCoverage => S::RoleCoverage,
        ContextSelectionStrategy::BoundedBundles => S::BoundedBundles,
    }
}

fn pack_source_matches(
    item: &PackItem,
    location: &knowell_query::Location,
    commit: Option<&knowell_query::CommitId>,
    symbol: Option<&str>,
    why: &[Reason],
    extent: Option<knowell_core::LineRange>,
) -> bool {
    let citation = &item.citation;
    location.project == citation.project
        && location.path == citation.path
        && location.view == citation.view
        && location.generation == citation.generation
        && location.content_hash == citation.content_hash
        && commit == citation.commit.as_ref()
        && symbol == item.symbol.as_deref()
        && why == item.why
        && extent.or(location.range).is_some_and(|range| {
            range.start() <= citation.range.start() && citation.range.end() <= range.end()
        })
    // Bodies may be shortened by the snippet limit. A symbol's skeleton
    // can precede a retrieved body chunk, so its original reasons bind it.
    // Source regions may include a surrounding guard outside the retrieved
    // chunk. Identity, origin and reasons bind it to this exact occurrence;
    // the emitted citation independently records its actual source range.
}

fn expanded_pack_source<'a>(
    expanded: &'a [knowell_query::ExpandedItem],
    item: &PackItem,
    seed_rank: u32,
    depth: u32,
    extent: &dyn Fn(&knowell_query::Location, Option<&str>) -> Option<knowell_core::LineRange>,
) -> Option<&'a knowell_query::ExpandedItem> {
    expanded.iter().find(|source| {
        source.seed_rank == seed_rank
            && source.depth == depth
            && pack_source_matches(
                item,
                &source.location,
                source.commit.as_ref(),
                source.symbol.as_deref(),
                &source.why,
                extent(&source.location, source.symbol.as_deref()),
            )
    })
}

fn source_section(
    run: &crate::search::SearchRun,
    location: &knowell_query::Location,
    why: &[Reason],
) -> ContextSection {
    let language = run
        .prepared
        .view_of(&location.project, &location.view)
        .and_then(|(view, overlay)| {
            if overlay {
                view.overlay
                    .as_ref()
                    .and_then(|o| o.overlay.file(&location.path))
                    .map(|file| file.parsed.language.as_str())
            } else {
                view.snapshot
                    .file(&location.path)
                    .and_then(|file| file.language.as_deref())
            }
        });
    let mapped = reasons(why, None, None, location);
    match hit_kind(&location.path, language, &mapped) {
        knowell_mcp::tools::HitKind::Test => ContextSection::Tests,
        knowell_mcp::tools::HitKind::Doc => ContextSection::Docs,
        knowell_mcp::tools::HitKind::Contract => ContextSection::Contracts,
        _ => ContextSection::Code,
    }
}

impl Engine {
    pub(crate) async fn tool_build_context(
        &self,
        access: Access,
        input: BuildContextInput,
    ) -> Result<BuildContextOutput, ToolError> {
        let pinned = self.resolve_target(&access, &input.target).await?;
        if let Some(job) = &input.job_id {
            return Err(ToolError::not_found(format!(
                "context job {job} does not exist; packs are built synchronously"
            )));
        }
        let Some(task) = input.task.as_deref() else {
            return Err(ToolError::invalid_input("pass `task`"));
        };
        let requested = input.token_budget.unwrap_or(8000);
        let wants =
            |section: ContextSection| input.include.is_empty() || input.include.contains(&section);
        let mut gaps = pinned.gaps.clone();
        gaps.extend(pinned.not_indexed_gaps(&[]));
        let mut entries = Vec::new();
        let mut used: u32 = 0;
        let mut uncertainties = Vec::new();
        // 1. Focus paths the caller named: they come first, as given.
        for locator in &input.focus_paths {
            let Some(project) = pinned.projects.get(&locator.project) else {
                gaps.push(Gap::for_project(
                    GapReason::NotFound,
                    locator.project.clone(),
                    format!("{} is not available in this context", locator.project),
                ));
                continue;
            };
            let snapshot = self.metadata_snapshot_of(project).await?;
            let Some(file) = snapshot.file(&locator.path).cloned() else {
                gaps.push(Gap::for_project(
                    GapReason::NotFound,
                    locator.project.clone(),
                    format!("{} does not exist in the pinned view", locator.path),
                ));
                continue;
            };
            let Some(text) = self
                .inner
                .texts
                .load(
                    &self.inner.store,
                    self.inner.organization,
                    &file.content_hash,
                )
                .await
                .map_err(ToolError::from)?
            else {
                continue;
            };
            let total = crate::snapshot::line_count(&text);
            let Some(lines) = locator.lines.or_else(|| crate::snapshot::whole_file(total)) else {
                continue;
            };
            if lines.start() > total || lines.end() > total {
                gaps.push(Gap::for_project(
                    GapReason::NotFound,
                    locator.project.clone(),
                    format!("{}:{lines} is outside this source version", locator.path),
                ));
                continue;
            }
            let body = crate::source_region::slice_source_lines(&text, lines);
            let cost = tokens(&body);
            if used.saturating_add(cost) > requested {
                gaps.push(Gap::for_project(
                    GapReason::BudgetExhausted,
                    locator.project.clone(),
                    format!(
                        "{}:{lines} needs {cost} tokens; the budget has {} left",
                        locator.path,
                        requested.saturating_sub(used)
                    ),
                ));
                continue;
            }
            let Some(commit) = project.commit_id() else {
                continue;
            };
            let id = crate::ids::source_id(
                &locator.project,
                Some(commit.as_str()),
                &file.content_hash,
                &locator.path,
                lines,
            )?;
            used = used.saturating_add(cost);
            entries.push(ContextEntry {
                id,
                section: if is_test_path(&locator.path) {
                    ContextSection::Tests
                } else {
                    ContextSection::Code
                },
                kind: EntryKind::Code,
                why_relevant: "you named this file".to_owned(),
                evidence: Some(knowell_mcp::Evidence {
                    project: locator.project.clone(),
                    view: project.target.clone(),
                    layer: knowell_mcp::ViewLayer::Shared,
                    commit,
                    path: locator.path.clone(),
                    lines,
                    content_hash: file.content_hash,
                    symbol: None,
                    why: Vec::new(),
                    freshness: FreshnessTier::T0Text,
                    index_state: project.index_state(),
                }),
                memory_id: None,
                content: UntrustedText::repository(body),
                content_lines: Some(lines),
                content_truncated: false,
                continuation_ids: Vec::new(),
                estimated_tokens: cost,
            });
        }
        // 2. Accepted rules and decisions about the task (small, high value).
        if wants(ContextSection::Rules) || wants(ContextSection::Memory) {
            let scopes = self.readable_scopes(&access, &pinned);
            let query = RecordQuery {
                scopes,
                states: vec![RecordState::Accepted],
                kinds: Vec::new(),
                text: Some(task.chars().take(200).collect()),
                limit: 5,
            };
            let records: Vec<_> = match self.inner.memory.find_records(&query).await {
                Ok(rows) => rows.into_iter().map(|r| r.record).collect(),
                Err(error) => {
                    tracing::debug!(error = %error, "memory lookup for a context pack failed");
                    Vec::new()
                }
            };
            let conflicts = conflict_map(&records);
            for record in &records {
                let is_rule = record.is_rule();
                if (is_rule && !wants(ContextSection::Rules))
                    || (!is_rule && !wants(ContextSection::Memory))
                {
                    continue;
                }
                let text = format!("{}\n\n{}", record.title, record.body);
                let cost = tokens(&text);
                if used.saturating_add(cost) > requested / 4 {
                    continue;
                }
                used = used.saturating_add(cost);
                let empty = Vec::new();
                let mcp = self
                    .visible_memory_record(
                        record,
                        conflicts.get(&record.id).unwrap_or(&empty),
                        &access,
                        &pinned,
                        &mut gaps,
                    )
                    .await?;
                entries.push(ContextEntry {
                    id: ResultId::new(format!("kn-memory:{}", record.id))
                        .map_err(|e| ToolError::internal(e.to_string()))?,
                    section: if is_rule {
                        ContextSection::Rules
                    } else {
                        ContextSection::Memory
                    },
                    kind: if is_rule {
                        EntryKind::Rule
                    } else {
                        EntryKind::Decision
                    },
                    why_relevant: "accepted team knowledge that mentions the task's words"
                        .to_owned(),
                    evidence: None,
                    memory_id: Some(mcp.id.clone()),
                    content: UntrustedText::memory(text),
                    content_lines: None,
                    content_truncated: false,
                    continuation_ids: Vec::new(),
                    estimated_tokens: cost,
                });
            }
        }
        let source_sections: Vec<_> = [
            ContextSection::Code,
            ContextSection::Tests,
            ContextSection::Contracts,
            ContextSection::Docs,
        ]
        .into_iter()
        .filter(|section| wants(*section))
        .collect();
        if source_sections.is_empty() {
            if entries.is_empty() && gaps.is_empty() {
                gaps.push(Gap::new(
                    GapReason::NoMatches,
                    "no matching accepted team knowledge was found",
                ));
            }
            dedupe(&mut gaps);
            let selection = input
                .selection_strategy
                .map(|strategy| ContextSelectionReport {
                    strategy,
                    considered_candidates: 0,
                    omitted_by_candidate_limit: 0,
                    evaluations: 0,
                    evaluation_budget_exhausted: false,
                    selected_candidates: 0,
                    covered_roles: Vec::new(),
                    missing_roles: if input.desired_roles.is_empty() {
                        knowell_query::TaskPackOptions::default()
                            .desired_roles
                            .into_iter()
                            .map(mcp_role)
                            .collect()
                    } else {
                        input.desired_roles.clone()
                    },
                    steps: Vec::new(),
                });
            return Ok(BuildContextOutput {
                entries,
                budget: TokenBudget { requested, used },
                uncertainties,
                job: None,
                gaps,
                selection,
            });
        }
        // 3. Code found for the task, packed into what is left.
        let mut query = task.to_owned();
        for symbol in &input.focus_symbols {
            query.push(' ');
            query.push_str(symbol);
        }
        let filters = Filters {
            sections: source_sections,
            ..Filters::default()
        };
        let strategy = input
            .selection_strategy
            .unwrap_or(ContextSelectionStrategy::Source);
        let mut run = if strategy == ContextSelectionStrategy::Source {
            self.run_source_search(&pinned, &filters, &query, 64)
                .await?
        } else {
            self.run_search(&pinned, &filters, &query, 20, true, false)
                .await?
        };
        // Section filtering precedes budget allocation and source acquisition.
        let result_sections: BTreeMap<_, _> = run
            .response
            .results
            .iter()
            .map(|r| (r.rank, source_section(&run, &r.location, &r.why)))
            .collect();
        let expanded_sections: Vec<_> = run
            .response
            .expanded
            .iter()
            .map(|r| source_section(&run, &r.location, &r.why))
            .collect();
        run.response
            .results
            .retain(|r| result_sections.get(&r.rank).is_some_and(|s| wants(*s)));
        let mut section = expanded_sections.into_iter();
        run.response
            .expanded
            .retain(|_| section.next().is_some_and(&wants));
        let texts = if strategy == ContextSelectionStrategy::Source {
            self.source_texts_for(&run).await?
        } else {
            self.texts_for(&run).await?
        };
        let source = if strategy == ContextSelectionStrategy::Source {
            source_snippets(&run, texts, self.inner.settings.max_fetch_lines)
        } else {
            snippets(&run, texts, self.inner.settings.max_fetch_lines)
        };
        let remaining = requested.saturating_sub(used);
        let (pack, selected) = {
            let mut options = knowell_query::TaskPackOptions {
                strategy: query_strategy(strategy),
                ..knowell_query::TaskPackOptions::default()
            };
            if !input.desired_roles.is_empty() {
                options.desired_roles = input
                    .desired_roles
                    .iter()
                    .copied()
                    .map(query_role)
                    .collect();
            }
            if strategy == ContextSelectionStrategy::Source {
                options.candidate_limit = 64;
                if input.desired_roles.is_empty() {
                    options.desired_roles.clear();
                }
            }
            let selected = if strategy == ContextSelectionStrategy::Source {
                knowell_query::pack_task_with(
                    &run.response,
                    crate::source_region::source_budget(remaining),
                    &source,
                    &crate::source_region::SourceBudgetCounter,
                    &options,
                )
            } else {
                knowell_query::pack_task(&run.response, remaining, &source, &options)
            }
            .map_err(|error| ToolError::internal(error.to_string()))?;
            (selected.pack, Some(selected.selection))
        };
        let mut discarded_sources = 0usize;
        for item in &pack.items {
            let Some(entry) = self.pack_entry(
                &run,
                item,
                &wants,
                (strategy == ContextSelectionStrategy::Source).then_some(&source),
            )?
            else {
                discarded_sources = discarded_sources.saturating_add(1);
                continue;
            };
            used = used.saturating_add(item.tokens);
            entries.push(entry);
        }
        if discarded_sources > 0 {
            gaps.push(Gap::new(GapReason::NotFound, format!(
                "{discarded_sources} selected source regions were omitted because their pinned identity or acquisition extent could not be verified")));
        }
        let over_budget = pack
            .omitted
            .iter()
            .filter(|o| matches!(o.reason, OmitReason::OverBudget { .. }))
            .count();
        if over_budget > 0 {
            gaps.push(Gap::new(
                GapReason::BudgetExhausted,
                format!("{over_budget} relevant items did not fit the token budget"),
            ));
        }
        let acquisition_gaps = source_omission_gaps(&pack.omitted);
        if pack.items.is_empty()
            && !run.response.results.is_empty()
            && acquisition_gaps.is_empty()
            && over_budget == 0
        {
            gaps.push(Gap::new(GapReason::NotFound,
                "retrieval found source candidates, but no source regions were emitted; inspect the pinned index or increase token_budget"));
        }
        gaps.extend(acquisition_gaps);
        let pack_uncertainties = if strategy == ContextSelectionStrategy::Source {
            source_uncertainties(&pack.uncertainties, &pack.items)
        } else {
            pack.uncertainties.iter().map(uncertainty_text).collect()
        };
        for text in pack_uncertainties {
            if !uncertainties.contains(&text) {
                uncertainties.push(text);
            }
        }
        gaps.extend(degradation_gaps(&run.response.degraded));
        if entries.is_empty() {
            gaps.extend(empty_gaps(&run.response));
            if gaps.is_empty() {
                gaps.push(Gap::new(GapReason::NoMatches, knowell_query::ABSENCE_NOTE));
            }
        }
        dedupe(&mut gaps);
        let selection =
            selected.map(|selected| {
                let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
                let steps: Vec<_> = selected
                    .role_evidence
                    .iter()
                    .filter_map(|role| {
                        let all_shown = role.citations.iter().all(|citation| {
                            entries.iter().any(|entry| {
                                !matches!(entry.kind, EntryKind::Signature | EntryKind::Skeleton)
                                    && entry.evidence.as_ref().is_some_and(|evidence| {
                                        citation.project == evidence.project
                                            && citation.path == evidence.path
                                            && citation.range == evidence.lines
                                            && citation.content_hash == evidence.content_hash
                                            && citation.commit.as_ref().is_some_and(|commit| {
                                                commit.as_str() == evidence.commit.as_str()
                                            })
                                    })
                            })
                        });
                        if !all_shown {
                            return None;
                        }
                        let source_ids: Vec<_> = entries
                            .iter()
                            .filter_map(|entry| {
                                let evidence = entry.evidence.as_ref()?;
                                role.citations
                                    .iter()
                                    .any(|citation| {
                                        citation.project == evidence.project
                                            && citation.path == evidence.path
                                            && citation.range == evidence.lines
                                            && citation.content_hash == evidence.content_hash
                                            && citation.commit.as_ref().is_some_and(|commit| {
                                                commit.as_str() == evidence.commit.as_str()
                                            })
                                    })
                                    .then(|| entry.id.clone())
                            })
                            .collect();
                        (!source_ids.is_empty()).then_some(ContextRoadmapStep {
                            role: mcp_role(role.role),
                            source_ids,
                        })
                    })
                    .collect();
                ContextSelectionReport {
                    strategy: input
                        .selection_strategy
                        .unwrap_or(ContextSelectionStrategy::Source),
                    considered_candidates: count(selected.considered_candidates),
                    omitted_by_candidate_limit: count(selected.omitted_by_candidate_limit),
                    evaluations: count(selected.evaluations),
                    evaluation_budget_exhausted: selected.evaluation_budget_exhausted,
                    selected_candidates: count(selected.selected_candidates),
                    covered_roles: selected
                        .covered_roles
                        .iter()
                        .copied()
                        .map(mcp_role)
                        .filter(|role| steps.iter().any(|step| step.role == *role))
                        .collect(),
                    missing_roles: selected
                        .missing_roles
                        .iter()
                        .copied()
                        .chain(
                            selected.covered_roles.iter().copied().filter(|role| {
                                !steps.iter().any(|step| step.role == mcp_role(*role))
                            }),
                        )
                        .map(mcp_role)
                        .collect(),
                    steps,
                }
            });
        Ok(BuildContextOutput {
            entries,
            budget: TokenBudget {
                requested,
                used: used.min(requested),
            },
            uncertainties,
            job: None,
            gaps,
            selection,
        })
    }

    fn pack_entry(
        &self,
        run: &crate::search::SearchRun,
        item: &PackItem,
        wants: &dyn Fn(ContextSection) -> bool,
        source: Option<&SnippetAdapter<'_>>,
    ) -> Result<Option<ContextEntry>, ToolError> {
        let (location, why, score) = match item.origin {
            Origin::Result { rank } => {
                let Some(result) = run.response.results.iter().find(|result| {
                    result.rank == rank
                        && pack_source_matches(
                            item,
                            &result.location,
                            result.commit.as_ref(),
                            result.symbol.as_deref(),
                            &result.why,
                            acquisition_extent(
                                run,
                                &result.location,
                                result.symbol.as_deref(),
                                source,
                            ),
                        )
                }) else {
                    return Ok(None);
                };
                (&result.location, &result.why, Some(&result.score))
            }
            Origin::Expanded { seed_rank, depth } => {
                let Some(expanded) = expanded_pack_source(
                    &run.response.expanded,
                    item,
                    seed_rank,
                    depth,
                    &|location, symbol| acquisition_extent(run, location, symbol, source),
                ) else {
                    return Ok(None);
                };
                (&expanded.location, &expanded.why, None)
            }
        };
        let mut cited = location.clone();
        cited.range = Some(item.citation.range);
        let mapped = reasons(why, score, item.symbol.as_deref(), location);
        let freshness = match score {
            Some(score) => crate::evidence::freshness_of(score, location.range),
            None => FreshnessTier::T1Symbols,
        };
        let Some(placed) = place(
            &run.prepared,
            &cited,
            item.symbol.as_deref(),
            mapped,
            freshness,
        )?
        else {
            return Ok(None);
        };
        let section = source_section(run, location, why);
        if !wants(section) {
            return Ok(None);
        }
        let kind = match (item.kind, section) {
            (SnippetKind::Skeleton, _) if item.symbol.is_some() => EntryKind::Signature,
            (SnippetKind::Skeleton, _) => EntryKind::Skeleton,
            (SnippetKind::Body, ContextSection::Tests) => EntryKind::Test,
            (SnippetKind::Body, ContextSection::Docs) => EntryKind::Doc,
            (SnippetKind::Body, ContextSection::Contracts) => EntryKind::Contract,
            (SnippetKind::Body, _) => EntryKind::Code,
        };
        let extent = acquisition_extent(run, location, item.symbol.as_deref(), source);
        let continuation_ids = if item.kind == SnippetKind::Body {
            match extent {
                Some(extent) => crate::ids::source_continuations(
                    &placed.evidence.project,
                    Some(placed.evidence.commit.as_str()),
                    &placed.evidence.content_hash,
                    &placed.evidence.path,
                    item.citation.range,
                    extent,
                )?,
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };
        Ok(Some(ContextEntry {
            id: placed.id,
            section,
            kind,
            why_relevant: why_relevant(why),
            evidence: Some(placed.evidence),
            memory_id: None,
            content: UntrustedText::repository(item.text.clone()),
            content_lines: (item.kind == SnippetKind::Body).then_some(item.citation.range),
            content_truncated: item.kind == SnippetKind::Body
                && extent.is_some_and(|range| {
                    item.citation.range.start() > range.start()
                        || item.citation.range.end() < range.end()
                }),
            continuation_ids,
            estimated_tokens: tokens(&item.text),
        }))
    }

    pub(crate) async fn tool_history(
        &self,
        access: Access,
        input: HistoryInput,
    ) -> Result<HistoryOutput, ToolError> {
        let pinned = self.resolve_target(&access, &input.target).await?;
        let wants =
            |facet: HistoryFacet| input.include.is_empty() || input.include.contains(&facet);
        let limit = usize::try_from(input.limit.unwrap_or(10)).unwrap_or(10);
        // Resolve the subject to a project file (and symbol).
        let (project_name, path, symbol_key) = if let Some(id) = &input.id {
            let source = crate::ids::parse_source_id(id)
                .ok_or_else(|| ToolError::not_found(format!("result id {id} does not resolve")))?;
            let project = pinned
                .projects
                .get(&source.project)
                .ok_or_else(|| ToolError::not_found(format!("result id {id} does not resolve")))?;
            let snapshot = self.snapshot_of(project).await?;
            let path = snapshot
                .files
                .keys()
                .find(|p| source.path.matches(p))
                .cloned()
                .ok_or_else(|| ToolError::not_found(format!("result id {id} does not resolve")))?;
            let key = snapshot
                .enclosing_symbol(&path, source.lines)
                .map(|s| s.key.clone());
            (source.project, path, key)
        } else {
            let Some(project) = input.project.clone() else {
                return Err(ToolError::invalid_input(
                    "`project` is required with `path` or `symbol`",
                ));
            };
            if !pinned.projects.contains_key(&project) {
                if pinned.not_indexed.contains(&project) {
                    return Ok(HistoryOutput {
                        gaps: pinned.not_indexed_gaps(std::slice::from_ref(&project)),
                        ..HistoryOutput::default()
                    });
                }
                return Err(ToolError::not_found(format!(
                    "project {project} does not exist"
                )));
            }
            match (&input.path, &input.symbol) {
                (Some(path), _) => (project, path.clone(), None),
                (None, Some(symbol)) => {
                    let found = self
                        .find_symbols(&pinned, None, Some(symbol), Some(&project))
                        .await?;
                    let Some((_, _, entry, _)) = found.first() else {
                        return Ok(HistoryOutput {
                            gaps: vec![Gap::for_project(
                                GapReason::NotFound,
                                project.clone(),
                                format!("no symbol {symbol} in {project}"),
                            )],
                            ..HistoryOutput::default()
                        });
                    };
                    (project, entry.path.clone(), Some(entry.key.clone()))
                }
                (None, None) => {
                    return Err(ToolError::invalid_input("pass `path`, `symbol` or `id`"));
                }
            }
        };
        let Some(project) = pinned.projects.get(&project_name) else {
            return Err(ToolError::not_found(format!(
                "project {project_name} does not exist"
            )));
        };
        let mut gaps = Vec::new();
        if wants(HistoryFacet::Commits) || wants(HistoryFacet::Blame) {
            gaps.push(Gap::for_project(
                GapReason::RelationsNotReady,
                project_name.clone(),
                "git commit log and blame are not available yet (knowell-source has no log or blame reader); co-changes come from indexed generations",
            ));
        }
        let mut co_changed = Vec::new();
        if wants(HistoryFacet::CoChanged) {
            match self.co_changes(project, &path, limit).await? {
                Some(list) => co_changed = list,
                None => gaps.push(Gap::for_project(
                    GapReason::NoMatches,
                    project_name.clone(),
                    "fewer than two indexed generations of this file; nothing to compare",
                )),
            }
        }
        let mut rationale = Vec::new();
        if wants(HistoryFacet::Rationale) {
            let scopes = self.readable_scopes(&access, &pinned);
            let rows = self
                .inner
                .memory
                .find_records(&RecordQuery {
                    scopes,
                    states: vec![
                        RecordState::Accepted,
                        RecordState::Stale,
                        RecordState::Proposed,
                    ],
                    kinds: Vec::new(),
                    text: None,
                    limit: 500,
                })
                .await
                .map_err(memory_error)?;
            for row in rows {
                // Names and paths alone do not establish a saved pointer's workspace.
                let record = self
                    .visible_memory_record(&row.record, &[], &access, &pinned, &mut gaps)
                    .await?;
                let cites = record
                    .evidence
                    .iter()
                    .any(|e| e.project == project_name && e.path == path);
                let about = symbol_key
                    .as_ref()
                    .is_some_and(|k| record.related_symbols.iter().any(|s| s.as_str() == k));
                let in_project = record.scope.level == ScopeLevel::Project
                    && record.scope.project.as_ref() == Some(&project_name);
                if cites || about || (in_project && record.title.contains(path.file_name())) {
                    rationale.push(record);
                }
                if rationale.len() >= limit {
                    break;
                }
            }
            if rationale.is_empty() {
                gaps.push(Gap::new(
                    GapReason::NoMatches,
                    "no recorded decision cites this file or symbol",
                ));
            }
        }
        dedupe(&mut gaps);
        Ok(HistoryOutput {
            commits: Vec::new(),
            blame: Vec::new(),
            co_changed,
            rationale,
            gaps,
        })
    }

    /// Files that changed in the same indexed generations as `path`,
    /// most frequent first; `None` with fewer than two generations.
    async fn co_changes(
        &self,
        project: &crate::scope::PinnedProject,
        path: &RepoPath,
        limit: usize,
    ) -> Result<Option<Vec<CoChange>>, ToolError> {
        let mut conn = self.inner.store.acquire().await.map_err(store_tool)?;
        let history = content::file_history(&mut conn, project.view, path, 50)
            .await
            .map_err(store_tool)?;
        let generations = views::list_generations(&mut conn, project.view)
            .await
            .map_err(store_tool)?;
        let readable: BTreeSet<i64> = generations
            .iter()
            .filter(|g| g.generation <= project.generation)
            .map(|g| g.generation)
            .collect();
        // Generations in which the subject changed (a new version began),
        // excluding the first one (everything is "new" there).
        let first = readable.iter().next().copied();
        let change_points: Vec<i64> = history
            .iter()
            .map(|v| v.valid_from)
            .filter(|g| Some(*g) != first && readable.contains(g))
            .collect();
        if change_points.is_empty() {
            return Ok(None);
        }
        let mut together: BTreeMap<RepoPath, u32> = BTreeMap::new();
        for generation in &change_points {
            let Some(previous) = readable.range(..*generation).next_back().copied() else {
                continue;
            };
            let now = content::files_at(
                &mut conn,
                GenerationPin {
                    view: project.view,
                    generation: *generation,
                },
            )
            .await
            .map_err(store_tool)?;
            let before = content::files_at(
                &mut conn,
                GenerationPin {
                    view: project.view,
                    generation: previous,
                },
            )
            .await
            .map_err(store_tool)?;
            let before: BTreeMap<_, _> = before
                .into_iter()
                .map(|f| (f.path, f.content_hash))
                .collect();
            for file in now {
                if &file.path != path && before.get(&file.path) != Some(&file.content_hash) {
                    let count = together.entry(file.path).or_default();
                    *count = count.saturating_add(1);
                }
            }
        }
        let of = u32::try_from(change_points.len()).unwrap_or(u32::MAX);
        let mut list: Vec<CoChange> = together
            .into_iter()
            .map(|(other, count)| CoChange {
                project: project.entry.name.clone(),
                path: other,
                together: count,
                of_commits: of,
            })
            .collect();
        list.sort_by(|a, b| {
            b.together
                .cmp(&a.together)
                .then_with(|| a.path.cmp(&b.path))
        });
        list.truncate(limit);
        Ok(Some(list))
    }
}

#[cfg(test)]
mod tests {
    use knowell_core::{ContentHash, LineRange, Name};
    use knowell_query::{
        Citation, CommitId, EdgeKind, EvidenceType, ExpandedItem, GraphStep, Layer, Location,
        Resolution, ViewId,
    };

    use super::*;

    fn expanded(symbol: &str, start: u32, edge: EdgeKind) -> ExpandedItem {
        let location = Location {
            project: Name::new("synthetic").unwrap(),
            path: RepoPath::new("src/fixture.rs").unwrap(),
            range: Some(LineRange::new(start, start + 4).unwrap()),
            view: ViewId::new("fixture-view").unwrap(),
            generation: 7,
            content_hash: ContentHash::of(b"synthetic source identity fixture"),
        };
        let mut seed = location.clone();
        seed.path = RepoPath::new("src/subject.rs").unwrap();
        seed.range = Some(LineRange::new(1, 5).unwrap());
        let path = vec![GraphStep {
            edge,
            evidence: EvidenceType::SyntacticObservation,
            resolution: Resolution::Resolved,
            to: location.clone(),
            symbol: Some(symbol.to_owned()),
        }];
        ExpandedItem {
            location,
            commit: Some(CommitId::new("1".repeat(40)).unwrap()),
            layer: Layer::Base,
            symbol: Some(symbol.to_owned()),
            language: None,
            seed_rank: 1,
            depth: 1,
            score: 0.5,
            why: vec![Reason::GraphPath {
                seed,
                steps: path.clone(),
            }],
            path,
        }
    }

    fn packed(source: &ExpandedItem) -> PackItem {
        PackItem {
            origin: Origin::Expanded {
                seed_rank: source.seed_rank,
                depth: source.depth,
            },
            kind: SnippetKind::Body,
            citation: Citation {
                project: source.location.project.clone(),
                view: source.location.view.clone(),
                generation: source.location.generation,
                commit: source.commit.clone(),
                path: source.location.path.clone(),
                range: source.location.range.unwrap(),
                content_hash: source.location.content_hash,
            },
            symbol: source.symbol.clone(),
            text: "synthetic shown body".to_owned(),
            tokens: 12,
            why: source.why.clone(),
            covers: Vec::new(),
        }
    }

    #[test]
    fn expanded_bodies_keep_their_same_file_same_depth_endpoint_and_graph_path() {
        let sources = vec![
            expanded("first", 10, EdgeKind::Caller),
            expanded("second", 30, EdgeKind::Test),
        ];
        let item = packed(&sources[1]);
        let extent = |location: &Location, _: Option<&str>| location.range;
        let source = expanded_pack_source(&sources, &item, 1, 1, &extent).unwrap();
        assert_eq!(source.location, sources[1].location);
        assert_eq!(source.symbol, sources[1].symbol);
        assert_eq!(source.path, sources[1].path);
        assert_eq!(source.why, sources[1].why);
        assert_eq!(why_relevant(&source.why), "test of a top result (1 hop(s))");

        let mut wrong_reason = item.clone();
        wrong_reason.why = sources[0].why.clone();
        assert!(expanded_pack_source(&sources, &wrong_reason, 1, 1, &extent).is_none());
    }

    #[test]
    fn packed_sources_require_the_exact_binding_and_allow_only_shortened_bodies() {
        let source = expanded("second", 30, EdgeKind::Test);
        let item = packed(&source);
        let matches = |item: &PackItem| {
            pack_source_matches(
                item,
                &source.location,
                source.commit.as_ref(),
                source.symbol.as_deref(),
                &source.why,
                Some(LineRange::new(20, 34).unwrap()),
            )
        };
        assert!(matches(&item));
        for changed in 0..9 {
            let mut wrong = item.clone();
            match changed {
                0 => wrong.citation.project = Name::new("other").unwrap(),
                1 => wrong.citation.path = RepoPath::new("src/other.rs").unwrap(),
                2 => wrong.citation.view = ViewId::new("other-view").unwrap(),
                3 => wrong.citation.generation += 1,
                4 => wrong.citation.content_hash = ContentHash::of(b"other synthetic body"),
                5 => wrong.citation.commit = Some(CommitId::new("2".repeat(40)).unwrap()),
                6 => wrong.citation.range = LineRange::new(10, 14).unwrap(),
                7 => wrong.symbol = Some("other".to_owned()),
                8 => wrong.why.clear(),
                _ => unreachable!(),
            }
            assert!(
                !matches(&wrong),
                "accepted a mismatched identity field {changed}"
            );
        }
        let mut shortened = item.clone();
        shortened.citation.range = LineRange::new(30, 32).unwrap();
        assert!(matches(&shortened));

        let mut preceding_signature = item;
        preceding_signature.kind = SnippetKind::Skeleton;
        preceding_signature.citation.range = LineRange::new(20, 20).unwrap();
        assert!(matches(&preceding_signature));
    }

    #[test]
    fn packed_parent_expansion_stays_inside_its_verified_acquisition_extent() {
        let source = expanded("guarded", 150, EdgeKind::Callee);
        let mut item = packed(&source);
        item.citation.range = LineRange::new(140, 180).unwrap();
        let extent = Some(LineRange::new(1, 3000).unwrap());
        let matches = |item: &PackItem| {
            pack_source_matches(
                item,
                &source.location,
                source.commit.as_ref(),
                source.symbol.as_deref(),
                &source.why,
                extent,
            )
        };
        assert!(matches(&item));
        item.citation.range = LineRange::new(2990, 3001).unwrap();
        assert!(!matches(&item));
        item.citation.range = LineRange::new(140, 180).unwrap();
        assert!(!pack_source_matches(
            &item,
            &source.location,
            source.commit.as_ref(),
            source.symbol.as_deref(),
            &source.why,
            None
        ));
    }

    #[test]
    fn source_caveats_follow_selected_links_instead_of_all_workspace_languages() {
        let source = expanded("selected", 30, EdgeKind::Callee);
        let mut item = packed(&source);
        for reason in &mut item.why {
            if let Reason::GraphPath { steps, .. } = reason
                && let Some(step) = steps.first_mut()
            {
                step.resolution = Resolution::Unresolved;
            }
        }
        let mut uncertainties: Vec<_> = ["rust", "markdown", "toml", "yaml"]
            .into_iter()
            .map(|language| Uncertainty::NoReferenceResolution {
                project: source.location.project.clone(),
                language: knowell_query::Language::new(language).unwrap(),
            })
            .collect();
        let other = expanded("unselected", 60, EdgeKind::Callee);
        uncertainties.push(Uncertainty::WeakGraphEvidence {
            location: other.location,
            edge: EdgeKind::Callee,
            evidence: EvidenceType::SyntacticObservation,
            resolution: Resolution::Unresolved,
        });
        uncertainties.push(Uncertainty::WeakGraphEvidence {
            location: source.location.clone(),
            edge: EdgeKind::Callee,
            evidence: EvidenceType::SyntacticObservation,
            resolution: Resolution::Unresolved,
        });
        let mut plain = item.clone();
        plain.why.clear();
        let plain_notices = source_uncertainties(&uncertainties, &[plain]);
        assert!(plain_notices.is_empty());
        assert!(
            !plain_notices
                .iter()
                .any(|notice| notice.contains("compiler reference"))
        );
        let notices = source_uncertainties(&uncertainties, &[item]);
        assert_eq!(notices.len(), 2);
        assert_eq!(
            notices
                .iter()
                .filter(|notice| notice.contains("compiler reference"))
                .count(),
            1
        );
        assert!(
            !notices
                .iter()
                .any(|notice| notice.contains("markdown") || notice.contains("toml"))
        );
        assert!(source_uncertainties(&uncertainties, &[]).is_empty());
    }
}
