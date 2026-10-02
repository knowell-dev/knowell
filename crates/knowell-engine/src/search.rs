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
//!        └─ GraphAdapter     (GraphExpander)  stored imports (callers, callees, tests)
//!        ▼
//!   knowell_query::search_with_candidates ──▶ SearchResponse ──pack()──▶ ContextPack
//!                                                         SnippetAdapter (SnippetSource)
//! ```
//!
//! Permission enforcement point 2: every adapter only reads the prepared
//! views, and those exist only for pinned (visible) projects, so graph
//! expansion and context packing can never reach an unauthorised project.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use knowell_auth::UserId;
use knowell_core::{ContentHash, LineRange, Name, RepoPath};
use knowell_embed::Embedder;
use knowell_index::{EmbeddingPlan, Overlay, TierSkip};
use knowell_lexical::LexicalIndex;
use knowell_mcp::ToolError;
use knowell_query::{
    Candidate, Component, Degradation, EdgeKind, EvidenceType as QEvidence, ExactSource,
    ExactTarget, ExpandRequest, GraphExpander, GraphNode, Language, LexicalSource, Location,
    MatchDetail, Neighbor, OverlayPin, PathFilter, PathGlob, PinnedView, PlanOptions,
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
    self, ChunkEntry, Snapshot, SymbolEntry, best_chunk, slice_lines, symbol_entries, terms_of,
    whole_file,
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

/// Path prefixes as include globs (`dir/` covers everything below `dir`;
/// a partial segment matches files and directories starting with it).
fn prefix_globs(prefixes: &[String]) -> Result<Vec<PathGlob>, ToolError> {
    let mut globs = Vec::new();
    for prefix in prefixes {
        let prefix = prefix.trim();
        if prefix.is_empty() {
            continue;
        }
        let patterns: Vec<String> = if prefix.ends_with('/') {
            vec![prefix.to_owned()]
        } else if prefix.contains('/') {
            vec![format!("{prefix}*"), format!("{prefix}*/")]
        } else {
            vec![format!("{prefix}*/")]
        };
        for pattern in patterns {
            globs.push(
                PathGlob::new(pattern)
                    .map_err(|e| ToolError::invalid_input(format!("path_prefixes: {e}")))?,
            );
        }
    }
    Ok(globs)
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

/// Exact lookups over the pinned snapshots (and overlays): symbols, paths
/// and contract keys.
pub(crate) struct ExactAdapter<'a> {
    pub(crate) views: &'a [PreparedView],
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
                            language: None,
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
        Ok(rank_and_limit(
            found,
            request.limit,
            request.per_project_limit,
        ))
    }
}

/// BM25 over the Tantivy index of every pinned generation (and overlays),
/// with file-level hits mapped onto the chunk that shares the most matched
/// terms.
pub(crate) struct LexicalAdapter<'a> {
    pub(crate) views: &'a [PreparedView],
    pub(crate) failures: Mutex<Vec<Degradation>>,
}

impl LexicalAdapter<'_> {
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
        Candidate {
            id: format!("lex:{view}:{path}"),
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
                match index.search(&query, request.limit) {
                    Ok(hits) => {
                        answered = answered.saturating_add(1);
                        for hit in hits {
                            let Ok(path) = RepoPath::new(hit.id.as_str()) else {
                                continue;
                            };
                            let Some(file) = view.snapshot.file(&path) else {
                                continue;
                            };
                            let chunk = view.snapshot.best_chunk(&path, &hit.matched_terms);
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
                                    hit.matched_terms,
                                ),
                            ));
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
                match overlay.overlay.search(&query, request.limit) {
                    Ok(hits) => {
                        for hit in hits {
                            let Ok(path) = RepoPath::new(hit.id.as_str()) else {
                                continue;
                            };
                            let Some(file) = overlay.overlay.file(&path) else {
                                continue;
                            };
                            let chunk = overlay
                                .chunks
                                .get(&path)
                                .and_then(|c| best_chunk(c, &hit.matched_terms));
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
                                    hit.matched_terms,
                                ),
                            ));
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
        Ok(rank_and_limit(
            found,
            request.limit,
            request.per_project_limit,
        ))
    }
}

