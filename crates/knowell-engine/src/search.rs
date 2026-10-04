//! The hybrid search pipeline over real data, and the adapters that serve
//! `knowell-query`'s source traits.
//!
//! ```text
//! pinned manifest ──prepare()──▶ PreparedView per project
//!   snapshot (symbols, chunks, imports) · Tantivy index of the pinned
//!   generation · personal overlay (owner only)
//!        │
//!        ├─ ExactAdapter     (ExactSource)    symbols, paths, contract keys
//!        ├─ LexicalAdapter   (LexicalSource)  BM25 per view, file hits mapped onto chunks
//!        ├─ semantic()       (async)          pgvector `nearest` per profile, one list per profile
//!        └─ GraphAdapter     (GraphExpander)  explicit stored calls and tests
//!        ▼
//!   knowell_query::search_with_candidates ──▶ SearchResponse ──pack()──▶ ContextPack
//!                                                         SnippetAdapter (SnippetSource)
//! ```
//!
//! Permission enforcement point 2: every adapter only reads the prepared
//! views, and those exist only for pinned (visible) projects, so graph
//! expansion and context packing can never reach an unauthorised project.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use knowell_auth::UserId;
use knowell_core::{ContentHash, LineRange, Name, RepoPath};
use knowell_embed::{Embedder, Usage};
use knowell_index::{EmbeddingPlan, Overlay, TierSkip};
use knowell_lexical::{LexicalHit, LexicalIndex};
use knowell_mcp::ToolError;
use knowell_mcp::tools::{ContextSection, HitKind, SearchKind};
use knowell_query::{
    Candidate, Component, Degradation, EdgeKind, EvidenceType as QEvidence, ExactSource,
    ExactTarget, ExpandRequest, GraphExpander, GraphNode, Intent, Language, LexicalSource,
    Location, MatchDetail, Neighbor, OverlayPin, PathFilter, PinnedView, PlanOptions,
    ProjectCoverage, ProjectPin, QueryPlan, QueryScope, Resolution as QResolution, SearchConfig,
    SearchResponse, Snippet, SnippetKind, SnippetRequest, SnippetSource, SourceError, SourceKind,
    SourceLists, SourceRequest, SourceStatus, TermKind, ViewId as QViewId, ViewManifest,
};
use knowell_store::ProfileId;
use knowell_store::content;
use knowell_store::embeddings::{self, NearestOptions};
use knowell_store::views::{self, GenerationPin};

use crate::engine::Engine;
use crate::scope::{Pinned, PinnedProject};
use crate::snapshot::{
    self, ChunkEntry, Snapshot, SymbolEntry, best_chunks, symbol_entries, terms_of, whole_file,
};

/// A personal overlay prepared for searching.
pub(crate) struct PreparedOverlay {
    pub(crate) view: QViewId,
    pub(crate) generation: u64,
    pub(crate) overlay: Arc<Overlay>,
    pub(crate) symbols: Vec<SymbolEntry>,
    pub(crate) chunks: BTreeMap<RepoPath, Vec<ChunkEntry>>,
}

impl PreparedOverlay {
    /// Language of an overlay file.
    fn language(&self, path: &RepoPath) -> Option<Language> {
        self.overlay
            .file(path)
            .and_then(|f| Language::new(f.parsed.language.as_str()).ok())
    }
}

/// One pinned project, ready to be searched.
pub(crate) struct PreparedView {
    pub(crate) pinned: PinnedProject,
    pub(crate) snapshot: Arc<Snapshot>,
    pub(crate) base_view: QViewId,
    pub(crate) lexical: Result<Arc<LexicalIndex>, String>,
    pub(crate) overlay: Option<PreparedOverlay>,
}

impl PreparedView {
    pub(crate) fn project(&self) -> &Name {
        &self.pinned.entry.name
    }

    pub(crate) fn generation(&self) -> u64 {
        u64::try_from(self.pinned.generation).unwrap_or(0)
    }

    fn language(&self, path: &RepoPath) -> Option<Language> {
        self.snapshot
            .file(path)
            .and_then(|f| f.language.as_deref())
            .and_then(|l| Language::new(l).ok())
    }

    /// The location of `path` (and `range`) in the base view.
    pub(crate) fn base_location(
        &self,
        path: &RepoPath,
        range: Option<LineRange>,
    ) -> Option<Location> {
        let file = self.snapshot.file(path)?;
        Some(Location {
            project: self.project().clone(),
            path: path.clone(),
            range,
            view: self.base_view.clone(),
            generation: self.generation(),
            content_hash: file.content_hash,
        })
    }
}

/// Filters of one search.
#[derive(Debug, Clone, Default)]
pub(crate) struct Filters {
    pub(crate) projects: Option<BTreeSet<Name>>,
    pub(crate) languages: Option<BTreeSet<Language>>,
    pub(crate) path_prefixes: Vec<String>,
    pub(crate) kinds: Vec<SearchKind>,
    pub(crate) sections: Vec<ContextSection>,
}

/// Retrieval work performed before fusion. Counts include repeated refill
/// queries; embedding latency includes provider queueing and retry backoff.
#[derive(Debug, Clone, Default)]
pub(crate) struct RetrievalMetrics {
    pub(crate) preparation_ms: u64,
    pub(crate) source_hydration_ms: u64,
    pub(crate) exact_ms: u64,
    pub(crate) lexical_ms: u64,
    pub(crate) semantic_ms: u64,
    pub(crate) fusion_expansion_ms: u64,
    pub(crate) prepared_views: usize,
    pub(crate) lexical_queries: usize,
    pub(crate) lexical_file_hits_examined: usize,
    pub(crate) lexical_candidates: usize,
    pub(crate) semantic_queries: usize,
    pub(crate) semantic_neighbors_examined: usize,
    pub(crate) embedding_calls: usize,
    pub(crate) embedding_failures: usize,
    pub(crate) embedding_usage: Usage,
    pub(crate) exact_path_embedding_bypass: bool,
}

/// Admission happens before a source's quota. Otherwise out-of-scope or
/// shadowed files can use every slot and hide valid lower-ranked evidence.
fn candidate_allowed_in_sections(
    scope: &QueryScope,
    candidate: &Candidate,
    kinds: &[SearchKind],
    sections: &[ContextSection],
) -> bool {
    if scope
        .projects
        .as_ref()
        .is_some_and(|projects| !projects.contains(&candidate.project))
        || !scope.paths.admits(&candidate.path)
        || scope.languages.as_ref().is_some_and(|languages| {
            candidate
                .language
                .as_ref()
                .is_none_or(|language| !languages.contains(language))
        })
    {
        return false;
    }
    let Some(project) = scope.manifest.projects.get(&candidate.project) else {
        return false;
    };
    let Some((pin, layer)) = project.view(&candidate.view) else {
        return false;
    };
    if pin.generation != candidate.generation
        || (layer == knowell_query::Layer::Base
            && project
                .overlay
                .as_ref()
                .is_some_and(|overlay| overlay.shadowed_paths.contains(&candidate.path)))
    {
        return false;
    }
    let hit_kind = if matches!(
        candidate.detail,
        MatchDetail::Exact {
            target: ExactTarget::Contract,
            ..
        }
    ) {
        HitKind::Contract
    } else {
        crate::evidence::hit_kind(
            &candidate.path,
            candidate.language.as_ref().map(Language::as_str),
            &[],
        )
    };
    let kind = match hit_kind {
        HitKind::Doc => SearchKind::Docs,
        HitKind::Contract => SearchKind::Contracts,
        _ => SearchKind::Code,
    };
    let section = match hit_kind {
        HitKind::Test => ContextSection::Tests,
        HitKind::Doc => ContextSection::Docs,
        HitKind::Contract => ContextSection::Contracts,
        _ => ContextSection::Code,
    };
    (kinds.is_empty() || kinds.contains(&kind))
        && (sections.is_empty() || sections.contains(&section))
}

#[cfg(test)]
fn candidate_allowed(scope: &QueryScope, candidate: &Candidate, kinds: &[SearchKind]) -> bool {
    candidate_allowed_in_sections(scope, candidate, kinds, &[])
}

/// A bounded refill still cannot promise complete retrieval. If the bound
/// is reached before enough admitted hits, the caller reports degradation.
const MAX_LEXICAL_REFILL: usize = 4096;
#[derive(Default)]
struct LexicalScan {
    hits: Vec<LexicalHit>,
    queries: usize,
    examined: usize,
    capped: bool,
}

fn scoped_lexical_hits<E>(
    mut search: impl FnMut(usize) -> Result<Vec<LexicalHit>, E>,
    admitted: impl Fn(&LexicalHit) -> bool,
    limit: usize,
) -> Result<LexicalScan, E> {
    if limit == 0 {
        return Ok(LexicalScan::default());
    }
    let max = limit.max(MAX_LEXICAL_REFILL);
    let mut fetch = limit;
    let mut scan = LexicalScan::default();
    loop {
        let hits = search(fetch)?;
        scan.queries = scan.queries.saturating_add(1);
        scan.examined = scan.examined.saturating_add(hits.len());
        let exhausted = hits.len() < fetch;
        scan.hits = hits.into_iter().filter(&admitted).take(limit).collect();
        if scan.hits.len() >= limit || exhausted {
            break;
        }
        if fetch >= max {
            scan.capped = true;
            break;
        }
        fetch = fetch.saturating_mul(2).min(max);
    }
    Ok(scan)
}

/// A path-only request already answered by an exact full path needs no
/// similarity lookup. A basename, mixed question, missing path or truncated
/// plan keeps the normal semantic path.
fn exact_path_answered(plan: &QueryPlan, exact: &SourceStatus) -> bool {
    if plan.intent != Intent::PathOrFile || plan.truncated || !plan.expansions.is_empty() {
        return false;
    }
    let [term] = plan.exact_terms.as_slice() else {
        return false;
    };
    let query = plan.query.trim().replace('\\', "/");
    let requested = query.trim_start_matches("./");
    if term.kind != TermKind::Path || requested != term.text.trim_start_matches("./") {
        return false;
    }
    let SourceStatus::Answered(candidates) = exact else {
        return false;
    };
    candidates.iter().any(|candidate| {
        candidate.path.as_str() == requested
            && matches!(
                candidate.detail,
                MatchDetail::Exact {
                    target: ExactTarget::Path,
                    ..
                }
            )
    })
}

/// Everything prepared for one query over a pinned manifest.
pub(crate) struct Prepared {
    pub(crate) views: Vec<PreparedView>,
    pub(crate) scope: QueryScope,
    pub(crate) degraded: Vec<Degradation>,
}

impl Prepared {
    /// The prepared view a query view id belongs to, and whether it is the
    /// overlay.
    pub(crate) fn view_of(&self, project: &Name, view: &QViewId) -> Option<(&PreparedView, bool)> {
        let prepared = self.views.iter().find(|v| v.project() == project)?;
        if &prepared.base_view == view {
            return Some((prepared, false));
        }
        prepared
            .overlay
            .as_ref()
            .filter(|o| &o.view == view)
            .map(|_| (prepared, true))
    }
}

/// The query view id of a base view.
pub(crate) fn base_view_id(view: knowell_store::ViewId) -> Result<QViewId, ToolError> {
    QViewId::new(view.to_string()).map_err(|e| ToolError::internal(e.to_string()))
}

fn overlay_view_id(
    view: knowell_store::ViewId,
    owner: UserId,
    generation: u64,
) -> Result<QViewId, ToolError> {
    QViewId::new(format!("overlay:{view}:{owner}:{generation}"))
        .map_err(|e| ToolError::internal(e.to_string()))
}

/// Normalizes literal prefixes once for lexical admission and SQL ANN filters.
/// Wildcard characters remain literal source-path characters.
fn normalize_prefixes(prefixes: &[String]) -> Result<Vec<String>, ToolError> {
    let mut normalized = BTreeSet::new();
    for prefix in prefixes {
        if prefix.chars().any(char::is_control) {
            return Err(ToolError::invalid_input(
                "`path_prefixes` must not contain control characters",
            ));
        }
        let prefix = prefix.trim();
        if prefix.is_empty() {
            continue;
        }
        if RepoPath::new(prefix.strip_suffix('/').unwrap_or(prefix)).is_err() {
            return Err(ToolError::invalid_input(
                "`path_prefixes` must be relative, '/'-separated literal prefixes without empty, '.' or '..' components or control characters",
            ));
        }
        normalized.insert(prefix.to_owned());
    }
    Ok(normalized.into_iter().collect())
}

