//! `search`, `fetch` and `inspect_symbol`.

use std::collections::BTreeSet;
use std::sync::Arc;

use knowell_core::{LineRange, Name, RepoPath};
use knowell_mcp::tools::{
    FetchInput, FetchOutput, FetchedItem, HitKind, InspectSymbolInput, InspectSymbolOutput,
    SearchHit, SearchInput, SearchKind, SearchOutput, SymbolFacet, SymbolInfo, SymbolKind,
    SymbolLink, VersionStatus,
};
use knowell_mcp::{
    EvidenceType, FileLocator, FreshnessTier, Gap, GapReason, MatchReason, RelationKind,
    Resolution, ResultId, ToolError, UntrustedText,
};
use knowell_parse::SymbolKind as ParseKind;
use knowell_query::Language;
use knowell_secrets::ExclusionPolicy;
use knowell_store::content;

use super::workspace::analysis_level;
use crate::access::Access;
use crate::engine::Engine;
use crate::error::store_tool;
use crate::evidence::{
    coverage_gaps, degradation_gaps, empty_gaps, freshness_of, hit_kind, no_commit_gap, place,
    query_class, reasons,
};
use crate::ids::{parse_source_id, source_id};
use crate::scope::{Pinned, PinnedProject};
use crate::search::{Filters, is_test_path, symbol_strength};
use crate::snapshot::{Snapshot, SymbolEntry, line_count, slice_lines, whole_file};

/// Most lines of a search snippet.
const SNIPPET_LINES: u32 = 40;

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

/// `range` clamped to `max` lines; `true` when it was cut.
fn cap(range: LineRange, max: u32) -> (LineRange, bool) {
    if range.line_count() <= max {
        return (range, false);
    }
    let end = range.start().saturating_add(max.saturating_sub(1));
    (LineRange::new(range.start(), end).unwrap_or(range), true)
}

