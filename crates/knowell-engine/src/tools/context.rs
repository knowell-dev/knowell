//! `build_context` and `history`.

use std::collections::{BTreeMap, BTreeSet};

use knowell_core::RepoPath;
use knowell_knowledge::RecordState;
use knowell_mcp::tools::{
    BuildContextInput, BuildContextOutput, CoChange, ContextEntry, ContextSection, EntryKind,
    HistoryFacet, HistoryInput, HistoryOutput, ScopeLevel, TokenBudget,
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
use crate::evidence::{degradation_gaps, empty_gaps, place, reasons};
use crate::memory::RecordQuery;
use crate::search::{Filters, is_test_path, snippets};
use crate::snapshot::slice_lines;

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
            "{project}: {language} references are not resolved; callers and tests come from file imports only"
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
            let snapshot = self.snapshot_of(project).await?;
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
            let Some(lines) = locator
                .lines
                .or_else(|| crate::snapshot::whole_file(file.line_count))
            else {
                continue;
            };
            let body = slice_lines(&text, lines);
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
                    estimated_tokens: cost,
                });
            }
        }
        // 3. Code found for the task, packed into what is left.
        let mut query = task.to_owned();
        for symbol in &input.focus_symbols {
            query.push(' ');
            query.push_str(symbol);
        }
        let run = self
            .run_search(&pinned, &Filters::default(), &query, 20, true, false)
            .await?;
        let texts = self.texts_for(&run).await?;
        let source = snippets(&run, texts, self.inner.settings.max_fetch_lines);
        let remaining = requested.saturating_sub(used);
        let pack = knowell_query::pack(&run.response, remaining, &source);
        for item in &pack.items {
            let Some(entry) = self.pack_entry(&run, item, &wants)? else {
                continue;
            };
            used = used.saturating_add(item.tokens);
            entries.push(entry);
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
        for u in &pack.uncertainties {
            let text = uncertainty_text(u);
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
        Ok(BuildContextOutput {
            entries,
            budget: TokenBudget {
                requested,
                used: used.min(requested),
            },
            uncertainties,
            job: None,
            gaps,
        })
    }

    fn pack_entry(
        &self,
        run: &crate::search::SearchRun,
        item: &PackItem,
        wants: &dyn Fn(ContextSection) -> bool,
    ) -> Result<Option<ContextEntry>, ToolError> {
        let (location, why, score) = match item.origin {
            Origin::Result { rank } => {
                let Some(result) = run.response.results.iter().find(|r| r.rank == rank) else {
                    return Ok(None);
                };
                (&result.location, &result.why, Some(&result.score))
            }
            Origin::Expanded { seed_rank, depth } => {
                let Some(expanded) = run.response.expanded.iter().find(|e| {
                    e.seed_rank == seed_rank
                        && e.depth == depth
                        && e.location.path == item.citation.path
                        && e.location.project == item.citation.project
                }) else {
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
        let language = placed.language.as_deref();
        let section = if is_test_path(&location.path) {
            ContextSection::Tests
        } else if matches!(language, Some("markdown" | "text")) {
            ContextSection::Docs
        } else {
            ContextSection::Code
        };
        if !wants(section) {
            return Ok(None);
        }
        let kind = match (item.kind, section) {
            (SnippetKind::Skeleton, _) if item.symbol.is_some() => EntryKind::Signature,
            (SnippetKind::Skeleton, _) => EntryKind::Skeleton,
            (SnippetKind::Body, ContextSection::Tests) => EntryKind::Test,
            (SnippetKind::Body, ContextSection::Docs) => EntryKind::Doc,
            (SnippetKind::Body, _) => EntryKind::Code,
        };
        Ok(Some(ContextEntry {
            id: placed.id,
            section,
            kind,
            why_relevant: why_relevant(why),
            evidence: Some(placed.evidence),
            memory_id: None,
            content: UntrustedText::repository(item.text.clone()),
            estimated_tokens: item.tokens,
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