/// Whether a path looks like a test file (by common naming conventions).
pub(crate) fn is_test_path(path: &RepoPath) -> bool {
    let lower = path.as_str().to_ascii_lowercase();
    let name = path.file_name().to_ascii_lowercase();
    lower
        .split('/')
        .any(|seg| matches!(seg, "test" | "tests" | "__tests__" | "spec" | "specs"))
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.contains("_test.")
        || name.starts_with("test_")
        || name.ends_with("test.java")
        || name.ends_with("tests.cs")
        || name.ends_with("_spec.rb")
}

fn map_evidence(evidence: knowell_store::EvidenceType) -> QEvidence {
    match evidence {
        knowell_store::EvidenceType::SemanticResolved => QEvidence::SemanticallyResolved,
        knowell_store::EvidenceType::ContractDerived => QEvidence::ContractDerived,
        knowell_store::EvidenceType::Syntactic => QEvidence::SyntacticObservation,
        knowell_store::EvidenceType::Heuristic => QEvidence::HeuristicMatch,
        knowell_store::EvidenceType::ModelSuggestion => QEvidence::ModelSuggestion,
        knowell_store::EvidenceType::RuntimeObserved => QEvidence::RuntimeObservation,
    }
}

fn map_resolution(resolution: knowell_store::Resolution) -> QResolution {
    match resolution {
        knowell_store::Resolution::Resolved => QResolution::Resolved,
        knowell_store::Resolution::Ambiguous => QResolution::Ambiguous,
        knowell_store::Resolution::Unresolved => QResolution::Unresolved,
    }
}