impl Engine {
    pub(crate) async fn tool_search(
        &self,
        access: Access,
        input: SearchInput,
    ) -> Result<SearchOutput, ToolError> {
        let pinned = self.resolve_target(&access, &input.target).await?;
        let limit = usize::try_from(input.limit.unwrap_or(10)).unwrap_or(10);
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
        };
        let mut gaps = pinned.gaps.clone();
        gaps.extend(pinned.not_indexed_gaps(&input.projects));
        let mut hits = Vec::new();
        let mut more_available = false;
        let mut query_class_out =
            query_class(knowell_query::plan(&input.query, &self.inner.glossary).intent);
        if wants(SearchKind::Code) || wants(SearchKind::Docs) || wants(SearchKind::Contracts) {
            let run = self
                .run_search(&pinned, &filters, &input.query, limit, true, false)
                .await?;
            query_class_out = query_class(run.response.plan.intent);
            let texts = if input.include_snippets.unwrap_or(true) {
                self.texts_for(&run).await?
            } else {
                Default::default()
            };
            let mut no_commit = BTreeSet::new();
            for result in &run.response.results {
                let why = reasons(
                    &result.why,
                    Some(&result.score),
                    result.symbol.as_deref(),
                    &result.location,
                );
                let freshness = freshness_of(&result.score, result.location.range);
                let Some(placed) = place(
                    &run.prepared,
                    &result.location,
                    result.symbol.as_deref(),
                    why.clone(),
                    freshness,
                )?
                else {
                    no_commit.insert(result.location.project.clone());
                    continue;
                };
                let kind = hit_kind(&result.location.path, placed.language.as_deref(), &why);
                let wanted = match kind {
                    HitKind::Doc => wants(SearchKind::Docs),
                    HitKind::Contract => wants(SearchKind::Contracts),
                    _ => wants(SearchKind::Code),
                };
                if !wanted {
                    continue;
                }
                let snippet = if input.include_snippets.unwrap_or(true) {
                    let (lines, _) = cap(placed.evidence.lines, SNIPPET_LINES);
                    match run
                        .prepared
                        .view_of(&result.location.project, &result.location.view)
                    {
                        Some((view, true)) => view
                            .overlay
                            .as_ref()
                            .and_then(|o| o.overlay.file(&result.location.path))
                            .map(|f| UntrustedText::repository(slice_lines(&f.text, lines))),
                        Some((_, false)) => texts
                            .get(&result.location.content_hash)
                            .map(|t| UntrustedText::repository(slice_lines(t, lines))),
                        None => None,
                    }
                } else {
                    None
                };
                let title = result.symbol.clone().unwrap_or_else(|| {
                    format!("{}:{}", result.location.path, placed.evidence.lines)
                });
                hits.push(SearchHit {
                    id: placed.id,
                    kind,
                    title,
                    evidence: placed.evidence,
                    snippet,
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
            self.memory_hits(&access, &pinned, &input.query, limit)
                .await?
        } else {
            Vec::new()
        };
        if hits.is_empty() && memory_hits.is_empty() && gaps.is_empty() {
            gaps.push(Gap::new(GapReason::NoMatches, knowell_query::ABSENCE_NOTE));
        }
        dedupe(&mut gaps);
        Ok(SearchOutput {
            query_class: query_class_out,
            hits,
            memory_hits,
            more_available,
            gaps,
        })
    }

    /// The text of a file version of a pinned project, by full hash.
    async fn text_of(
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
        let lines = LineRange::new(
            lines.start().min(total.max(1)),
            lines
                .end()
                .min(total.max(1))
                .max(lines.start().min(total.max(1))),
        )
        .unwrap_or(lines);
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
            content: UntrustedText::repository(slice_lines(text, lines)),
            truncated,
            status,
            current_id,
        }))
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
        {
            let total = line_count(&file.text);
            return Self::fetched(
                project,
                &file.path,
                file.content_hash,
                Some(file.parsed.language.as_str().to_owned()),
                &file.text,
                widen(source.lines, context, total),
                VersionStatus::Current,
                None,
                true,
                max_lines,
            );
        }
        let snapshot = self.snapshot_of(project).await?;
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
        {
            let Some(text) = self.text_of(&file.content_hash).await? else {
                return Ok(Err(not_found()));
            };
            return Self::fetched(
                project,
                path,
                file.content_hash,
                file.language.clone(),
                &text,
                widen(source.lines, context, file.line_count),
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
        drop(conn);
        let Some(version) = history
            .iter()
            .find(|v| source.names_version(&v.content_hash))
        else {
            return Ok(Err(not_found()));
        };
        let Some(text) = self.text_of(&version.content_hash).await? else {
            return Ok(Err(not_found()));
        };
        let total = line_count(&text);
        let (status, current_id) = match &current {
            Some((cur_path, cur_file)) => {
                let lines = LineRange::new(
                    source.lines.start().min(cur_file.line_count.max(1)),
                    source.lines.end().min(cur_file.line_count.max(1)),
                )
                .ok()
                .or_else(|| whole_file(cur_file.line_count));
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
        let language = current.as_ref().and_then(|(_, f)| f.language.clone());
        Self::fetched(
            project,
            &path,
            version.content_hash,
            language,
            &text,
            widen(source.lines, context, total),
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
                    .map(|l| widen(l, context, total))
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
        let snapshot = self.snapshot_of(project).await?;
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
        let lines = locator
            .lines
            .map(|l| widen(l, context, file.line_count))
            .or_else(|| whole_file(file.line_count))
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
            let snapshot = self.snapshot_of(pinned_project).await?;
            let Some(path) = snapshot
                .files
                .keys()
                .find(|p| source.path.matches(p))
                .cloned()
            else {
                return Ok(found);
            };
            let exact = snapshot
                .symbols_in(&path)
                .find(|s| s.lines == source.lines)
                .or_else(|| snapshot.enclosing_symbol(&path, source.lines))
                .cloned();
            if let Some(symbol) = exact {
                found.push((pinned_project.clone(), snapshot, symbol, 6));
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
        let limit = usize::try_from(input.limit.unwrap_or(20)).unwrap_or(20);
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
        let mut languages_without_refs = BTreeSet::new();
        for (project, snapshot, symbol, _) in found.into_iter().take(10) {
            let Some(file) = snapshot.file(&symbol.path) else {
                continue;
            };
            let language = file.language.clone().unwrap_or_else(|| "text".to_owned());
            let Some(commit) = project.commit_id() else {
                gaps.push(no_commit_gap(&project.entry.name));
                continue;
            };
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
            let mut references = Vec::new();
            let mut tests = Vec::new();
            if let Some(id) = symbol.store_id {
                for edge in snapshot.uses_of(id) {
                    let Some(file) = snapshot.file(&edge.origin) else {
                        continue;
                    };
                    let Some(lines) = edge.lines.or_else(|| whole_file(file.line_count)) else {
                        continue;
                    };
                    let Some(commit) = project.commit_id() else {
                        continue;
                    };
                    let test = is_test_path(&edge.origin);
                    let link = SymbolLink {
                        id: source_id(
                            &project.entry.name,
                            Some(commit.as_str()),
                            &file.content_hash,
                            &edge.origin,
                            lines,
                        )?,
                        relation: if test {
                            RelationKind::Tests
                        } else if edge.kind == "calls" {
                            RelationKind::Calls
                        } else {
                            RelationKind::References
                        },
                        evidence_type: match edge.evidence {
                            knowell_store::EvidenceType::SemanticResolved => {
                                EvidenceType::SemanticallyResolved
                            }
                            knowell_store::EvidenceType::ContractDerived => {
                                EvidenceType::ContractDerived
                            }
                            knowell_store::EvidenceType::Syntactic => {
                                EvidenceType::SyntacticObservation
                            }
                            knowell_store::EvidenceType::Heuristic => EvidenceType::HeuristicMatch,
                            knowell_store::EvidenceType::ModelSuggestion => {
                                EvidenceType::ModelSuggestion
                            }
                            knowell_store::EvidenceType::RuntimeObserved => {
                                EvidenceType::RuntimeObservation
                            }
                        },
                        resolution: match edge.resolution {
                            knowell_store::Resolution::Resolved => Resolution::Resolved,
                            knowell_store::Resolution::Ambiguous => Resolution::Ambiguous,
                            knowell_store::Resolution::Unresolved => Resolution::Unresolved,
                        },
                        evidence: knowell_mcp::Evidence {
                            project: project.entry.name.clone(),
                            view: project.target.clone(),
                            layer: knowell_mcp::ViewLayer::Shared,
                            commit,
                            path: edge.origin.clone(),
                            lines,
                            content_hash: file.content_hash,
                            symbol: snapshot
                                .enclosing_symbol(&edge.origin, lines)
                                .map(|s| s.local.clone()),
                            why: Vec::new(),
                            freshness: FreshnessTier::T1Symbols,
                            index_state: project.index_state(),
                        },
                    };
                    if test {
                        tests.push(link);
                    } else {
                        references.push(link);
                    }
                }
            }
            for edge in snapshot.imports_of(&symbol.path) {
                let Some(importer) = snapshot.file(&edge.from) else {
                    continue;
                };
                let lines = edge
                    .lines
                    .or_else(|| whole_file(importer.line_count))
                    .ok_or_else(|| ToolError::internal("empty range"))?;
                let Some(commit) = project.commit_id() else {
                    continue;
                };
                let link_id = source_id(
                    &project.entry.name,
                    Some(commit.as_str()),
                    &importer.content_hash,
                    &edge.from,
                    lines,
                )?;
                let test = is_test_path(&edge.from);
                let link = SymbolLink {
                    id: link_id,
                    relation: if test {
                        RelationKind::Tests
                    } else {
                        RelationKind::Imports
                    },
                    evidence_type: EvidenceType::SyntacticObservation,
                    resolution: match edge.resolution {
                        knowell_store::Resolution::Resolved => Resolution::Resolved,
                        knowell_store::Resolution::Ambiguous => Resolution::Ambiguous,
                        knowell_store::Resolution::Unresolved => Resolution::Unresolved,
                    },
                    evidence: knowell_mcp::Evidence {
                        project: project.entry.name.clone(),
                        view: project.target.clone(),
                        layer: knowell_mcp::ViewLayer::Shared,
                        commit,
                        path: edge.from.clone(),
                        lines,
                        content_hash: importer.content_hash,
                        symbol: snapshot
                            .enclosing_symbol(&edge.from, lines)
                            .map(|s| s.local.clone()),
                        why: Vec::new(),
                        freshness: FreshnessTier::T1Symbols,
                        index_state: project.index_state(),
                    },
                };
                if test {
                    tests.push(link);
                } else {
                    references.push(link);
                }
            }
            references.sort_by(|a, b| a.evidence.path.cmp(&b.evidence.path));
            tests.sort_by(|a, b| a.evidence.path.cmp(&b.evidence.path));
            references.truncate(limit);
            tests.truncate(limit);
            languages_without_refs.insert((project.entry.name.clone(), language.clone()));
            symbols.push(SymbolInfo {
                id,
                name: symbol.name.clone(),
                qualified_name: symbol.local.clone(),
                kind: symbol_kind(symbol.kind),
                analysis: analysis_level(&language),
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
                    references
                } else {
                    Vec::new()
                },
                implementations: Vec::new(),
                tests: if wants(SymbolFacet::Tests) {
                    tests
                } else {
                    Vec::new()
                },
                // Only file imports are known; call sites are not resolved.
                references_complete: false,
            });
        }
        for (project, language) in languages_without_refs {
            gaps.push(Gap::for_project(
                GapReason::NoReferenceResolutionForLanguage,
                project,
                format!(
                    "{language}: references are the files that import the definition's file (syntactic); call sites and implementations are not resolved yet"
                ),
            ));
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
    fn symbol_kinds_map() {
        assert_eq!(symbol_kind(ParseKind::Method), SymbolKind::Method);
        assert_eq!(symbol_kind(ParseKind::Heading), SymbolKind::Other);
    }
}