/// Graph neighbours from the stored edges of the pinned snapshots: symbol
/// `references` / `calls` (the indexer's reference resolution) give
/// callers and callees of a symbol; file imports give file-level callers
/// (importers) and callees (imported files). Callers in test files are
/// reported as tests.
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
        let node_at = |path: &RepoPath| -> Option<GraphNode> {
            Some(GraphNode {
                location: view.base_location(path, None)?,
                symbol: None,
                language: view.language(path),
            })
        };
        // Symbol-level relations (references, calls) of the node's symbol.
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
                for edge in snapshot.uses_of(id) {
                    let Some(node) = symbol_node(&edge.from) else {
                        continue;
                    };
                    let kind = if is_test_path(&node.location.path) {
                        EdgeKind::Test
                    } else {
                        EdgeKind::Caller
                    };
                    if wants(kind) {
                        out.push(Neighbor {
                            node,
                            edge: kind,
                            evidence: map_evidence(edge.evidence),
                            resolution: map_resolution(edge.resolution),
                        });
                    }
                }
            }
            if wants(EdgeKind::Callee) {
                for edge in snapshot.used_by(id) {
                    if let Some(node) = symbol_node(&edge.to) {
                        out.push(Neighbor {
                            node,
                            edge: EdgeKind::Callee,
                            evidence: map_evidence(edge.evidence),
                            resolution: map_resolution(edge.resolution),
                        });
                    }
                }
            }
        }
        if wants(EdgeKind::Callee) {
            for edge in snapshot.imports_from(&location.path) {
                if let snapshot::ImportTarget::File(to) = &edge.to
                    && let Some(node) = node_at(to)
                {
                    out.push(Neighbor {
                        node,
                        edge: EdgeKind::Callee,
                        evidence: map_evidence(edge.evidence),
                        resolution: map_resolution(edge.resolution),
                    });
                }
            }
        }
        if wants(EdgeKind::Caller) || wants(EdgeKind::Test) {
            for edge in snapshot.imports_of(&location.path) {
                let kind = if is_test_path(&edge.from) {
                    EdgeKind::Test
                } else {
                    EdgeKind::Caller
                };
                if !wants(kind) {
                    continue;
                }
                if let Some(node) = node_at(&edge.from) {
                    out.push(Neighbor {
                        node,
                        edge: kind,
                        evidence: map_evidence(edge.evidence),
                        resolution: map_resolution(edge.resolution),
                    });
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
}

impl SnippetAdapter<'_> {
    fn text_of(&self, location: &Location) -> Option<(Arc<str>, Option<&SymbolEntry>, bool)> {
        let (view, is_overlay) = self.prepared.view_of(&location.project, &location.view)?;
        if is_overlay {
            let overlay = view.overlay.as_ref()?;
            let file = overlay.overlay.file(&location.path)?;
            return Some((Arc::clone(&file.text), None, true));
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
            && let Some(found) = symbols.iter().find(|s| s.local == name)
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
        match request.kind {
            SnippetKind::Body => {
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
                    text: slice_lines(&text, range),
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
}

impl Engine {
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
                    inner.indexer.config().parse_limits,
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
    ) -> Result<Prepared, ToolError> {
        let mut views = Vec::new();
        let mut degraded = Vec::new();
        let mut manifest = ViewManifest::new(pinned.workspace.name.clone());
        manifest.not_indexed = pinned.not_indexed.clone();
        let chunk_options = self.inner.indexer.config().chunking;
        for (name, project) in &pinned.projects {
            let snapshot = self.snapshot_of(project).await?;
            let lexical = self.lexical_for(project).await;
            if let Err(reason) = &lexical {
                degraded.push(Degradation::new(Component::Lexical, reason.clone()));
            }
            let base_view = base_view_id(project.view)?;
            let overlay = match &project.overlay {
                Some(o) => {
                    let mut symbols = Vec::new();
                    let mut chunks = BTreeMap::new();
                    for file in o.overlay.files() {
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
            let languages = snapshot
                .languages()
                .keys()
                .filter_map(|l| Language::new(l).ok())
                .collect();
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
        let include = prefix_globs(&filters.path_prefixes)?;
        let scope = QueryScope {
            workspace: pinned.workspace.name.clone(),
            projects: filters.projects.clone(),
            languages: filters.languages.clone(),
            paths: PathFilter {
                include,
                exclude: Vec::new(),
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

    /// The profile whose active vector index covers exactly the pinned
    /// generation of `view`: the planned profile first, then any profile a
    /// registration named before (during a blue/green switch the old
    /// profile keeps serving the generations its vectors cover).
    async fn serving_profile(&self, view: &PreparedView) -> Result<Option<ProfileId>, String> {
        let pin = view.pinned.pin();
        let mut candidates = Vec::new();
        if let EmbeddingPlan::Embed { profile, .. } = &view.pinned.entry.embedding {
            candidates.push(*profile);
        }
        let known: Vec<ProfileId> = self
            .inner
            .profile_providers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .copied()
            .collect();
        for profile in known {
            if !candidates.contains(&profile) {
                candidates.push(profile);
            }
        }
        let mut conn = self
            .inner
            .store
            .acquire()
            .await
            .map_err(|e| format!("store: {e}"))?;
        for profile in candidates {
            let covered = embeddings::active_index_generation(&mut conn, pin.view, profile)
                .await
                .map_err(|e| format!("store: {e}"))?
                .is_some_and(|g| g.view_generation == pin.generation);
            if covered {
                return Ok(Some(profile));
            }
        }
        Ok(None)
    }

    /// Vector candidates: one `nearest` search per embedding profile over
    /// the views whose vectors cover exactly the pinned generation. Profiles
    /// are never mixed: their lists are interleaved by rank. A cloud
    /// provider is never asked on behalf of a local-only project.
    async fn semantic(
        &self,
        plan: &QueryPlan,
        prepared: &Prepared,
        limit: usize,
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
            let provider = self
                .inner
                .profile_providers
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&profile)
                .cloned();
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
            match self.semantic_group(plan, profile_id, &views, limit).await {
                Ok(list) => lists.push(list),
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
    async fn semantic_group(
        &self,
        plan: &QueryPlan,
        profile_id: ProfileId,
        views: &[&PreparedView],
        limit: usize,
    ) -> Result<Vec<Candidate>, String> {
        let provider = self
            .inner
            .profile_providers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&profile_id)
            .cloned()
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
        let vector = embedder
            .embed_query(&plan.semantic_text())
            .await
            .map_err(|e| format!("embedding the query with `{provider}`: {e}"))?;
        let pins: Vec<GenerationPin> = views.iter().map(|v| v.pinned.pin()).collect();
        let k = u32::try_from(limit.clamp(1, 1000)).unwrap_or(100);
        let options = NearestOptions {
            k,
            ef_search: Some(k.max(embeddings::DEFAULT_EF_SEARCH)),
            scope: Some(pins.clone()),
        };
        let mut conn = self
            .inner
            .store
            .acquire()
            .await
            .map_err(|e| format!("store: {e}"))?;
        let neighbors = embeddings::nearest(&mut conn, &profile, vector.as_slice(), &options)
            .await
            .map_err(|e| format!("vector search: {e}"))?;
        let hashes: Vec<ContentHash> = neighbors.iter().map(|n| n.prepared_input_hash).collect();
        // Per-path chunk inputs (the embedding input includes the path, so
        // identical content at two paths has two inputs); generations indexed
        // before per-path inputs existed fall back to content-level rows.
        let mut locations =
            content::locate_chunk_inputs(&mut conn, self.inner.organization, &pins, &hashes)
                .await
                .map_err(|e| format!("store: {e}"))?;
        if locations.is_empty() && !hashes.is_empty() {
            locations =
                content::locate_prepared_inputs(&mut conn, self.inner.organization, &pins, &hashes)
                    .await
                    .map_err(|e| format!("store: {e}"))?;
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
                list.push(Candidate {
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
                    source_rank: u32::try_from(list.len().saturating_add(1)).unwrap_or(u32::MAX),
                    raw_score: 1.0 - neighbor.distance,
                    detail: MatchDetail::Semantic {
                        profile: profile.name.to_string(),
                    },
                });
            }
        }
        Ok(list)
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
        let prepared = self.prepare(pinned, filters).await?;
        let mut config: SearchConfig = self.inner.settings.search.clone();
        config.fusion.result_limit = limit.clamp(1, 1000);
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
        let exact = if !searched || weights.exact <= 0.0 {
            SourceStatus::NotConsulted
        } else {
            let adapter = ExactAdapter {
                views: &prepared.views,
            };
            match adapter.search_exact(&request) {
                Ok(list) => SourceStatus::Answered(list),
                Err(error) => SourceStatus::Failed(error),
            }
        };
        let lexical = if !searched || weights.lexical <= 0.0 {
            SourceStatus::NotConsulted
        } else {
            let adapter = LexicalAdapter {
                views: &prepared.views,
                failures: Mutex::new(Vec::new()),
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
            status
        };
        let semantic = if !searched || weights.semantic <= 0.0 {
            SourceStatus::NotConsulted
        } else {
            let (status, notes) = self
                .semantic(&plan, &prepared, config.fusion.candidate_limit)
                .await;
            extra.extend(notes);
            status
        };
        let lists = SourceLists {
            exact,
            lexical,
            semantic,
        };
        let graph = GraphAdapter {
            prepared: &prepared,
        };
        let mut response = knowell_query::search_with_candidates(
            &plan,
            &prepared.scope,
            lists,
            Some(&graph),
            None,
            &config,
        )
        .map_err(|e| ToolError::invalid_input(e.to_string()))?;
        for degradation in extra {
            if !response.degraded.contains(&degradation) {
                response.degraded.push(degradation);
            }
        }
        Ok(SearchRun { response, prepared })
    }

    /// Texts of every result and expanded item, for snippets and packing.
    pub(crate) async fn texts_for(
        &self,
        run: &SearchRun,
    ) -> Result<BTreeMap<ContentHash, Arc<str>>, ToolError> {
        let mut hashes = BTreeSet::new();
        for result in &run.response.results {
            if let Some((_, false)) = run
                .prepared
                .view_of(&result.location.project, &result.location.view)
            {
                hashes.insert(result.location.content_hash);
            }
        }
        for item in &run.response.expanded {
            if let Some((_, false)) = run
                .prepared
                .view_of(&item.location.project, &item.location.view)
            {
                hashes.insert(item.location.content_hash);
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
    fn prefixes_become_globs() {
        let globs = prefix_globs(&["src/pay".into(), "docs/".into(), "lib".into()]).unwrap();
        let p = |s| RepoPath::new(s).unwrap();
        let admits = |path: &RepoPath| globs.iter().any(|g| g.matches(path));
        assert!(admits(&p("src/payments/a.ts")));
        assert!(admits(&p("src/pay.ts")));
        assert!(admits(&p("docs/x.md")));
        assert!(admits(&p("lib/a.rs")));
        assert!(!admits(&p("src/billing/a.ts")));
        assert!(!admits(&p("other/lib/a.rs")));
        assert!(prefix_globs(&["/abs".into()]).is_err());
    }
}