/// Chunks of an overlay file, computed with the indexer's chunk options.
fn overlay_chunks(
    file: &knowell_index::OverlayFile,
    options: &knowell_parse::ChunkOptions,
) -> Vec<ChunkEntry> {
    match knowell_parse::chunks(&file.parsed, &file.text, options) {
        Ok(chunks) => chunks
            .into_iter()
            .map(|c| ChunkEntry {
                terms: terms_of(&c.text),
                bytes: u64::try_from(c.byte_range.len()).unwrap_or(u64::MAX),
                lines: c.range,
                kind: c.kind.as_str().to_owned(),
                symbol_path: c.symbol_path,
                structure: None,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Strength of an exact symbol match of `term` (higher is stronger):
/// qualified exact 6, qualified suffix 5, case-insensitive qualified 4,
/// short name 3, case-insensitive short name 2.
pub(crate) fn symbol_strength(symbol: &SymbolEntry, term: &str) -> Option<u8> {
    let normalized = term.trim().replace("::", ".").replace('#', ".");
    let qualified = normalized.contains('.');
    if symbol.local == normalized {
        Some(6)
    } else if qualified && symbol.local.ends_with(&format!(".{normalized}")) {
        Some(5)
    } else if symbol.local.eq_ignore_ascii_case(&normalized) {
        Some(4)
    } else if !qualified && symbol.name == normalized {
        Some(3)
    } else if !qualified && symbol.name.eq_ignore_ascii_case(&normalized) {
        Some(2)
    } else {
        None
    }
}

/// The short name a term is looked up by (last qualified segment).
fn short_of(term: &str) -> String {
    term.replace("::", ".")
        .replace('#', ".")
        .rsplit('.')
        .next()
        .unwrap_or(term)
        .to_lowercase()
}

/// Strength of an exact path match.
fn path_strength(path: &RepoPath, term: &str) -> Option<u8> {
    let term = term.trim_start_matches("./");
    if path.as_str() == term {
        Some(6)
    } else if term.contains('/') && path.as_str().ends_with(&format!("/{term}")) {
        Some(5)
    } else if !term.contains('/') && path.file_name() == term {
        Some(3)
    } else {
        None
    }
}

fn rank_and_limit(
    mut found: Vec<(f64, Candidate)>,
    limit: usize,
    per_project: Option<usize>,
) -> Vec<Candidate> {
    found.sort_by(|(sa, a), (sb, b)| {
        sb.total_cmp(sa)
            .then_with(|| a.location().cmp(&b.location()))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut seen = BTreeSet::new();
    let mut per: BTreeMap<Name, usize> = BTreeMap::new();
    let mut out = Vec::new();
    for (_, mut candidate) in found {
        if out.len() >= limit {
            break;
        }
        if !seen.insert(candidate.location()) {
            continue;
        }
        let count = per.entry(candidate.project.clone()).or_default();
        if per_project.is_some_and(|max| *count >= max) {
            continue;
        }
        *count = count.saturating_add(1);
        candidate.source_rank = u32::try_from(out.len().saturating_add(1)).unwrap_or(u32::MAX);
        out.push(candidate);
    }
    out
}

/// A lexical file hit may yield several source spans. Spend the first wave
/// on distinct pinned files before their alternatives consume the same quota.
fn rank_lexical_and_limit(
    found: Vec<(f64, Candidate)>,
    limit: usize,
    per_project: Option<usize>,
) -> Vec<Candidate> {
    if limit == 0 {
        return Vec::new();
    }
    let mut found: Vec<_> = found
        .into_iter()
        .map(|(score, candidate)| {
            let mut file = candidate.location();
            file.range = None;
            (score, file, candidate)
        })
        .collect();
    found.sort_by(|(sa, fa, a), (sb, fb, b)| {
        sb.total_cmp(sa)
            .then_with(|| fa.cmp(fb))
            .then_with(|| lexical_order(a, b))
            .then_with(|| a.location().cmp(&b.location()))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut seen = BTreeSet::new();
    let mut files: BTreeMap<Location, VecDeque<Candidate>> = BTreeMap::new();
    let mut file_order = Vec::new();
    for (_, file, candidate) in found {
        if !seen.insert(candidate.location()) {
            continue;
        }
        if !files.contains_key(&file) {
            file_order.push(file.clone());
        }
        files.entry(file).or_default().push_back(candidate);
    }
    let mut per: BTreeMap<Name, usize> = BTreeMap::new();
    let mut out = Vec::new();
    loop {
        let mut advanced = false;
        for file in &file_order {
            if out.len() >= limit {
                return out;
            }
            let Some(spans) = files.get_mut(file) else {
                continue;
            };
            let Some(mut candidate) = spans.pop_front() else {
                continue;
            };
            advanced = true;
            let count = per.entry(candidate.project.clone()).or_default();
            if per_project.is_some_and(|max| *count >= max) {
                spans.clear();
                continue;
            }
            *count = count.saturating_add(1);
            candidate.source_rank = u32::try_from(out.len().saturating_add(1)).unwrap_or(u32::MAX);
            out.push(candidate);
        }
        if !advanced {
            break;
        }
    }
    out
}

/// For spans of the same pinned file, preserve the local evidence before
/// source position. Different files are tied by their complete file identity.
fn lexical_order(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    match (&a.detail, &b.detail) {
        (MatchDetail::Lexical { terms: a_terms }, MatchDetail::Lexical { terms: b_terms }) => {
            b_terms
                .len()
                .cmp(&a_terms.len())
                .then_with(|| b.symbol.is_some().cmp(&a.symbol.is_some()))
                .then_with(|| {
                    a.range
                        .map_or(u32::MAX, |range| range.line_count())
                        .cmp(&b.range.map_or(u32::MAX, |range| range.line_count()))
                })
        }
        _ => std::cmp::Ordering::Equal,
    }
}

/// Exact lookups over the pinned snapshots (and overlays): symbols, paths
/// and contract keys.
pub(crate) struct ExactAdapter<'a> {
    pub(crate) views: &'a [PreparedView],
    pub(crate) kinds: &'a [SearchKind],
    pub(crate) sections: &'a [ContextSection],
}

/// Whether a contract key matches a planned term: equal ignoring case, or
/// an endpoint key (`POST /v1/x`) whose route equals a route term.
fn contract_matches(key: &str, term: &str) -> bool {
    key.eq_ignore_ascii_case(term)
        || key
            .split_once(' ')
            .is_some_and(|(_, route)| route.eq_ignore_ascii_case(term))
}

impl ExactAdapter<'_> {
    fn symbol_candidate(
        view: &PreparedView,
        overlay: Option<&PreparedOverlay>,
        symbol: &SymbolEntry,
        term: &str,
        strength: u8,
    ) -> Option<(f64, Candidate)> {
        let (view_id, generation, hash, language) = match overlay {
            Some(o) => (
                o.view.clone(),
                o.generation,
                o.overlay.file(&symbol.path)?.content_hash,
                o.language(&symbol.path),
            ),
            None => (
                view.base_view.clone(),
                view.generation(),
                view.snapshot.file(&symbol.path)?.content_hash,
                view.language(&symbol.path),
            ),
        };
        Some((
            f64::from(strength),
            Candidate {
                id: format!("sym:{view_id}:{}", symbol.key),
                project: view.project().clone(),
                view: view_id,
                generation,
                path: symbol.path.clone(),
                range: Some(symbol.lines),
                content_hash: hash,
                symbol: Some(symbol.local.clone()),
                language,
                source: SourceKind::Exact,
                source_rank: 1,
                raw_score: f64::from(strength),
                detail: MatchDetail::Exact {
                    term: term.to_owned(),
                    target: ExactTarget::Symbol,
                },
            },
        ))
    }
}

impl ExactSource for ExactAdapter<'_> {
    fn search_exact(&self, request: &SourceRequest<'_>) -> Result<Vec<Candidate>, SourceError> {
        let mut found: Vec<(f64, Candidate)> = Vec::new();
        for view in self.views {
            if request
                .scope
                .projects
                .as_ref()
                .is_some_and(|p| !p.contains(view.project()))
            {
                continue;
            }
            for term in &request.plan.exact_terms {
                match term.kind {
                    TermKind::Identifier
                    | TermKind::QualifiedName
                    | TermKind::ErrorType
                    | TermKind::ErrorCode => {
                        let short = short_of(&term.text);
                        for index in view.snapshot.by_name.get(&short).into_iter().flatten() {
                            let Some(symbol) = view.snapshot.symbols.get(*index) else {
                                continue;
                            };
                            if let Some(strength) = symbol_strength(symbol, &term.text)
                                && let Some(c) =
                                    Self::symbol_candidate(view, None, symbol, &term.text, strength)
                            {
                                found.push(c);
                            }
                        }
                        if let Some(overlay) = &view.overlay {
                            for symbol in &overlay.symbols {
                                if symbol.name.to_lowercase() != short {
                                    continue;
                                }
                                if let Some(strength) = symbol_strength(symbol, &term.text)
                                    && let Some(c) = Self::symbol_candidate(
                                        view,
                                        Some(overlay),
                                        symbol,
                                        &term.text,
                                        strength,
                                    )
                                {
                                    found.push(c);
                                }
                            }
                        }
                    }
                    TermKind::Path => {
                        for (path, file) in &view.snapshot.files {
                            if let Some(strength) = path_strength(path, &term.text) {
                                found.push((
                                    f64::from(strength),
                                    Candidate {
                                        id: format!("path:{}:{path}", view.base_view),
                                        project: view.project().clone(),
                                        view: view.base_view.clone(),
                                        generation: view.generation(),
                                        path: path.clone(),
                                        range: None,
                                        content_hash: file.content_hash,
                                        symbol: None,
                                        language: view.language(path),
                                        source: SourceKind::Exact,
                                        source_rank: 1,
                                        raw_score: f64::from(strength),
                                        detail: MatchDetail::Exact {
                                            term: term.text.clone(),
                                            target: ExactTarget::Path,
                                        },
                                    },
                                ));
                            }
                        }
                        if let Some(overlay) = &view.overlay {
                            for file in overlay.overlay.files() {
                                if let Some(strength) = path_strength(&file.path, &term.text) {
                                    found.push((
                                        f64::from(strength),
                                        Candidate {
                                            id: format!("path:{}:{}", overlay.view, file.path),
                                            project: view.project().clone(),
                                            view: overlay.view.clone(),
                                            generation: overlay.generation,
                                            path: file.path.clone(),
                                            range: None,
                                            content_hash: file.content_hash,
                                            symbol: None,
                                            language: overlay.language(&file.path),
                                            source: SourceKind::Exact,
                                            source_rank: 1,
                                            raw_score: f64::from(strength),
                                            detail: MatchDetail::Exact {
                                                term: term.text.clone(),
                                                target: ExactTarget::Path,
                                            },
                                        },
                                    ));
                                }
                            }
                        }
                    }
                    TermKind::Route | TermKind::Phrase => {}
                }
                for party in &view.snapshot.contracts {
                    if !contract_matches(&party.contract.key, &term.text) {
                        continue;
                    }
                    let Some(path) = crate::snapshot::origin_path(&party.contract.origin) else {
                        continue;
                    };
                    let range = party
                        .contract
                        .symbol
                        .and_then(|id| view.snapshot.symbol_by_id(id))
                        .map(|s| s.lines);
                    let Some(location) = view.base_location(&path, range) else {
                        continue;
                    };
                    found.push((
                        7.0,
                        Candidate {
                            id: format!(
                                "contract:{}:{}:{}",
                                view.base_view, party.contract.kind, party.contract.key
                            ),
                            project: location.project,
                            view: location.view,
                            generation: location.generation,
                            range: location.range,
                            path,
                            content_hash: location.content_hash,
                            symbol: party
                                .contract
                                .symbol
                                .and_then(|id| view.snapshot.symbol_by_id(id))
                                .map(|s| s.local.clone()),
                            language: view.language(&location.path),
                            source: SourceKind::Exact,
                            source_rank: 1,
                            raw_score: 7.0,
                            detail: MatchDetail::Exact {
                                term: party.contract.key.clone(),
                                target: ExactTarget::Contract,
                            },
                        },
                    ));
                }
            }
        }
        found.retain(|(_, candidate)| {
            candidate_allowed_in_sections(request.scope, candidate, self.kinds, self.sections)
        });
        Ok(rank_and_limit(
            found,
            request.limit,
            request.per_project_limit,
        ))
    }
}

/// BM25 over the Tantivy index of every pinned generation (and overlays),
/// with file-level hits mapped onto a bounded set of matching source spans.
pub(crate) struct LexicalAdapter<'a> {
    pub(crate) views: &'a [PreparedView],
    pub(crate) kinds: &'a [SearchKind],
    pub(crate) sections: &'a [ContextSection],
    pub(crate) spans_per_file: u8,
    pub(crate) failures: Mutex<Vec<Degradation>>,
    pub(crate) metrics: Mutex<RetrievalMetrics>,
}

impl LexicalAdapter<'_> {
    fn query<E>(
        &self,
        search: impl FnOnce() -> Result<Vec<LexicalHit>, E>,
    ) -> Result<Vec<LexicalHit>, E> {
        let mut metrics = self.metrics.lock().unwrap_or_else(PoisonError::into_inner);
        metrics.lexical_queries = metrics.lexical_queries.saturating_add(1);
        drop(metrics);
        let hits = search()?;
        let mut metrics = self.metrics.lock().unwrap_or_else(PoisonError::into_inner);
        metrics.lexical_file_hits_examined = metrics
            .lexical_file_hits_examined
            .saturating_add(hits.len());
        Ok(hits)
    }

    fn record_scan(&self, scan: &LexicalScan, project: &Name, overlay: bool, limit: usize) {
        tracing::debug!(
            queries = scan.queries,
            examined = scan.examined,
            "scoped lexical scan"
        );
        if scan.capped {
            let layer = if overlay { " personal layer" } else { "" };
            self.failures
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(Degradation::new(
                    Component::Lexical,
                    format!(
                        "{project}{layer}: scoped lexical retrieval reached its {}-file bound; more matching files may exist",
                        limit.max(MAX_LEXICAL_REFILL)
                    ),
                ));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn candidate(
        project: &Name,
        view: QViewId,
        generation: u64,
        path: RepoPath,
        hash: ContentHash,
        language: Option<Language>,
        chunk: Option<&ChunkEntry>,
        score: f64,
        terms: Vec<String>,
    ) -> Candidate {
        let terms = match chunk {
            Some(chunk) => terms
                .into_iter()
                .filter(|term| chunk.terms.contains(term))
                .collect(),
            None => terms,
        };
        let span = chunk.map_or_else(|| "file".to_owned(), |chunk| chunk.lines.to_string());
        Candidate {
            id: format!("lex:{view}:{path}:{span}"),
            project: project.clone(),
            view,
            generation,
            range: chunk.map(|c| c.lines),
            symbol: chunk.and_then(|c| c.symbol_path.clone()),
            path,
            content_hash: hash,
            language,
            source: SourceKind::Lexical,
            source_rank: 1,
            raw_score: score,
            detail: MatchDetail::Lexical { terms },
        }
    }
}

impl LexicalSource for LexicalAdapter<'_> {
    fn search_lexical(&self, request: &SourceRequest<'_>) -> Result<Vec<Candidate>, SourceError> {
        let query = request.plan.lexical_terms().join(" ");
        let mut found: Vec<(f64, Candidate)> = Vec::new();
        let mut answered = 0usize;
        for view in self.views {
            if request
                .scope
                .projects
                .as_ref()
                .is_some_and(|p| !p.contains(view.project()))
            {
                continue;
            }
            if let Ok(index) = &view.lexical {
                let scan = scoped_lexical_hits(
                    |fetch| self.query(|| index.search(&query, fetch)),
                    |hit| {
                        let Ok(path) = RepoPath::new(hit.id.as_str()) else {
                            return false;
                        };
                        let Some(file) = view.snapshot.file(&path) else {
                            return false;
                        };
                        let candidate = Self::candidate(
                            view.project(),
                            view.base_view.clone(),
                            view.generation(),
                            path.clone(),
                            file.content_hash,
                            view.language(&path),
                            None,
                            f64::from(hit.score),
                            hit.matched_terms.clone(),
                        );
                        candidate_allowed_in_sections(
                            request.scope,
                            &candidate,
                            self.kinds,
                            self.sections,
                        )
                    },
                    request.limit,
                );
                match scan {
                    Ok(scan) => {
                        answered = answered.saturating_add(1);
                        self.record_scan(&scan, view.project(), false, request.limit);
                        for hit in scan.hits {
                            let Ok(path) = RepoPath::new(hit.id.as_str()) else {
                                continue;
                            };
                            let Some(file) = view.snapshot.file(&path) else {
                                continue;
                            };
                            let chunks = view.snapshot.best_chunks(
                                &path,
                                &hit.matched_terms,
                                usize::from(self.spans_per_file),
                            );
                            let spans = if chunks.is_empty() {
                                vec![None]
                            } else {
                                chunks.into_iter().map(Some).collect()
                            };
                            for chunk in spans {
                                found.push((
                                    f64::from(hit.score),
                                    Self::candidate(
                                        view.project(),
                                        view.base_view.clone(),
                                        view.generation(),
                                        path.clone(),
                                        file.content_hash,
                                        view.language(&path),
                                        chunk,
                                        f64::from(hit.score),
                                        hit.matched_terms.clone(),
                                    ),
                                ));
                            }
                        }
                    }
                    Err(error) => self
                        .failures
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(Degradation::new(
                            Component::Lexical,
                            format!("{}: failed: {error}", view.project()),
                        )),
                }
            }
            if let Some(overlay) = &view.overlay {
                let scan = scoped_lexical_hits(
                    |fetch| self.query(|| overlay.overlay.search(&query, fetch)),
                    |hit| {
                        let Ok(path) = RepoPath::new(hit.id.as_str()) else {
                            return false;
                        };
                        let Some(file) = overlay.overlay.file(&path) else {
                            return false;
                        };
                        let candidate = Self::candidate(
                            view.project(),
                            overlay.view.clone(),
                            overlay.generation,
                            path.clone(),
                            file.content_hash,
                            overlay.language(&path),
                            None,
                            f64::from(hit.score),
                            hit.matched_terms.clone(),
                        );
                        candidate_allowed_in_sections(
                            request.scope,
                            &candidate,
                            self.kinds,
                            self.sections,
                        )
                    },
                    request.limit,
                );
                match scan {
                    Ok(scan) => {
                        answered = answered.saturating_add(1);
                        self.record_scan(&scan, view.project(), true, request.limit);
                        for hit in scan.hits {
                            let Ok(path) = RepoPath::new(hit.id.as_str()) else {
                                continue;
                            };
                            let Some(file) = overlay.overlay.file(&path) else {
                                continue;
                            };
                            let chunks = overlay
                                .chunks
                                .get(&path)
                                .map(|chunks| {
                                    best_chunks(
                                        chunks,
                                        &hit.matched_terms,
                                        usize::from(self.spans_per_file),
                                    )
                                })
                                .unwrap_or_default();
                            let spans = if chunks.is_empty() {
                                vec![None]
                            } else {
                                chunks.into_iter().map(Some).collect()
                            };
                            for chunk in spans {
                                found.push((
                                    f64::from(hit.score),
                                    Self::candidate(
                                        view.project(),
                                        overlay.view.clone(),
                                        overlay.generation,
                                        path.clone(),
                                        file.content_hash,
                                        overlay.language(&path),
                                        chunk,
                                        f64::from(hit.score),
                                        hit.matched_terms.clone(),
                                    ),
                                ));
                            }
                        }
                    }
                    Err(error) => self
                        .failures
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(Degradation::new(
                            Component::Lexical,
                            format!("{} personal layer: failed: {error}", view.project()),
                        )),
                }
            }
        }
        if answered == 0 && found.is_empty() {
            return Err(SourceError::Unavailable(
                "no lexical index is available for the pinned generations".to_owned(),
            ));
        }
        self.metrics
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .lexical_candidates = found.len();
        Ok(rank_lexical_and_limit(
            found,
            request.limit,
            request.per_project_limit,
        ))
    }
}

/// Explicit stored relationship semantics, independent of path naming.
/// Imports and references do not demonstrate a call or a test execution.
fn graph_edge_kind(kind: &str, incoming: bool) -> Option<EdgeKind> {
    match (kind, incoming) {
        ("calls", true) => Some(EdgeKind::Caller),
        ("calls", false) => Some(EdgeKind::Callee),
        ("tests", true) => Some(EdgeKind::Test),
        _ => None,
    }
}

/// Graph neighbours with explicit stored call or test relationships. File
/// imports remain structural metadata, not callers, callees or proof of tests.
pub(crate) struct GraphAdapter<'a> {
    pub(crate) prepared: &'a Prepared,
}

impl GraphExpander for GraphAdapter<'_> {
    fn neighbors(&self, request: &ExpandRequest<'_>) -> Result<Vec<Neighbor>, SourceError> {
        let location = &request.node.location;
        let Some((view, is_overlay)) = self.prepared.view_of(&location.project, &location.view)
        else {
            return Ok(Vec::new());
        };
        if is_overlay {
            return Ok(Vec::new());
        }
        let snapshot = &view.snapshot;
        let wants = |kind: EdgeKind| request.edges.contains(&kind);
        let mut out = Vec::new();
        // The edge's stored relation supplies semantics; evidence strength and
        // resolution remain separate and travel to the query layer.
        let symbol = request
            .node
            .symbol
            .as_deref()
            .and_then(|s| snapshot.symbol_by_local(&location.path, s))
            .or_else(|| {
                location
                    .range
                    .and_then(|r| snapshot.enclosing_symbol(&location.path, r))
            });
        if let Some(id) = symbol.and_then(|s| s.store_id) {
            let symbol_node = |node: &knowell_store::graph::NodeRef| -> Option<GraphNode> {
                let (path, lines, target) = snapshot.place_of(node)?;
                Some(GraphNode {
                    location: view.base_location(&path, lines)?,
                    symbol: target.map(|s| s.local.clone()),
                    language: view.language(&path),
                })
            };
            if wants(EdgeKind::Caller) || wants(EdgeKind::Test) {
                for edge in snapshot
                    .edges_into
                    .get(&id)
                    .into_iter()
                    .flatten()
                    .filter_map(|index| snapshot.other_edges.get(*index))
                {
                    let Some(kind) = graph_edge_kind(&edge.kind, true) else {
                        continue;
                    };
                    if !wants(kind) {
                        continue;
                    }
                    let Some(node) = symbol_node(&edge.from) else {
                        continue;
                    };
                    out.push(Neighbor {
                        node,
                        edge: kind,
                        evidence: map_evidence(edge.evidence),
                        resolution: map_resolution(edge.resolution),
                    });
                }
            }
            if wants(EdgeKind::Callee) {
                for edge in snapshot
                    .edges_from
                    .get(&id)
                    .into_iter()
                    .flatten()
                    .filter_map(|index| snapshot.other_edges.get(*index))
                {
                    let Some(kind) = graph_edge_kind(&edge.kind, false) else {
                        continue;
                    };
                    if !wants(kind) {
                        continue;
                    }
                    if let Some(node) = symbol_node(&edge.to) {
                        out.push(Neighbor {
                            node,
                            edge: kind,
                            evidence: map_evidence(edge.evidence),
                            resolution: map_resolution(edge.resolution),
                        });
                    }
                }
            }
        }
        out.sort_by(|a, b| {
            (a.edge, a.evidence, a.resolution, &a.node.location).cmp(&(
                b.edge,
                b.evidence,
                b.resolution,
                &b.node.location,
            ))
        });
        out.dedup_by(|a, b| a.edge == b.edge && a.node.location == b.node.location);
        out.truncate(request.limit);
        Ok(out)
    }
}

/// Snippets from prefetched file text (base views) and overlay files.
pub(crate) struct SnippetAdapter<'a> {
    pub(crate) prepared: &'a Prepared,
    pub(crate) texts: BTreeMap<ContentHash, Arc<str>>,
    pub(crate) max_lines: u32,
    /// Query cues enable source-region acquisition rather than prefix clipping.
    pub(crate) source_terms: Option<BTreeSet<String>>,
}

impl SnippetAdapter<'_> {
    /// Actual acquisition extent, including an enclosing declaration when the
    /// selected file's parser verifies one. Continuations address this extent,
    /// so a window is never presented as a complete declaration.
    pub(crate) fn source_extent(
        &self,
        location: &Location,
        symbol: Option<&str>,
    ) -> Option<LineRange> {
        let (text, _, _) = self.text_of(location)?;
        let total = snapshot::line_count(&text);
        let full = location.range.or_else(|| whole_file(total))?;
        if full.start() > total {
            return None;
        }
        let mut cited = location.clone();
        if location.range.is_none()
            && full.line_count() > self.max_lines
            && let Some(terms) = &self.source_terms
        {
            let window =
                crate::source_region::source_region(&text, full, None, terms, self.max_lines);
            if let Some(anchor) = crate::source_region::region_anchor(&text, window, terms) {
                cited.range = Some(LineRange::new(anchor, anchor).unwrap_or(window));
            }
        }
        let extent = self.body_extent(&cited, symbol).unwrap_or(full);
        LineRange::new(extent.start(), extent.end().min(total)).ok()
    }

    /// Bounds from persisted coordinates or the selected file's parser, never
    /// from a generated embedding header or a symbol's mutable current name.
    fn body_extent(&self, location: &Location, symbol: Option<&str>) -> Option<LineRange> {
        if let Some(found) = self.symbol_for(location, symbol) {
            return Some(found.lines);
        }
        let range = location.range?;
        let (view, is_overlay) = self.prepared.view_of(&location.project, &location.view)?;
        if is_overlay {
            return crate::source_region::enclosing_at(
                view.overlay
                    .as_ref()?
                    .symbols
                    .iter()
                    .filter(|entry| entry.path == location.path && entry.lines.end() >= range.end())
                    .map(|entry| entry.lines),
                range.start(),
            );
        }
        view.snapshot
            .chunks
            .get(&location.path)?
            .iter()
            .filter(|chunk| {
                chunk.lines.start() <= range.start() && range.end() <= chunk.lines.end()
            })
            .filter_map(|chunk| chunk.structure.as_ref())
            .flat_map(|structure| [structure.declaration.as_ref(), structure.enclosing.as_ref()])
            .flatten()
            .map(|region| region.lines)
            .filter(|region| region.start() <= range.start() && range.end() <= region.end())
            .min_by_key(|region| (region.line_count(), region.start(), region.end()))
    }

    fn text_of(&self, location: &Location) -> Option<(Arc<str>, Option<&SymbolEntry>, bool)> {
        let (view, is_overlay) = self.prepared.view_of(&location.project, &location.view)?;
        if is_overlay {
            let overlay = view.overlay.as_ref()?;
            let file = overlay.overlay.file(&location.path)?;
            if file.content_hash != location.content_hash {
                return None;
            }
            return Some((Arc::clone(&file.text), None, true));
        }
        if view.snapshot.file(&location.path)?.content_hash != location.content_hash {
            return None;
        }
        let text = self.texts.get(&location.content_hash)?;
        Some((Arc::clone(text), None, false))
    }

    fn symbol_for<'s>(
        &'s self,
        location: &Location,
        symbol: Option<&str>,
    ) -> Option<&'s SymbolEntry> {
        let (view, is_overlay) = self.prepared.view_of(&location.project, &location.view)?;
        let symbols: Box<dyn Iterator<Item = &SymbolEntry>> = if is_overlay {
            Box::new(
                view.overlay
                    .as_ref()?
                    .symbols
                    .iter()
                    .filter(|s| s.path == location.path),
            )
        } else {
            Box::new(view.snapshot.symbols_in(&location.path))
        };
        let symbols: Vec<&SymbolEntry> = symbols.collect();
        if let Some(name) = symbol
            && let Some(found) = symbols.iter().find(|s| {
                s.local == name
                    && location.range.is_none_or(|range| {
                        s.lines.start() <= range.start() && range.end() <= s.lines.end()
                    })
            })
        {
            return Some(found);
        }
        let range = location.range?;
        symbols
            .into_iter()
            .filter(|s| s.lines.start() <= range.start() && s.lines.end() >= range.end())
            .min_by_key(|s| (s.lines.line_count(), s.lines.start()))
    }
}

impl SnippetSource for SnippetAdapter<'_> {
    fn snippet(&self, request: &SnippetRequest<'_>) -> Result<Option<Snippet>, SourceError> {
        let location = request.location;
        let Some((text, _, _)) = self.text_of(location) else {
            return Ok(None);
        };
        let line_count = snapshot::line_count(&text);
        let full = location
            .range
            .or_else(|| whole_file(line_count))
            .ok_or_else(|| SourceError::Failed("empty line range".to_owned()))?;
        if self.source_terms.is_some() && full.end() > line_count {
            return Err(SourceError::Failed(
                "source range is outside the pinned file".to_owned(),
            ));
        }
        match request.kind {
            SnippetKind::Body => {
                if let Some(terms) = &self.source_terms {
                    let declaration = self.source_extent(location, request.symbol);
                    let retrieved = if location.range.is_none() {
                        declaration.unwrap_or(full)
                    } else {
                        full
                    };
                    let range = crate::source_region::source_region(
                        &text,
                        retrieved,
                        declaration,
                        terms,
                        self.max_lines,
                    );
                    return Ok(Some(Snippet {
                        text: crate::source_region::slice_source_lines(&text, range),
                        range,
                        content_hash: location.content_hash,
                    }));
                }
                let end = full
                    .end()
                    .min(
                        full.start()
                            .saturating_add(self.max_lines.saturating_sub(1)),
                    )
                    .min(line_count.max(full.start()));
                let range = LineRange::new(full.start(), end.max(full.start()))
                    .map_err(|e| SourceError::Failed(e.to_string()))?;
                Ok(Some(Snippet {
                    text: crate::source_region::slice_source_lines(&text, range),
                    range,
                    content_hash: location.content_hash,
                }))
            }
            SnippetKind::Skeleton => {
                if let Some(symbol) = self.symbol_for(location, request.symbol) {
                    let mut out = String::new();
                    if let Some(doc) = &symbol.doc {
                        for line in doc.lines() {
                            out.push_str("/// ");
                            out.push_str(line);
                            out.push('\n');
                        }
                    }
                    out.push_str(&symbol.signature);
                    let sig_lines =
                        u32::try_from(symbol.signature.lines().count().max(1)).unwrap_or(1);
                    let start = symbol.name_line.max(symbol.lines.start());
                    let end = start
                        .saturating_add(sig_lines.saturating_sub(1))
                        .min(symbol.lines.end())
                        .max(start);
                    let range = LineRange::new(start, end)
                        .map_err(|e| SourceError::Failed(e.to_string()))?;
                    return Ok(Some(Snippet {
                        text: out,
                        range,
                        content_hash: location.content_hash,
                    }));
                }
                if location.range.is_some() {
                    return Ok(None);
                }
                let parsed = knowell_parse::parse(&location.path, &text);
                match knowell_parse::skeleton(&parsed, &text) {
                    Ok(outline) if !outline.trim().is_empty() => Ok(Some(Snippet {
                        text: outline,
                        range: full,
                        content_hash: location.content_hash,
                    })),
                    _ => Ok(None),
                }
            }
        }
    }
}

/// What one search produced, with the prepared views for evidence.
pub(crate) struct SearchRun {
    pub(crate) response: SearchResponse,
    pub(crate) prepared: Prepared,
    pub(crate) retrieval: RetrievalMetrics,
}

/// Cheap acquisition cues, not a proof that a natural-language task has all
/// its evidence. A larger fused pool reuses the same lexical/vector candidates.
fn should_widen_source_pool(response: &SearchResponse, bound: usize) -> bool {
    if response.results.len() >= bound || response.stats.truncated_by_limit == 0 {
        return false;
    }
    let files: BTreeSet<_> = response
        .results
        .iter()
        .map(|result| {
            (
                &result.location.project,
                &result.location.view,
                &result.location.path,
            )
        })
        .collect();
    let requested = crate::source_region::region_terms(&response.plan.query);
    let matched: BTreeSet<_> = response
        .results
        .iter()
        .flat_map(|result| &result.why)
        .flat_map(|reason| match reason {
            knowell_query::Reason::ExactMatch { term, .. } => terms_of(term),
            knowell_query::Reason::LexicalTerms { terms } => terms_of(&terms.join(" ")),
            _ => BTreeSet::new(),
        })
        .collect();
    source_pool_needs_more(
        response.results.len(),
        bound,
        files.len(),
        !requested.is_subset(&matched),
    )
}

fn source_pool_needs_more(returned: usize, bound: usize, files: usize, missing_cues: bool) -> bool {
    returned < bound
        && (returned < bound.min(32) || files.saturating_mul(2) < returned || missing_cues)
}

impl Engine {
    /// Source-free metadata, cached separately from complete graph snapshots.
    pub(crate) async fn metadata_snapshot_of(
        &self,
        project: &PinnedProject,
    ) -> Result<Arc<Snapshot>, ToolError> {
        let pin = project.pin();
        let cell = self.inner.snapshots.metadata_cell(pin);
        let inner = &self.inner;
        let snapshot = cell
            .get_or_try_init(|| async {
                snapshot::build_metadata(
                    &inner.store,
                    inner.organization,
                    project.entry.name.clone(),
                    project.entry.id,
                    pin,
                )
                .await
                .map(Arc::new)
            })
            .await
            .map_err(ToolError::from)?;
        Ok(Arc::clone(snapshot))
    }

    /// Loads (or reuses) the snapshot of a pinned project.
    pub(crate) async fn snapshot_of(
        &self,
        project: &PinnedProject,
    ) -> Result<Arc<Snapshot>, ToolError> {
        let pin = project.pin();
        let cell = self.inner.snapshots.cell(pin);
        let inner = &self.inner;
        let snapshot = cell
            .get_or_try_init(|| async {
                snapshot::build(
                    &inner.store,
                    &inner.texts,
                    inner.organization,
                    project.entry.name.clone(),
                    project.entry.id,
                    pin,
                    snapshot::ParseOptions {
                        limits: inner.indexer.config().parse_limits,
                        products: inner.parse_products.clone(),
                    },
                )
                .await
                .map(Arc::new)
            })
            .await
            .map_err(ToolError::from)?;
        Ok(Arc::clone(snapshot))
    }

    /// The Tantivy index serving exactly the pinned generation.
    async fn lexical_for(&self, project: &PinnedProject) -> Result<Arc<LexicalIndex>, String> {
        if project.view != project.entry.view {
            return Err(format!(
                "{}@{} is not served by this engine's indexer",
                project.entry.name, project.target
            ));
        }
        let active = async {
            let mut conn = self.inner.store.acquire().await.ok()?;
            views::get_view(&mut conn, project.view)
                .await
                .ok()
                .flatten()
                .and_then(|v| v.active_generation)
        };
        if active.await != Some(project.generation) {
            return Err(format!(
                "{}: generation {} is no longer the served one; call open_workspace again",
                project.entry.name, project.generation
            ));
        }
        match self.inner.indexer.lexical(project.view).await {
            Ok(Some(index)) => Ok(index),
            Ok(None) => Err(format!("{}: no lexical index yet", project.entry.name)),
            Err(error) => {
                tracing::warn!(error = %error, "opening a lexical index failed");
                Err(format!(
                    "{}: the lexical index could not be opened",
                    project.entry.name
                ))
            }
        }
    }

    /// Prepares every pinned project (snapshot, lexical index, overlay) and
    /// the query scope with its view manifest.
    pub(crate) async fn prepare(
        &self,
        pinned: &Pinned,
        filters: &Filters,
        metadata_only: bool,
    ) -> Result<Prepared, ToolError> {
        let mut views = Vec::new();
        let mut degraded = Vec::new();
        let mut manifest = ViewManifest::new(pinned.workspace.name.clone());
        manifest.not_indexed = pinned.not_indexed.clone();
        let chunk_options = self.inner.indexer.config().chunking;
        for (name, project) in &pinned.projects {
            if filters
                .projects
                .as_ref()
                .is_some_and(|projects| !projects.contains(name))
            {
                continue;
            }
            let snapshot = if metadata_only {
                self.metadata_snapshot_of(project).await?
            } else {
                self.snapshot_of(project).await?
            };
            let lexical = self.lexical_for(project).await;
            if let Err(reason) = &lexical {
                degraded.push(Degradation::new(Component::Lexical, reason.clone()));
            }
            let base_view = base_view_id(project.view)?;
            let mut languages: BTreeSet<_> = snapshot
                .languages()
                .keys()
                .filter_map(|language| Language::new(language).ok())
                .collect();
            let overlay = match &project.overlay {
                Some(o) => {
                    let mut symbols = Vec::new();
                    let mut chunks = BTreeMap::new();
                    for file in o.overlay.files() {
                        // Only prepared, admitted files add capabilities; a new code
                        // language still lacks call/test analysis in the personal layer.
                        if let Ok(language) = Language::new(file.parsed.language.as_str()) {
                            languages.insert(language);
                        }
                        let base = symbols.len();
                        symbols.extend(symbol_entries(&file.path, &file.parsed, base));
                        chunks.insert(file.path.clone(), overlay_chunks(file, &chunk_options));
                    }
                    if !o.overlay.is_empty() {
                        degraded.push(Degradation::new(
                            Component::Semantic,
                            format!(
                                "{name}: personal-layer files are not embedded; they match by words and symbols only"
                            ),
                        ));
                    }
                    Some(PreparedOverlay {
                        view: overlay_view_id(project.view, o.owner, o.generation)?,
                        generation: o.generation,
                        overlay: Arc::clone(&o.overlay),
                        symbols,
                        chunks,
                    })
                }
                None => None,
            };
            let commit = project
                .commit
                .as_deref()
                .and_then(|c| knowell_query::CommitId::new(c).ok());
            manifest.projects.insert(
                name.clone(),
                ProjectPin {
                    base: PinnedView {
                        view: base_view.clone(),
                        generation: u64::try_from(project.generation).unwrap_or(0),
                        commit: commit.clone(),
                    },
                    overlay: overlay.as_ref().map(|o| OverlayPin {
                        pin: PinnedView {
                            view: o.view.clone(),
                            generation: o.generation,
                            commit: o
                                .overlay
                                .head_commit()
                                .and_then(|c| knowell_query::CommitId::new(c).ok())
                                .or(commit),
                        },
                        shadowed_paths: o.overlay.shadowed_paths(),
                    }),
                    coverage: ProjectCoverage {
                        languages,
                        reference_resolution: BTreeSet::new(),
                    },
                },
            );
            views.push(PreparedView {
                pinned: project.clone(),
                snapshot,
                base_view,
                lexical,
                overlay,
            });
        }
        let scope = QueryScope {
            workspace: pinned.workspace.name.clone(),
            projects: filters.projects.clone(),
            languages: filters.languages.clone(),
            paths: PathFilter {
                include: Vec::new(),
                exclude: Vec::new(),
                prefixes: filters.path_prefixes.clone(),
            },
            domain: None,
            manifest,
        };
        Ok(Prepared {
            views,
            scope,
            degraded,
        })
    }

    /// The profile the view served when its context was pinned, if that
    /// profile's vectors cover the pinned generation completely (an index
    /// generation that is active, or retired by a newer one: its vectors are
    /// kept). Never another profile in its place.
    async fn serving_profile(&self, view: &PreparedView) -> Result<Option<ProfileId>, String> {
        let Some(profile) = view.pinned.serving_profile else {
            return Ok(None);
        };
        let mut conn = self
            .inner
            .store
            .acquire()
            .await
            .map_err(|e| format!("store: {e}"))?;
        let covered = embeddings::index_generation_at(&mut conn, view.pinned.pin(), profile)
            .await
            .map_err(|e| format!("store: {e}"))?
            .is_some_and(|ig| {
                matches!(
                    ig.state,
                    knowell_store::GenerationState::Active
                        | knowell_store::GenerationState::Retired
                )
            });
        Ok(covered.then_some(profile))
    }

    /// The configured provider whose embedder produces `profile` (by
    /// identity), remembered once found.
    async fn provider_of(&self, profile: ProfileId) -> Result<Option<Name>, String> {
        if let Some(known) = self
            .inner
            .profile_providers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&profile)
            .cloned()
        {
            return Ok(Some(known));
        }
        let mut conn = self
            .inner
            .store
            .acquire()
            .await
            .map_err(|e| format!("store: {e}"))?;
        let Some(stored) = embeddings::get_profile(&mut conn, profile)
            .await
            .map_err(|e| format!("store: {e}"))?
        else {
            return Ok(None);
        };
        let found = self.provider_for(&stored);
        if let Some(name) = &found {
            self.inner
                .profile_providers
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(profile, name.clone());
        }
        Ok(found)
    }

    /// Vector candidates: one `nearest` search per embedding profile over
    /// the views whose vectors cover exactly the pinned generation. Profiles
    /// are never mixed: their lists are interleaved by rank. A cloud
    /// provider is never asked on behalf of a local-only project.
    async fn semantic(
        &self,
        plan: &QueryPlan,
        prepared: &Prepared,
        filters: &Filters,
        limit: usize,
        metrics: &mut RetrievalMetrics,
    ) -> (SourceStatus, Vec<Degradation>) {
        let mut notes = Vec::new();
        let mut failures = Vec::new();
        let mut groups: BTreeMap<ProfileId, Vec<&PreparedView>> = BTreeMap::new();
        for view in &prepared.views {
            if prepared
                .scope
                .projects
                .as_ref()
                .is_some_and(|p| !p.contains(view.project()))
            {
                continue;
            }
            let project = view.project();
            let serving = match self.serving_profile(view).await {
                Ok(serving) => serving,
                Err(reason) => {
                    failures.push(reason);
                    continue;
                }
            };
            let Some(profile) = serving else {
                let text = match &view.pinned.entry.embedding {
                    EmbeddingPlan::Skip {
                        reason: TierSkip::NoProvider,
                    } => format!("{project} has no embedding provider"),
                    EmbeddingPlan::Skip {
                        reason: TierSkip::DataPolicyLocalOnly,
                    } => format!(
                        "{project} is local-only and its provider is a cloud service; nothing was sent"
                    ),
                    EmbeddingPlan::Skip {
                        reason: TierSkip::BudgetExhausted,
                    } => format!("{project}: the embedding budget is exhausted"),
                    EmbeddingPlan::Unavailable { reason } => format!("{project}: {reason}"),
                    EmbeddingPlan::Embed { .. } => format!(
                        "embeddings of {project} generation {} are not ready",
                        view.pinned.generation
                    ),
                };
                notes.push(Degradation::new(Component::Semantic, text));
                continue;
            };
            let provider = match self.provider_of(profile).await {
                Ok(provider) => provider,
                Err(reason) => {
                    failures.push(reason);
                    continue;
                }
            };
            let Some(provider) = provider else {
                notes.push(Degradation::new(
                    Component::Semantic,
                    format!("{project}: no provider of its embedding profile is configured"),
                ));
                continue;
            };
            if view.pinned.entry.data_policy == knowell_config::DataPolicy::LocalOnly
                && self.provider_is_cloud(&provider)
            {
                notes.push(Degradation::new(
                    Component::Semantic,
                    format!(
                        "{project} is local-only and its provider is a cloud service; nothing was sent"
                    ),
                ));
                continue;
            }
            groups.entry(profile).or_default().push(view);
        }
        let mut lists: Vec<Vec<Candidate>> = Vec::new();
        for (profile_id, views) in groups {
            match self
                .semantic_group(plan, profile_id, &views, prepared, filters, limit, metrics)
                .await
            {
                Ok((list, capped)) => {
                    lists.push(list);
                    if capped {
                        notes.push(Degradation::new(
                            Component::Semantic,
                            "scoped semantic retrieval reached its 1000-neighbor bound; more matching evidence may exist",
                        ));
                    }
                }
                Err(reason) => failures.push(reason),
            }
        }
        if lists.is_empty() {
            let status = if failures.is_empty() {
                SourceStatus::Failed(SourceError::Unavailable(
                    "no searched project has embeddings for its pinned generation".to_owned(),
                ))
            } else {
                SourceStatus::Failed(SourceError::Failed(failures.join("; ")))
            };
            return (status, notes);
        }
        for reason in failures {
            notes.push(Degradation::new(
                Component::Semantic,
                format!("failed: {reason}"),
            ));
        }
        // Interleave profiles by rank; vectors of different profiles are
        // never compared.
        let mut merged = Vec::new();
        let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
        for rank in 0..longest {
            for list in &lists {
                if let Some(c) = list.get(rank) {
                    merged.push(c.clone());
                }
            }
        }
        for (i, candidate) in merged.iter_mut().enumerate() {
            candidate.source_rank = u32::try_from(i.saturating_add(1)).unwrap_or(u32::MAX);
        }
        merged.truncate(limit);
        (SourceStatus::Answered(merged), notes)
    }

    /// One profile's nearest-neighbour search over `views` (all served by
    /// that profile at their pinned generations).
    #[allow(clippy::too_many_arguments)]
    async fn semantic_group(
        &self,
        plan: &QueryPlan,
        profile_id: ProfileId,
        views: &[&PreparedView],
        prepared: &Prepared,
        filters: &Filters,
        limit: usize,
        metrics: &mut RetrievalMetrics,
    ) -> Result<(Vec<Candidate>, bool), String> {
        let provider = self
            .provider_of(profile_id)
            .await?
            .ok_or_else(|| "the embedding profile has no provider".to_owned())?;
        let Some(embedder) = self.inner.embedders.get(&provider) else {
            return Err(format!(
                "provider `{provider}` is not configured in this engine"
            ));
        };
        let mut conn = self
            .inner
            .store
            .acquire()
            .await
            .map_err(|e| format!("store: {e}"))?;
        let Some(profile) = embeddings::get_profile(&mut conn, profile_id)
            .await
            .map_err(|e| format!("store: {e}"))?
        else {
            return Err("the embedding profile is not registered".to_owned());
        };
        drop(conn);
        metrics.embedding_calls = metrics.embedding_calls.saturating_add(1);
        let embedded = embedder.embed_query_with_usage(&plan.semantic_text()).await;
        let (vector, usage) = match embedded {
            Ok(embedded) => embedded,
            Err(error) => {
                metrics.embedding_failures = metrics.embedding_failures.saturating_add(1);
                return Err(format!("embedding the query with `{provider}`: {error}"));
            }
        };
        metrics.embedding_usage.merge(&usage);
        let pins: Vec<GenerationPin> = views.iter().map(|v| v.pinned.pin()).collect();
        let max = usize::try_from(embeddings::MAX_K).unwrap_or(1000);
        let mut k = u32::try_from(limit.clamp(1, max)).unwrap_or(100);
        let mut conn = self
            .inner
            .store
            .acquire()
            .await
            .map_err(|e| format!("store: {e}"))?;
        loop {
            let options = NearestOptions {
                k,
                ef_search: Some(k.max(embeddings::DEFAULT_EF_SEARCH)),
                scope: Some(pins.clone()),
                path_prefixes: filters.path_prefixes.clone(),
                languages: filters
                    .languages
                    .as_ref()
                    .map(|languages| {
                        languages
                            .iter()
                            .map(|language| language.as_str().to_owned())
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            metrics.semantic_queries = metrics.semantic_queries.saturating_add(1);
            let neighbors = embeddings::nearest(&mut conn, &profile, vector.as_slice(), &options)
                .await
                .map_err(|e| format!("vector search: {e}"))?;
            metrics.semantic_neighbors_examined = metrics
                .semantic_neighbors_examined
                .saturating_add(neighbors.len());
            let exhausted = neighbors.len() < usize::try_from(k).unwrap_or(usize::MAX);
            let hashes: Vec<ContentHash> =
                neighbors.iter().map(|n| n.prepared_input_hash).collect();
            // Per-path chunk inputs (the embedding input includes the path, so
            // identical content at two paths has two inputs); generations indexed
            // before per-path inputs existed fall back to content-level rows.
            let mut locations =
                content::locate_chunk_inputs(&mut conn, self.inner.organization, &pins, &hashes)
                    .await
                    .map_err(|e| format!("store: {e}"))?;
            let located: BTreeSet<_> = locations
                .iter()
                .map(|location| location.chunk.prepared_input_hash)
                .collect();
            let missing: Vec<_> = hashes
                .iter()
                .filter(|hash| !located.contains(*hash))
                .copied()
                .collect();
            if !missing.is_empty() {
                let legacy = content::locate_prepared_inputs(
                    &mut conn,
                    self.inner.organization,
                    &pins,
                    &missing,
                )
                .await
                .map_err(|e| format!("store: {e}"))?;
                locations.extend(legacy);
            }
            let mut by_hash: BTreeMap<ContentHash, Vec<&content::ChunkLocation>> = BTreeMap::new();
            for location in &locations {
                by_hash
                    .entry(location.chunk.prepared_input_hash)
                    .or_default()
                    .push(location);
            }
            let mut list = Vec::new();
            for neighbor in &neighbors {
                for location in by_hash
                    .get(&neighbor.prepared_input_hash)
                    .into_iter()
                    .flatten()
                {
                    let Some(view) = views.iter().find(|v| v.pinned.pin() == location.pin) else {
                        continue;
                    };
                    let candidate = Candidate {
                        id: format!(
                            "vec:{}:{}:{}",
                            view.base_view, location.path, location.chunk.ordinal
                        ),
                        project: view.project().clone(),
                        view: view.base_view.clone(),
                        generation: view.generation(),
                        path: location.path.clone(),
                        range: Some(location.chunk.lines),
                        content_hash: location.chunk.content_hash,
                        symbol: location.chunk.symbol_path.clone(),
                        language: view.language(&location.path),
                        source: SourceKind::Semantic,
                        source_rank: u32::try_from(list.len().saturating_add(1))
                            .unwrap_or(u32::MAX),
                        raw_score: 1.0 - neighbor.distance,
                        detail: MatchDetail::Semantic {
                            profile: profile.name.to_string(),
                        },
                    };
                    if candidate_allowed_in_sections(
                        &prepared.scope,
                        &candidate,
                        &filters.kinds,
                        &filters.sections,
                    ) {
                        list.push(candidate);
                    }
                }
            }
            if list.len() >= limit || exhausted || k >= embeddings::MAX_K {
                let capped = list.len() < limit && !exhausted && k >= embeddings::MAX_K;
                list.truncate(limit);
                for (rank, candidate) in list.iter_mut().enumerate() {
                    candidate.source_rank =
                        u32::try_from(rank.saturating_add(1)).unwrap_or(u32::MAX);
                }
                return Ok((list, capped));
            }
            k = k.saturating_mul(2).min(embeddings::MAX_K);
        }
    }

    /// Runs the whole pipeline for `query` over a pinned manifest.
    pub(crate) async fn run_search(
        &self,
        pinned: &Pinned,
        filters: &Filters,
        query: &str,
        limit: usize,
        expand: bool,
        rerank: bool,
    ) -> Result<SearchRun, ToolError> {
        self.run_search_policy(pinned, filters, query, limit, expand, rerank, false)
            .await
    }

    /// Retrieval for source responses. Candidate admission is widened inside
    /// one retrieved pool, so widening never repeats an embedding request.
    pub(crate) async fn run_source_search(
        &self,
        pinned: &Pinned,
        filters: &Filters,
        query: &str,
        limit: usize,
    ) -> Result<SearchRun, ToolError> {
        self.run_search_policy(pinned, filters, query, limit, true, false, true)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_search_policy(
        &self,
        pinned: &Pinned,
        filters: &Filters,
        query: &str,
        limit: usize,
        expand: bool,
        rerank: bool,
        source_policy: bool,
    ) -> Result<SearchRun, ToolError> {
        let preparation_started = Instant::now();
        let mut filters = filters.clone();
        filters.path_prefixes = normalize_prefixes(&filters.path_prefixes)?;
        let mut prepared = self.prepare(pinned, &filters, source_policy).await?;
        let mut retrieval = RetrievalMetrics {
            preparation_ms: u64::try_from(preparation_started.elapsed().as_millis())
                .unwrap_or(u64::MAX),
            prepared_views: prepared.views.len(),
            ..Default::default()
        };
        let mut config: SearchConfig = self.inner.settings.search.clone();
        let result_limit = limit.clamp(1, if source_policy { 64 } else { 1000 });
        config.fusion.result_limit = if source_policy {
            result_limit.min(16)
        } else {
            result_limit
        };
        if source_policy {
            config.fusion.candidate_limit = config
                .fusion
                .candidate_limit
                .max(result_limit.saturating_mul(2))
                .min(256);
        }
        config.expansion.enabled = expand && config.expansion.enabled;
        config.rerank.enabled = rerank;
        let plan =
            knowell_query::plan_with(query, &self.inner.glossary, &PlanOptions::default(), None);
        let searched = !plan.is_empty() && prepared.scope.searched_projects().next().is_some();
        let weights = config.fusion.weights.for_intent(plan.intent);
        let request = SourceRequest {
            plan: &plan,
            scope: &prepared.scope,
            limit: config.fusion.candidate_limit,
            per_project_limit: config.fusion.candidate_quota_per_project,
        };
        let mut extra = prepared.degraded.clone();
        let exact_started = Instant::now();
        let exact = if !searched || weights.exact <= 0.0 {
            SourceStatus::NotConsulted
        } else {
            let adapter = ExactAdapter {
                views: &prepared.views,
                kinds: &filters.kinds,
                sections: &filters.sections,
            };
            match adapter.search_exact(&request) {
                Ok(list) => SourceStatus::Answered(list),
                Err(error) => SourceStatus::Failed(error),
            }
        };
        retrieval.exact_ms = u64::try_from(exact_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let lexical_started = Instant::now();
        let lexical = if !searched || weights.lexical <= 0.0 {
            SourceStatus::NotConsulted
        } else {
            let adapter = LexicalAdapter {
                views: &prepared.views,
                kinds: &filters.kinds,
                sections: &filters.sections,
                spans_per_file: self.inner.settings.lexical_spans_per_file,
                failures: Mutex::new(Vec::new()),
                metrics: Mutex::new(RetrievalMetrics::default()),
            };
            let status = match adapter.search_lexical(&request) {
                Ok(list) => SourceStatus::Answered(list),
                Err(error) => SourceStatus::Failed(error),
            };
            extra.extend(
                adapter
                    .failures
                    .into_inner()
                    .unwrap_or_else(PoisonError::into_inner),
            );
            let lexical = adapter
                .metrics
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner);
            retrieval.lexical_queries = lexical.lexical_queries;
            retrieval.lexical_file_hits_examined = lexical.lexical_file_hits_examined;
            retrieval.lexical_candidates = lexical.lexical_candidates;
            status
        };
        retrieval.lexical_ms =
            u64::try_from(lexical_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        retrieval.exact_path_embedding_bypass = exact_path_answered(&plan, &exact);
        let semantic_started = Instant::now();
        let semantic =
            if !searched || weights.semantic <= 0.0 || retrieval.exact_path_embedding_bypass {
                SourceStatus::NotConsulted
            } else {
                let (status, notes) = self
                    .semantic(
                        &plan,
                        &prepared,
                        &filters,
                        config.fusion.candidate_limit,
                        &mut retrieval,
                    )
                    .await;
                extra.extend(notes);
                status
            };
        retrieval.semantic_ms =
            u64::try_from(semantic_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let lists = SourceLists {
            exact,
            lexical,
            semantic,
        };
        let graph = GraphAdapter {
            prepared: &prepared,
        };
        let fusion_started = Instant::now();
        let mut response = knowell_query::search_with_candidates(
            &plan,
            &prepared.scope,
            lists.clone(),
            Some(&graph),
            None,
            &config,
        )
        .map_err(|e| ToolError::invalid_input(e.to_string()))?;
        while source_policy && should_widen_source_pool(&response, result_limit) {
            config.fusion.result_limit = config
                .fusion
                .result_limit
                .saturating_mul(2)
                .min(result_limit);
            response = knowell_query::search_with_candidates(
                &plan,
                &prepared.scope,
                lists.clone(),
                Some(&graph),
                None,
                &config,
            )
            .map_err(|error| ToolError::invalid_input(error.to_string()))?;
        }
        retrieval.fusion_expansion_ms =
            u64::try_from(fusion_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        for degradation in extra {
            if !response.degraded.contains(&degradation) {
                response.degraded.push(degradation);
            }
        }
        if source_policy {
            let hydration_started = Instant::now();
            self.hydrate_source_results(&mut prepared, &response)
                .await?;
            retrieval.source_hydration_ms =
                u64::try_from(hydration_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        }
        tracing::debug!(metrics = ?retrieval, "search retrieval work");
        Ok(SearchRun {
            response,
            prepared,
            retrieval,
        })
    }

    /// Only result and expanded paths need source bodies. Metadata and graph
    /// tools retain their own complete build path; hydration cannot widen scope.
    async fn hydrate_source_results(
        &self,
        prepared: &mut Prepared,
        response: &SearchResponse,
    ) -> Result<(), ToolError> {
        let mut by_view: BTreeMap<(Name, QViewId), BTreeSet<RepoPath>> = BTreeMap::new();
        let locations = knowell_query::task_source_locations(response, 64)
            .map_err(|error| ToolError::internal(error.to_string()))?;
        for location in locations {
            if let Some((view, false)) = prepared.view_of(&location.project, &location.view)
                && prepared.scope.paths.admits(&location.path)
                && view
                    .snapshot
                    .file(&location.path)
                    .is_some_and(|file| file.content_hash == location.content_hash)
            {
                by_view
                    .entry((location.project.clone(), location.view.clone()))
                    .or_default()
                    .insert(location.path.clone());
            }
        }
        for view in &mut prepared.views {
            let Some(paths) = by_view.remove(&(view.project().clone(), view.base_view.clone()))
            else {
                continue;
            };
            let paths: Vec<_> = paths.into_iter().collect();
            view.snapshot = Arc::new(
                snapshot::hydrate_paths(
                    &self.inner.store,
                    &self.inner.texts,
                    self.inner.organization,
                    &view.snapshot,
                    &paths,
                    snapshot::ParseOptions {
                        limits: self.inner.indexer.config().parse_limits,
                        products: self.inner.parse_products.clone(),
                    },
                )
                .await
                .map_err(ToolError::from)?,
            );
        }
        Ok(())
    }

    /// Texts of every result and expanded item, for snippets and packing.
    pub(crate) async fn texts_for(
        &self,
        run: &SearchRun,
    ) -> Result<BTreeMap<ContentHash, Arc<str>>, ToolError> {
        let locations: Vec<_> = run
            .response
            .results
            .iter()
            .map(|result| &result.location)
            .chain(run.response.expanded.iter().map(|item| &item.location))
            .collect();
        self.texts_of_locations(&run.prepared, &locations).await
    }

    /// Reads the same bounded candidate admission used by Source packing.
    /// Unselected graph neighbors must not cause source transfer or parsing.
    pub(crate) async fn source_texts_for(
        &self,
        run: &SearchRun,
    ) -> Result<BTreeMap<ContentHash, Arc<str>>, ToolError> {
        let locations = knowell_query::task_source_locations(&run.response, 64)
            .map_err(|error| ToolError::internal(error.to_string()))?;
        self.texts_of_locations(&run.prepared, &locations).await
    }

    async fn texts_of_locations(
        &self,
        prepared: &Prepared,
        locations: &[&Location],
    ) -> Result<BTreeMap<ContentHash, Arc<str>>, ToolError> {
        let mut hashes = BTreeSet::new();
        for location in locations {
            if let Some((_, false)) = prepared.view_of(&location.project, &location.view) {
                hashes.insert(location.content_hash);
            }
        }
        let mut out = BTreeMap::new();
        for hash in hashes {
            if let Some(text) = self
                .inner
                .texts
                .load(&self.inner.store, self.inner.organization, &hash)
                .await
                .map_err(ToolError::from)?
            {
                out.insert(hash, text);
            }
        }
        Ok(out)
    }
}

/// A snippet adapter over a finished run.
pub(crate) fn snippets<'a>(
    run: &'a SearchRun,
    texts: BTreeMap<ContentHash, Arc<str>>,
    max_lines: u32,
) -> SnippetAdapter<'a> {
    SnippetAdapter {
        prepared: &run.prepared,
        texts,
        max_lines,
        source_terms: None,
    }
}

/// A body-only region adapter that preserves short declarations and searches
/// long ones around the query and retrieved span. No generative model runs.
pub(crate) fn source_snippets<'a>(
    run: &'a SearchRun,
    texts: BTreeMap<ContentHash, Arc<str>>,
    max_lines: u32,
) -> SnippetAdapter<'a> {
    SnippetAdapter {
        prepared: &run.prepared,
        texts,
        max_lines,
        source_terms: Some(crate::source_region::region_terms(&run.response.plan.query)),
    }
}

#[cfg(test)]
mod tests {
    use knowell_parse::SymbolKind;

    use super::*;

    fn symbol(local: &str) -> SymbolEntry {
        SymbolEntry {
            key: format!("a.ts#{local}"),
            local: local.into(),
            name: local.rsplit('.').next().unwrap().into(),
            kind: SymbolKind::Method,
            path: RepoPath::new("a.ts").unwrap(),
            lines: LineRange::new(1, 2).unwrap(),
            name_line: 1,
            signature: String::new(),
            doc: None,
            parent: None,
            store_id: None,
        }
    }

    #[test]
    fn symbol_matching_prefers_qualified_exact_names() {
        let s = symbol("SubscriptionService.cancelSubscription");
        assert_eq!(
            symbol_strength(&s, "SubscriptionService.cancelSubscription"),
            Some(6)
        );
        assert_eq!(
            symbol_strength(&s, "SubscriptionService::cancelSubscription"),
            Some(6)
        );
        assert_eq!(symbol_strength(&s, "cancelSubscription"), Some(3));
        assert_eq!(symbol_strength(&s, "cancelsubscription"), Some(2));
        assert_eq!(symbol_strength(&s, "Other.cancelSubscription"), None);
        let nested = symbol("billing.SubscriptionService.cancelSubscription");
        assert_eq!(
            symbol_strength(&nested, "SubscriptionService.cancelSubscription"),
            Some(5)
        );
        assert_eq!(short_of("A::b"), "b");
    }

    #[test]
    fn path_matching() {
        let p = RepoPath::new("src/billing/service.ts").unwrap();
        assert_eq!(path_strength(&p, "src/billing/service.ts"), Some(6));
        assert_eq!(path_strength(&p, "./src/billing/service.ts"), Some(6));
        assert_eq!(path_strength(&p, "billing/service.ts"), Some(5));
        assert_eq!(path_strength(&p, "service.ts"), Some(3));
        assert_eq!(path_strength(&p, "ice.ts"), None);
    }

    #[test]
    fn test_paths_are_recognised() {
        for path in [
            "src/a.spec.ts",
            "tests/api.rs",
            "pkg/a_test.go",
            "test_payments.py",
            "src/__tests__/x.tsx",
        ] {
            assert!(is_test_path(&RepoPath::new(path).unwrap()), "{path}");
        }
        assert!(!is_test_path(&RepoPath::new("src/contest.ts").unwrap()));
    }

    #[test]
    fn prefixes_are_literal_and_match_sql_starts_with() {
        let prefixes = normalize_prefixes(&[
            " src/pay ".into(),
            "docs/".into(),
            "lib".into(),
            "README".into(),
            "src/star*/".into(),
            "src/question?/".into(),
            "src/%_/".into(),
            "docs/".into(),
        ])
        .unwrap();
        let filter = PathFilter {
            prefixes: prefixes.clone(),
            ..PathFilter::default()
        };
        let p = |s| RepoPath::new(s).unwrap();
        for path in [
            "src/payments/a.ts",
            "src/pay.ts",
            "docs/x.md",
            "lib/a.rs",
            "library.rs",
            "README.md",
            "src/star*/file.rs",
            "src/question?/file.rs",
            "src/%_/file.rs",
            "src/billing/a.ts",
            "other/lib/a.rs",
            "nested/README.md",
            "src/starZZ/file.rs",
            "src/questionZ/file.rs",
            "src/XX/file.rs",
            "docs",
        ] {
            let path = p(path);
            assert_eq!(
                filter.admits(&path),
                prefixes
                    .iter()
                    .any(|prefix| path.as_str().starts_with(prefix)),
                "{path}"
            );
        }
        assert!(filter.admits(&p("README.md")));
        assert!(!filter.admits(&p("src/starZZ/file.rs")));
        assert_eq!(
            prefixes
                .iter()
                .filter(|prefix| prefix.as_str() == "docs/")
                .count(),
            1
        );
        for prefix in [
            "/abs",
            "../outside",
            "src//",
            "src/./file",
            "src\\file",
            "src/\n",
        ] {
            assert!(normalize_prefixes(&[prefix.into()]).is_err(), "{prefix:?}");
        }
    }

    fn candidate(path: &str, language: Option<&str>, detail: MatchDetail) -> Candidate {
        Candidate {
            id: format!("candidate:{path}"),
            project: Name::new("sample").unwrap(),
            view: QViewId::new("main").unwrap(),
            generation: 1,
            path: RepoPath::new(path).unwrap(),
            range: None,
            content_hash: ContentHash::of(b"synthetic"),
            symbol: None,
            language: language.map(|language| Language::new(language).unwrap()),
            source: detail.kind(),
            source_rank: 1,
            raw_score: 1.0,
            detail,
        }
    }

    fn test_scope() -> QueryScope {
        let mut manifest = ViewManifest::new(Name::new("sample-workspace").unwrap());
        manifest.projects.insert(
            Name::new("sample").unwrap(),
            ProjectPin {
                base: PinnedView {
                    view: QViewId::new("main").unwrap(),
                    generation: 1,
                    commit: None,
                },
                overlay: None,
                coverage: ProjectCoverage::default(),
            },
        );
        QueryScope::all(manifest)
    }

    fn lexical_detail() -> MatchDetail {
        MatchDetail::Lexical {
            terms: vec!["decode".into()],
        }
    }

    fn lexical_span(path: &str, start: u32, end: u32, terms: &[&str]) -> Candidate {
        let mut candidate = candidate(
            path,
            Some("rust"),
            MatchDetail::Lexical {
                terms: terms.iter().map(|term| (*term).into()).collect(),
            },
        );
        candidate.id = format!("lex:{path}:{start}:{end}");
        candidate.range = Some(LineRange::new(start, end).unwrap());
        candidate
    }

    #[test]
    fn lexical_waves_preserve_file_breadth_under_the_global_quota() {
        let found = vec![
            (
                10.0,
                lexical_span("src/a.rs", 10, 12, &["decode", "header", "objects"]),
            ),
            (
                10.0,
                lexical_span("src/a.rs", 30, 32, &["decode", "header"]),
            ),
            (10.0, lexical_span("src/a.rs", 50, 52, &["decode"])),
            (9.0, lexical_span("src/b.rs", 10, 12, &["decode"])),
            (8.0, lexical_span("src/c.rs", 10, 12, &["decode"])),
        ];
        let first_wave = rank_lexical_and_limit(found.clone(), 3, None);
        assert_eq!(
            first_wave
                .iter()
                .map(|hit| hit.id.as_str())
                .collect::<Vec<_>>(),
            [
                "lex:src/a.rs:10:12",
                "lex:src/b.rs:10:12",
                "lex:src/c.rs:10:12"
            ]
        );
        let all = rank_lexical_and_limit(found, 5, None);
        assert_eq!(
            all.iter().map(|hit| hit.id.as_str()).collect::<Vec<_>>(),
            [
                "lex:src/a.rs:10:12",
                "lex:src/b.rs:10:12",
                "lex:src/c.rs:10:12",
                "lex:src/a.rs:30:32",
                "lex:src/a.rs:50:52"
            ]
        );
        assert_eq!(
            all.iter().map(|hit| hit.source_rank).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn lexical_tied_files_use_file_identity_before_local_evidence() {
        let weak = lexical_span("src/a.rs", 10, 11, &["decode"]);
        let best = lexical_span("src/a.rs", 30, 34, &["decode", "header"]);
        let mut other = lexical_span("src/b.rs", 10, 11, &["decode", "header", "objects"]);
        other.symbol = Some("decode_all".into());
        let hits = rank_lexical_and_limit(vec![(7.0, other), (7.0, weak), (7.0, best)], 3, None);
        assert_eq!(
            hits.iter().map(|hit| hit.id.as_str()).collect::<Vec<_>>(),
            [
                "lex:src/a.rs:30:34",
                "lex:src/b.rs:10:11",
                "lex:src/a.rs:10:11"
            ]
        );
    }

    #[test]
    fn lexical_local_span_order_is_stable_and_duplicates_do_not_spend_quota() {
        let short = lexical_span("src/a.rs", 10, 11, &["decode"]);
        let best = lexical_span("src/a.rs", 30, 34, &["decode", "header"]);
        let mut symbol = lexical_span("src/a.rs", 60, 64, &["decode"]);
        symbol.symbol = Some("decode_object".into());
        let mut found = vec![
            (7.0, short),
            (7.0, symbol),
            (7.0, best.clone()),
            (7.0, best),
        ];
        let forward = rank_lexical_and_limit(found.clone(), 3, None);
        found.reverse();
        assert_eq!(forward, rank_lexical_and_limit(found, 3, None));
        assert_eq!(
            forward
                .iter()
                .map(|hit| hit.id.as_str())
                .collect::<Vec<_>>(),
            [
                "lex:src/a.rs:30:34",
                "lex:src/a.rs:60:64",
                "lex:src/a.rs:10:11"
            ]
        );
    }

    #[test]
    fn lexical_waves_keep_project_limits_and_contiguous_ranks() {
        let mut outside = lexical_span("src/c.rs", 10, 11, &["decode"]);
        outside.project = Name::new("second").unwrap();
        let found = vec![
            (10.0, lexical_span("src/a.rs", 10, 11, &["decode"])),
            (10.0, lexical_span("src/a.rs", 30, 31, &["decode"])),
            (9.0, lexical_span("src/b.rs", 10, 11, &["decode"])),
            (8.0, outside),
        ];
        let hits = rank_lexical_and_limit(found.clone(), 4, Some(1));
        assert_eq!(
            hits.iter().map(|hit| hit.id.as_str()).collect::<Vec<_>>(),
            ["lex:src/a.rs:10:11", "lex:src/c.rs:10:11"]
        );
        assert_eq!(
            hits.iter().map(|hit| hit.source_rank).collect::<Vec<_>>(),
            [1, 2]
        );
        assert!(rank_lexical_and_limit(found.clone(), 0, None).is_empty());
        assert!(rank_lexical_and_limit(found, 4, Some(0)).is_empty());
    }

    #[test]
    fn lexical_file_waves_separate_generations_and_content_hashes() {
        let original = lexical_span("src/a.rs", 10, 11, &["decode"]);
        let extra = lexical_span("src/a.rs", 30, 31, &["decode"]);
        let mut newer = original.clone();
        newer.id = "newer".into();
        newer.generation = 2;
        let mut changed = original.clone();
        changed.id = "changed".into();
        changed.content_hash = ContentHash::of(b"changed synthetic");
        let hits = rank_lexical_and_limit(
            vec![(7.0, extra), (7.0, newer), (7.0, changed), (7.0, original)],
            3,
            None,
        );
        let stamps: BTreeSet<_> = hits
            .iter()
            .map(|hit| (hit.generation, hit.content_hash))
            .collect();
        assert_eq!(stamps.len(), 3);
        assert!(
            hits.iter()
                .all(|hit| hit.range == Some(LineRange::new(10, 11).unwrap()))
        );
    }

    #[test]
    fn exact_ranking_keeps_its_score_and_location_order() {
        let detail = MatchDetail::Exact {
            term: "decode".into(),
            target: ExactTarget::Symbol,
        };
        let mut first = candidate("src/a.rs", Some("rust"), detail.clone());
        first.id = "first".into();
        first.range = Some(LineRange::new(10, 11).unwrap());
        let mut second = first.clone();
        second.id = "second".into();
        second.range = Some(LineRange::new(30, 31).unwrap());
        let other = candidate("src/b.rs", Some("rust"), detail);
        let hits = rank_and_limit(vec![(6.0, other), (7.0, second), (7.0, first)], 2, None);
        assert_eq!(
            hits.iter().map(|hit| hit.id.as_str()).collect::<Vec<_>>(),
            ["first", "second"]
        );
    }

    #[test]
    fn source_admission_checks_scope_version_and_shadowing_before_quota() {
        let mut scope = test_scope();
        scope.paths.prefixes = normalize_prefixes(&["src/".into()]).unwrap();
        scope.languages = Some([Language::new("rust").unwrap()].into());
        let base = candidate("src/decoder.rs", Some("rust"), lexical_detail());
        assert!(candidate_allowed(&scope, &base, &[]));
        let wrong_path = candidate("docs/decoder.md", Some("rust"), lexical_detail());
        assert!(!candidate_allowed(&scope, &wrong_path, &[]));
        let unknown_language = candidate("src/decoder.rs", None, lexical_detail());
        assert!(!candidate_allowed(&scope, &unknown_language, &[]));
        let mut stale = base.clone();
        stale.generation = 0;
        assert!(!candidate_allowed(&scope, &stale, &[]));
        let mut unauthorized = base.clone();
        unauthorized.project = Name::new("outside").unwrap();
        assert!(!candidate_allowed(&scope, &unauthorized, &[]));
        scope
            .manifest
            .projects
            .get_mut(&base.project)
            .unwrap()
            .overlay = Some(OverlayPin {
            pin: PinnedView {
                view: QViewId::new("personal").unwrap(),
                generation: 2,
                commit: None,
            },
            shadowed_paths: [base.path.clone()].into(),
        });
        assert!(!candidate_allowed(&scope, &base, &[]));
        let mut overlay = base;
        overlay.view = QViewId::new("personal").unwrap();
        overlay.generation = 2;
        assert!(candidate_allowed(&scope, &overlay, &[]));
    }

    #[test]
    fn source_kind_filters_match_the_public_search_categories() {
        let scope = test_scope();
        let doc = candidate("docs/reader.md", Some("markdown"), lexical_detail());
        let code = candidate("src/reader.rs", Some("rust"), lexical_detail());
        let test_doc = candidate("tests/reader.md", Some("markdown"), lexical_detail());
        let contract = candidate(
            "api.yml",
            Some("yaml"),
            MatchDetail::Exact {
                term: "GET /documents".into(),
                target: ExactTarget::Contract,
            },
        );
        assert!(candidate_allowed(&scope, &doc, &[SearchKind::Docs]));
        assert!(!candidate_allowed(&scope, &code, &[SearchKind::Docs]));
        assert!(candidate_allowed(&scope, &code, &[SearchKind::Code]));
        assert!(candidate_allowed(&scope, &test_doc, &[SearchKind::Code]));
        assert!(!candidate_allowed(&scope, &test_doc, &[SearchKind::Docs]));
        assert!(candidate_allowed(
            &scope,
            &contract,
            &[SearchKind::Contracts]
        ));
        assert!(!candidate_allowed(&scope, &contract, &[SearchKind::Code]));
    }

    #[test]
    fn context_sections_are_admitted_before_source_quotas() {
        let scope = test_scope();
        let doc = candidate("docs/reader.md", Some("markdown"), lexical_detail());
        let code = candidate("src/reader.rs", Some("rust"), lexical_detail());
        let test = candidate("tests/reader.rs", Some("rust"), lexical_detail());
        let contract = candidate(
            "api.yml",
            Some("yaml"),
            MatchDetail::Exact {
                term: "GET /documents".into(),
                target: ExactTarget::Contract,
            },
        );
        let allowed = |candidate, sections: &[ContextSection]| {
            candidate_allowed_in_sections(&scope, candidate, &[], sections)
        };
        assert!(allowed(&doc, &[ContextSection::Docs]));
        assert!(!allowed(&code, &[ContextSection::Docs]));
        assert!(allowed(&test, &[ContextSection::Tests]));
        assert!(!allowed(&code, &[ContextSection::Tests]));
        assert!(!allowed(&test, &[ContextSection::Code]));
        assert!(allowed(&contract, &[ContextSection::Contracts]));
        assert!(!allowed(&doc, &[ContextSection::Memory]));
        assert!(allowed(&doc, &[ContextSection::Docs, ContextSection::Code]));
        assert!(allowed(
            &code,
            &[ContextSection::Docs, ContextSection::Code]
        ));
    }

    #[test]
    fn graph_relationships_require_explicit_semantics() {
        assert_eq!(graph_edge_kind("calls", true), Some(EdgeKind::Caller));
        assert_eq!(graph_edge_kind("calls", false), Some(EdgeKind::Callee));
        assert_eq!(graph_edge_kind("tests", true), Some(EdgeKind::Test));
        assert_eq!(graph_edge_kind("tests", false), None);
        for kind in [
            "imports",
            "references",
            "implements",
            "documents",
            "unknown",
        ] {
            assert_eq!(graph_edge_kind(kind, true), None);
            assert_eq!(graph_edge_kind(kind, false), None);
        }
    }

    #[test]
    fn lexical_refill_reaches_a_valid_file_beyond_the_first_result_window() {
        let index = LexicalIndex::create_in_ram().unwrap();
        let mut writer = index.writer().unwrap();
        for path in ["a/one.rs", "a/two.rs", "a/three.rs", "z/target.rs"] {
            writer
                .add(knowell_lexical::LexicalDoc {
                    id: path,
                    path,
                    text: "fn decode() {}",
                })
                .unwrap();
        }
        writer.commit().unwrap();
        let scan = scoped_lexical_hits(
            |fetch| index.search("decode", fetch),
            |hit| hit.path.starts_with("z/"),
            1,
        )
        .unwrap();
        assert_eq!(
            scan.hits
                .iter()
                .map(|hit| hit.path.as_str())
                .collect::<Vec<_>>(),
            vec!["z/target.rs"]
        );
        assert!(scan.queries > 1);
        assert!(!scan.capped);
    }

    #[test]
    fn lexical_refill_is_bounded_and_does_not_hide_partial_coverage() {
        let mut requested = Vec::new();
        let scan = scoped_lexical_hits(
            |fetch| {
                requested.push(fetch);
                Ok::<_, ()>(
                    (0..fetch)
                        .map(|id| LexicalHit {
                            id: format!("src/{id}.rs"),
                            path: format!("src/{id}.rs"),
                            score: 1.0,
                            matched_terms: vec!["decode".into()],
                        })
                        .collect(),
                )
            },
            |_| false,
            1,
        )
        .unwrap();
        assert!(scan.hits.is_empty());
        assert!(scan.capped);
        assert_eq!(requested.last(), Some(&MAX_LEXICAL_REFILL));
        assert!(requested.len() <= 14);
        let mut called = false;
        let empty = scoped_lexical_hits(
            |_| {
                called = true;
                Ok::<_, ()>(Vec::new())
            },
            |_| true,
            0,
        )
        .unwrap();
        assert!(!called);
        assert!(empty.hits.is_empty());
        let failed = scoped_lexical_hits(|_| Err::<Vec<LexicalHit>, _>("unavailable"), |_| true, 1);
        assert!(matches!(failed, Err("unavailable")));
    }

    #[test]
    fn lexical_span_reasons_and_ids_describe_the_returned_source_span() {
        let project = Name::new("sample").unwrap();
        let view = QViewId::new("main").unwrap();
        let path = RepoPath::new("src/reader.rs").unwrap();
        let chunk = |start, text: &str| ChunkEntry {
            lines: LineRange::new(start, start + 1).unwrap(),
            bytes: u64::try_from(text.len()).unwrap(),
            kind: "function".into(),
            symbol_path: Some(format!("decode_{start}")),
            terms: terms_of(text),
            structure: None,
        };
        let a = chunk(10, "decode header");
        let b = chunk(30, "decode objects");
        let make = |chunk| {
            LexicalAdapter::candidate(
                &project,
                view.clone(),
                1,
                path.clone(),
                ContentHash::of(b"synthetic"),
                Some(Language::new("rust").unwrap()),
                Some(chunk),
                7.0,
                vec!["decode".into(), "header".into(), "objects".into()],
            )
        };
        let first = make(&a);
        let second = make(&b);
        assert_ne!(first.id, second.id);
        assert_eq!(
            first.detail,
            MatchDetail::Lexical {
                terms: vec!["decode".into(), "header".into()]
            }
        );
        assert_eq!(
            second.detail,
            MatchDetail::Lexical {
                terms: vec!["decode".into(), "objects".into()]
            }
        );
        assert_eq!(first.range, Some(a.lines));
        assert_eq!(second.range, Some(b.lines));
    }

    #[test]
    fn only_answered_full_path_queries_skip_semantic_embedding() {
        let exact = SourceStatus::Answered(vec![candidate(
            "src/decoder.rs",
            Some("rust"),
            MatchDetail::Exact {
                term: "src/decoder.rs".into(),
                target: ExactTarget::Path,
            },
        )]);
        let plan = |query| knowell_query::plan(query, &knowell_query::Glossary::default());
        assert!(exact_path_answered(&plan("src/decoder.rs"), &exact));
        assert!(exact_path_answered(&plan("src\\decoder.rs"), &exact));
        assert!(exact_path_answered(&plan("./src/decoder.rs"), &exact));
        for query in [
            "decoder.rs",
            "how does src/decoder.rs decode files?",
            "src/missing.rs",
            "decodeReader",
        ] {
            assert!(!exact_path_answered(&plan(query), &exact), "{query}");
        }
        assert!(!exact_path_answered(
            &plan("src/decoder.rs"),
            &SourceStatus::Answered(Vec::new())
        ));
        assert!(!exact_path_answered(
            &plan("src/decoder.rs"),
            &SourceStatus::NotConsulted
        ));
        let mut truncated = plan("src/decoder.rs");
        truncated.truncated = true;
        assert!(!exact_path_answered(&truncated, &exact));
    }
}
