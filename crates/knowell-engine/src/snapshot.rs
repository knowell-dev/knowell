//! Per-generation snapshots and the text cache.
//!
//! A [`Snapshot`] is what the engine knows about one view at one pinned
//! generation, loaded once from the store and kept in memory:
//!
//! - the file list (path → content hash, language, size) from `files_at`;
//! - parsed symbols of every file with text (`knowell-parse` over the
//!   store's redacted text — the same parser version the indexer used, so
//!   keys equal the store's `path#qualified.name` symbol keys);
//! - the store's chunks of every file (`chunks_of`) with the normalised
//!   terms of each chunk, to map file-level BM25 hits onto chunks;
//! - every stored edge of the generation (`edges_with_origins` over all
//!   files): imports (resolved to files or unresolved names), symbol
//!   `references` from the indexer's reference resolution, and anything a
//!   relation stage wrote (contract edges, calls);
//! - the store's symbol ids (`definitions_in_paths`) and the contract
//!   participations (`contracts_with_origins`).
//!
//! Building costs two store round trips per file (text, chunks), three
//! batched queries for edges, definitions and contracts, plus parsing; snapshots
//! are cached per `(view, generation)` and dropped when the view activates a
//! newer generation. Generations never change after activation, so a cached
//! snapshot is never stale for its pin.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use knowell_core::{ContentHash, LineRange, Name, RepoPath};
use knowell_index::{parser_version_tag, symbol_key};
use knowell_parse::{ParseLimits, SymbolKind, parse_with};
use knowell_store::content;
use knowell_store::graph::{self, ContractParty, NodeRef};
use knowell_store::symbols;
use knowell_store::views::GenerationPin;
use knowell_store::{EvidenceType, OrganizationId, ProjectId, Resolution, Store, SymbolId};
use tokio::sync::OnceCell;

use crate::error::EngineError;

/// Metadata of one file of a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileInfo {
    pub(crate) content_hash: ContentHash,
    pub(crate) language: Option<String>,
    pub(crate) size_bytes: u64,
    pub(crate) line_count: u32,
    pub(crate) has_text: bool,
}

/// One parsed symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SymbolEntry {
    /// Store key: `path#qualified.name`.
    pub(crate) key: String,
    /// Qualified name inside the file (`SubscriptionService.cancel`).
    pub(crate) local: String,
    /// Short name (`cancel`).
    pub(crate) name: String,
    pub(crate) kind: SymbolKind,
    pub(crate) path: RepoPath,
    pub(crate) lines: LineRange,
    /// Line of the symbol's name (start of its signature).
    pub(crate) name_line: u32,
    pub(crate) signature: String,
    pub(crate) doc: Option<String>,
    /// Index of the enclosing symbol in [`Snapshot::symbols`].
    pub(crate) parent: Option<usize>,
    /// The store's id of the symbol (its definition in this generation).
    pub(crate) store_id: Option<SymbolId>,
}

/// One stored chunk with its normalised terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChunkEntry {
    pub(crate) lines: LineRange,
    /// Bytes of the chunk's source text.
    pub(crate) bytes: u64,
    pub(crate) kind: String,
    pub(crate) symbol_path: Option<String>,
    pub(crate) terms: BTreeSet<String>,
}

/// Where an import points.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ImportTarget {
    /// A file of the same project.
    File(RepoPath),
    /// A name that did not resolve to a file (package, alias, external).
    Name(String),
}

/// One stored `imports` edge of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportEdge {
    pub(crate) from: RepoPath,
    pub(crate) to: ImportTarget,
    pub(crate) evidence: EvidenceType,
    pub(crate) resolution: Resolution,
    pub(crate) lines: Option<LineRange>,
}

/// Any other stored relation (symbol references, calls, contract edges).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OtherEdge {
    /// The file whose analysis wrote the edge (where its evidence is).
    pub(crate) origin: RepoPath,
    pub(crate) from: NodeRef,
    pub(crate) to: NodeRef,
    pub(crate) kind: String,
    pub(crate) evidence: EvidenceType,
    pub(crate) resolution: Resolution,
    pub(crate) lines: Option<LineRange>,
}

/// Everything known about one view at one generation. See the module docs.
#[derive(Debug)]
pub(crate) struct Snapshot {
    pub(crate) project: Name,
    pub(crate) project_id: ProjectId,
    pub(crate) pin: GenerationPin,
    pub(crate) files: BTreeMap<RepoPath, FileInfo>,
    pub(crate) symbols: Vec<SymbolEntry>,
    pub(crate) symbols_by_file: BTreeMap<RepoPath, Vec<usize>>,
    /// Lowercased short name → symbol indices.
    pub(crate) by_name: BTreeMap<String, Vec<usize>>,
    pub(crate) chunks: BTreeMap<RepoPath, Vec<ChunkEntry>>,
    pub(crate) imports: Vec<ImportEdge>,
    /// Imported file → indices into `imports` of the edges pointing at it.
    pub(crate) importers: BTreeMap<RepoPath, Vec<usize>>,
    pub(crate) other_edges: Vec<OtherEdge>,
    /// Store symbol id -> index into `symbols`.
    pub(crate) by_id: BTreeMap<SymbolId, usize>,
    /// Indices into `other_edges` by target symbol and by source symbol.
    pub(crate) edges_into: BTreeMap<SymbolId, Vec<usize>>,
    pub(crate) edges_from: BTreeMap<SymbolId, Vec<usize>>,
    /// Contract participations of the generation.
    pub(crate) contracts: Vec<ContractParty>,
}

impl Snapshot {
    /// The file, if the generation has it.
    pub(crate) fn file(&self, path: &RepoPath) -> Option<&FileInfo> {
        self.files.get(path)
    }

    /// Symbols defined in `path`, in parse order.
    pub(crate) fn symbols_in(&self, path: &RepoPath) -> impl Iterator<Item = &SymbolEntry> {
        self.symbols_by_file
            .get(path)
            .into_iter()
            .flatten()
            .filter_map(|i| self.symbols.get(*i))
    }

    /// The innermost symbol of `path` whose range contains `lines`.
    pub(crate) fn enclosing_symbol(
        &self,
        path: &RepoPath,
        lines: LineRange,
    ) -> Option<&SymbolEntry> {
        self.symbols_in(path)
            .filter(|s| s.lines.start() <= lines.start() && s.lines.end() >= lines.end())
            .min_by_key(|s| (s.lines.line_count(), s.lines.start()))
    }

    /// The symbol of `path` with exactly this in-file qualified name.
    pub(crate) fn symbol_by_local(&self, path: &RepoPath, local: &str) -> Option<&SymbolEntry> {
        self.symbols_in(path).find(|s| s.local == local)
    }

    /// Languages with file counts.
    pub(crate) fn languages(&self) -> BTreeMap<String, u64> {
        let mut out: BTreeMap<String, u64> = BTreeMap::new();
        for file in self.files.values() {
            if let Some(language) = &file.language {
                let count = out.entry(language.clone()).or_default();
                *count = count.saturating_add(1);
            }
        }
        out
    }

    /// The import edges whose target is `path`.
    pub(crate) fn imports_of(&self, path: &RepoPath) -> impl Iterator<Item = &ImportEdge> {
        self.importers
            .get(path)
            .into_iter()
            .flatten()
            .filter_map(|i| self.imports.get(*i))
    }

    /// The import edges leaving `path`.
    pub(crate) fn imports_from<'a>(
        &'a self,
        path: &'a RepoPath,
    ) -> impl Iterator<Item = &'a ImportEdge> + 'a {
        self.imports.iter().filter(move |e| &e.from == path)
    }

    /// The symbol with store id `id`.
    pub(crate) fn symbol_by_id(&self, id: SymbolId) -> Option<&SymbolEntry> {
        self.by_id.get(&id).and_then(|i| self.symbols.get(*i))
    }

    /// Relations (`references`, `calls`) that end at `id`.
    pub(crate) fn uses_of(&self, id: SymbolId) -> impl Iterator<Item = &OtherEdge> {
        self.edges_into
            .get(&id)
            .into_iter()
            .flatten()
            .filter_map(|i| self.other_edges.get(*i))
            .filter(|e| matches!(e.kind.as_str(), "references" | "calls"))
    }

    /// Relations (`references`, `calls`) that start at `id`.
    pub(crate) fn used_by(&self, id: SymbolId) -> impl Iterator<Item = &OtherEdge> {
        self.edges_from
            .get(&id)
            .into_iter()
            .flatten()
            .filter_map(|i| self.other_edges.get(*i))
            .filter(|e| matches!(e.kind.as_str(), "references" | "calls"))
    }

    /// Where an edge end is: its file, lines and symbol (if a symbol).
    pub(crate) fn place_of(
        &self,
        node: &NodeRef,
    ) -> Option<(RepoPath, Option<LineRange>, Option<&SymbolEntry>)> {
        match node {
            NodeRef::Symbol(id) => {
                let symbol = self.symbol_by_id(*id)?;
                Some((symbol.path.clone(), Some(symbol.lines), Some(symbol)))
            }
            NodeRef::File { project, path } if *project == self.project_id => self
                .files
                .contains_key(path)
                .then(|| (path.clone(), None, None)),
            _ => None,
        }
    }

    /// The best chunk of `path` for a lexical hit with `terms`: the one
    /// sharing the most terms; ties prefer a chunk with a symbol, then the
    /// shorter, then the earlier one. `None` when no chunk shares a term.
    pub(crate) fn best_chunk(&self, path: &RepoPath, terms: &[String]) -> Option<&ChunkEntry> {
        best_chunk(self.chunks.get(path)?, terms)
    }
}

/// See [`Snapshot::best_chunk`]; shared with overlay files.
pub(crate) fn best_chunk<'a>(chunks: &'a [ChunkEntry], terms: &[String]) -> Option<&'a ChunkEntry> {
    chunks
        .iter()
        .map(|c| {
            let shared = terms.iter().filter(|t| c.terms.contains(*t)).count();
            (c, shared)
        })
        .filter(|(_, shared)| *shared > 0)
        .min_by(|(a, sa), (b, sb)| {
            sb.cmp(sa)
                .then_with(|| b.symbol_path.is_some().cmp(&a.symbol_path.is_some()))
                .then_with(|| a.lines.line_count().cmp(&b.lines.line_count()))
                .then_with(|| a.lines.start().cmp(&b.lines.start()))
        })
        .map(|(c, _)| c)
}

/// Normalised terms of `text` (the lexical index's own tokenizer).
pub(crate) fn terms_of(text: &str) -> BTreeSet<String> {
    knowell_lexical::tokenize(text)
        .into_iter()
        .map(|t| t.text)
        .collect()
}

/// Lines `range` of `text` (1-based, inclusive), clamped to the text.
pub(crate) fn slice_lines(text: &str, range: LineRange) -> String {
    let start = usize::try_from(range.start().saturating_sub(1)).unwrap_or(usize::MAX);
    let count = usize::try_from(range.line_count()).unwrap_or(usize::MAX);
    let mut out = String::new();
    for (i, line) in text.split('\n').skip(start).take(count).enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }
    out
}

/// Number of lines of `text` (a trailing newline does not start a line).
pub(crate) fn line_count(text: &str) -> u32 {
    let lines = text.split('\n').count();
    let lines = if text.ends_with('\n') {
        lines.saturating_sub(1)
    } else {
        lines
    };
    u32::try_from(lines.max(1)).unwrap_or(u32::MAX)
}

/// The whole file as a line range (`1..=max(1, line_count)`; always
/// `Some`, typed as an option because `LineRange` is fallible).
pub(crate) fn whole_file(line_count: u32) -> Option<LineRange> {
    LineRange::new(1, line_count.max(1)).ok()
}

/// Lines `[a, b]` of an edge's evidence JSON.
fn evidence_lines(evidence: &serde_json::Value) -> Option<LineRange> {
    let lines = evidence.get("lines")?.as_array()?;
    let start = u32::try_from(lines.first()?.as_u64()?).ok()?;
    let end = u32::try_from(lines.get(1)?.as_u64()?).ok()?;
    LineRange::new(start, end).ok()
}

/// Reads and parses one generation. See the module docs for the cost.
pub(crate) async fn build(
    store: &Store,
    texts: &TextCache,
    organization: OrganizationId,
    project: Name,
    project_id: ProjectId,
    pin: GenerationPin,
    limits: ParseLimits,
) -> Result<Snapshot, EngineError> {
    let parser_version = parser_version_tag();
    let mut conn = store.acquire().await?;
    let versions = content::files_at(&mut conn, pin).await?;
    let mut files = BTreeMap::new();
    let mut texts_by_path: Vec<(RepoPath, Arc<str>)> = Vec::new();
    let mut chunk_rows: BTreeMap<RepoPath, Vec<content::Chunk>> = BTreeMap::new();
    for version in versions {
        let path = version.path.clone();
        let hash = version.content_hash;
        let stored = content::get_content(&mut conn, organization, &hash).await?;
        let (language, size_bytes, text) = match stored {
            Some(c) => {
                let text: Option<Arc<str>> = c.redacted_text.map(Arc::from);
                (c.language, c.size_bytes, text)
            }
            None => (None, 0, None),
        };
        let line_count = text.as_deref().map_or(0, line_count);
        files.insert(
            path.clone(),
            FileInfo {
                content_hash: hash,
                language,
                size_bytes,
                line_count,
                has_text: text.is_some(),
            },
        );
        if let Some(text) = text {
            texts.insert(hash, Arc::clone(&text));
            texts_by_path.push((path.clone(), text));
        }
        let chunks = content::chunks_of(&mut conn, organization, &hash, &parser_version).await?;
        chunk_rows.insert(path.clone(), chunks);
    }
    let paths: Vec<RepoPath> = files.keys().cloned().collect();
    // Rows are grouped by origin: T1 writes the bare path, relation stages
    // prefix their name (`link:<path>`).
    let origins: Vec<String> = paths
        .iter()
        .flat_map(|p| {
            [
                p.to_string(),
                format!("{}:{p}", knowell_index::LINK_STAGE_NAME),
            ]
        })
        .collect();
    let mut imports = Vec::new();
    let mut other_edges = Vec::new();
    for edge in graph::edges_with_origins(&mut conn, pin, &origins).await? {
        let data = edge.edge;
        let Some(origin) = origin_path(&data.origin) else {
            continue;
        };
        let lines = evidence_lines(&data.evidence);
        match (data.kind.as_str(), &data.from, &data.to) {
            ("defines" | "contains", _, _) => {}
            (
                "imports",
                NodeRef::File {
                    project: pf,
                    path: from,
                },
                NodeRef::File {
                    project: pt,
                    path: to,
                },
            ) if *pf == project_id && *pt == project_id => imports.push(ImportEdge {
                from: from.clone(),
                to: ImportTarget::File(to.clone()),
                evidence: data.evidence_type,
                resolution: data.resolution,
                lines,
            }),
            (
                "imports",
                NodeRef::File {
                    project: pf,
                    path: from,
                },
                NodeRef::Name { name, .. },
            ) if *pf == project_id => imports.push(ImportEdge {
                from: from.clone(),
                to: ImportTarget::Name(name.clone()),
                evidence: data.evidence_type,
                resolution: data.resolution,
                lines,
            }),
            _ => other_edges.push(OtherEdge {
                origin,
                from: data.from.clone(),
                to: data.to.clone(),
                kind: data.kind.clone(),
                evidence: data.evidence_type,
                resolution: data.resolution,
                lines,
            }),
        }
    }
    let definitions = symbols::definitions_in_paths(&mut conn, pin, &paths).await?;
    let contracts = graph::contracts_with_origins(&mut conn, pin, &origins).await?;
    drop(conn);
    let parsed = tokio::task::spawn_blocking(move || analyse(texts_by_path, chunk_rows, limits))
        .await
        .map_err(|e| EngineError::internal(format!("parsing a snapshot: {e}")))?;
    let (mut symbols, chunks) = parsed;
    let ids: BTreeMap<&str, SymbolId> = definitions
        .iter()
        .map(|d| (d.symbol.qualified_name.as_str(), d.symbol.id))
        .collect();
    for symbol in &mut symbols {
        symbol.store_id = ids.get(symbol.key.as_str()).copied();
    }
    Ok(Snapshot::assemble(SnapshotParts {
        project,
        project_id,
        pin,
        files,
        symbols,
        chunks,
        imports,
        other_edges,
        contracts,
    }))
}

/// The loaded parts of a snapshot, before indexing.
pub(crate) struct SnapshotParts {
    pub(crate) project: Name,
    pub(crate) project_id: ProjectId,
    pub(crate) pin: GenerationPin,
    pub(crate) files: BTreeMap<RepoPath, FileInfo>,
    pub(crate) symbols: Vec<SymbolEntry>,
    pub(crate) chunks: BTreeMap<RepoPath, Vec<ChunkEntry>>,
    pub(crate) imports: Vec<ImportEdge>,
    pub(crate) other_edges: Vec<OtherEdge>,
    pub(crate) contracts: Vec<ContractParty>,
}

impl Snapshot {
    /// Builds the lookup indexes over loaded parts.
    pub(crate) fn assemble(parts: SnapshotParts) -> Snapshot {
        let SnapshotParts {
            project,
            project_id,
            pin,
            files,
            symbols,
            chunks,
            imports,
            other_edges,
            contracts,
        } = parts;
        let mut by_id = BTreeMap::new();
        let mut symbols_by_file: BTreeMap<RepoPath, Vec<usize>> = BTreeMap::new();
        let mut by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, symbol) in symbols.iter().enumerate() {
            if let Some(id) = symbol.store_id {
                by_id.entry(id).or_insert(i);
            }
            symbols_by_file
                .entry(symbol.path.clone())
                .or_default()
                .push(i);
            by_name
                .entry(symbol.name.to_lowercase())
                .or_default()
                .push(i);
        }
        let mut edges_into: BTreeMap<SymbolId, Vec<usize>> = BTreeMap::new();
        let mut edges_from: BTreeMap<SymbolId, Vec<usize>> = BTreeMap::new();
        for (i, edge) in other_edges.iter().enumerate() {
            if let NodeRef::Symbol(id) = &edge.to {
                edges_into.entry(*id).or_default().push(i);
            }
            if let NodeRef::Symbol(id) = &edge.from {
                edges_from.entry(*id).or_default().push(i);
            }
        }
        let mut importers: BTreeMap<RepoPath, Vec<usize>> = BTreeMap::new();
        for (i, edge) in imports.iter().enumerate() {
            if let ImportTarget::File(to) = &edge.to {
                importers.entry(to.clone()).or_default().push(i);
            }
        }
        Snapshot {
            project,
            project_id,
            pin,
            files,
            symbols,
            symbols_by_file,
            by_name,
            chunks,
            imports,
            importers,
            other_edges,
            by_id,
            edges_into,
            edges_from,
            contracts,
        }
    }
}

type Analysed = (Vec<SymbolEntry>, BTreeMap<RepoPath, Vec<ChunkEntry>>);

/// Named symbols of one parsed file as snapshot entries; `base` is the
/// index the first one will have (parents are remapped accordingly).
pub(crate) fn symbol_entries(
    path: &RepoPath,
    parsed: &knowell_parse::ParsedFile,
    base: usize,
) -> Vec<SymbolEntry> {
    let mut index_map = BTreeMap::new();
    let mut next = base;
    for (i, symbol) in parsed.symbols.iter().enumerate() {
        if !symbol.qualified_name.is_empty() {
            index_map.insert(i, next);
            next = next.saturating_add(1);
        }
    }
    parsed
        .symbols
        .iter()
        .filter(|s| !s.qualified_name.is_empty())
        .map(|symbol| SymbolEntry {
            key: symbol_key(path, &symbol.qualified_name),
            local: symbol.qualified_name.clone(),
            name: symbol.name.clone(),
            kind: symbol.kind,
            path: path.clone(),
            lines: symbol.range,
            name_line: symbol.name_line,
            signature: symbol.signature.clone(),
            doc: symbol.doc.clone(),
            parent: symbol.parent.and_then(|p| index_map.get(&p).copied()),
            store_id: None,
        })
        .collect()
}

/// Parses every file and computes chunk terms (blocking).
fn analyse(
    texts: Vec<(RepoPath, Arc<str>)>,
    mut chunk_rows: BTreeMap<RepoPath, Vec<content::Chunk>>,
    limits: ParseLimits,
) -> Analysed {
    let mut symbols = Vec::new();
    let mut chunks: BTreeMap<RepoPath, Vec<ChunkEntry>> = BTreeMap::new();
    for (path, text) in texts {
        let parsed = parse_with(&path, &text, &limits, None);
        let base = symbols.len();
        symbols.extend(symbol_entries(&path, &parsed, base));
        let rows = chunk_rows.remove(&path).unwrap_or_default();
        let entries = rows
            .into_iter()
            .map(|row| {
                let chunk = row.chunk;
                ChunkEntry {
                    terms: terms_of(&slice_lines(&text, chunk.lines)),
                    bytes: chunk.end_byte.saturating_sub(chunk.start_byte),
                    lines: chunk.lines,
                    kind: chunk.kind,
                    symbol_path: chunk.symbol_path,
                }
            })
            .collect();
        chunks.insert(path, entries);
    }
    // Files without text keep their (empty) chunk lists.
    for (path, rows) in chunk_rows {
        chunks.entry(path).or_insert_with(|| {
            rows.into_iter()
                .map(|row| ChunkEntry {
                    bytes: row.chunk.end_byte.saturating_sub(row.chunk.start_byte),
                    lines: row.chunk.lines,
                    kind: row.chunk.kind,
                    symbol_path: row.chunk.symbol_path,
                    terms: BTreeSet::new(),
                })
                .collect()
        });
    }
    (symbols, chunks)
}

/// A byte-bounded cache of redacted file text, keyed by content hash.
/// Least recently used entries are evicted beyond the limit.
#[derive(Debug)]
pub(crate) struct TextCache {
    limit: usize,
    state: Mutex<TextState>,
}

#[derive(Debug, Default)]
struct TextState {
    entries: BTreeMap<ContentHash, (Arc<str>, u64)>,
    order: BTreeMap<u64, ContentHash>,
    bytes: usize,
    tick: u64,
}

impl TextCache {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            state: Mutex::new(TextState::default()),
        }
    }

    /// The cached text of `hash`.
    pub(crate) fn get(&self, hash: &ContentHash) -> Option<Arc<str>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.tick = state.tick.wrapping_add(1);
        let tick = state.tick;
        let (text, old) = {
            let entry = state.entries.get_mut(hash)?;
            let old = entry.1;
            entry.1 = tick;
            (Arc::clone(&entry.0), old)
        };
        state.order.remove(&old);
        state.order.insert(tick, *hash);
        Some(text)
    }

    /// Caches `text` for `hash`, evicting old entries beyond the limit.
    pub(crate) fn insert(&self, hash: ContentHash, text: Arc<str>) {
        if text.len() > self.limit {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.tick = state.tick.wrapping_add(1);
        let tick = state.tick;
        if let Some((old_text, old_tick)) = state.entries.insert(hash, (Arc::clone(&text), tick)) {
            state.order.remove(&old_tick);
            state.bytes = state.bytes.saturating_sub(old_text.len());
        }
        state.order.insert(tick, hash);
        state.bytes = state.bytes.saturating_add(text.len());
        while state.bytes > self.limit {
            let Some((&oldest, _)) = state.order.iter().next() else {
                break;
            };
            let Some(evicted) = state.order.remove(&oldest) else {
                break;
            };
            if let Some((evicted_text, _)) = state.entries.remove(&evicted) {
                state.bytes = state.bytes.saturating_sub(evicted_text.len());
            }
        }
    }

    /// The text of `hash`, from the cache or the store.
    pub(crate) async fn load(
        &self,
        store: &Store,
        organization: OrganizationId,
        hash: &ContentHash,
    ) -> Result<Option<Arc<str>>, EngineError> {
        if let Some(text) = self.get(hash) {
            return Ok(Some(text));
        }
        let mut conn = store.acquire().await?;
        let stored = content::get_content(&mut conn, organization, hash).await?;
        let Some(text) = stored.and_then(|c| c.redacted_text) else {
            return Ok(None);
        };
        let text: Arc<str> = Arc::from(text);
        self.insert(*hash, Arc::clone(&text));
        Ok(Some(text))
    }
}

/// Snapshots by `(view, generation)`, built at most once each, least
/// recently used evicted beyond the capacity.
#[derive(Debug)]
pub(crate) struct SnapshotCache {
    capacity: usize,
    state: Mutex<SnapshotState>,
}

type SnapshotCell = Arc<OnceCell<Arc<Snapshot>>>;

#[derive(Debug, Default)]
struct SnapshotState {
    cells: BTreeMap<GenerationPin, (SnapshotCell, u64)>,
    tick: u64,
}

impl SnapshotCache {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            state: Mutex::new(SnapshotState::default()),
        }
    }

    /// The cell of `pin`, created when missing.
    pub(crate) fn cell(&self, pin: GenerationPin) -> SnapshotCell {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.tick = state.tick.wrapping_add(1);
        let tick = state.tick;
        let cell = match state.cells.get_mut(&pin) {
            Some(entry) => {
                entry.1 = tick;
                Arc::clone(&entry.0)
            }
            None => {
                let cell: SnapshotCell = Arc::new(OnceCell::new());
                state.cells.insert(pin, (Arc::clone(&cell), tick));
                cell
            }
        };
        while state.cells.len() > self.capacity {
            let oldest = state
                .cells
                .iter()
                .filter(|(p, _)| **p != pin)
                .min_by_key(|(_, (_, t))| *t)
                .map(|(p, _)| *p);
            match oldest {
                Some(p) => {
                    state.cells.remove(&p);
                }
                None => break,
            }
        }
        cell
    }

    /// Drops every snapshot of `view` older than `generation`.
    pub(crate) fn retire_older(&self, view: knowell_store::ViewId, generation: i64) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .cells
            .retain(|pin, _| pin.view != view || pin.generation >= generation);
    }

    /// Drops everything.
    pub(crate) fn clear(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.cells.clear();
    }
}

/// The file a stored edge or contract row belongs to. Rows written by T1 use
/// the bare path as origin; relation stages prefix their stage name
/// (`link:src/api.ts`), which is not part of the path.
pub(crate) fn origin_path(origin: &str) -> Option<RepoPath> {
    let path = match origin.split_once(':') {
        Some((stage, rest))
            if !stage.is_empty()
                && stage.chars().all(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'
                }) =>
        {
            rest
        }
        _ => origin,
    };
    RepoPath::new(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(a: u32, b: u32) -> LineRange {
        LineRange::new(a, b).unwrap()
    }

    #[test]
    fn slices_and_counts_lines() {
        let text = "a\nb\nc\n";
        assert_eq!(line_count(text), 3);
        assert_eq!(line_count("x"), 1);
        assert_eq!(line_count(""), 1);
        assert_eq!(slice_lines(text, lines(2, 3)), "b\nc");
        assert_eq!(slice_lines(text, lines(3, 9)), "c\n");
        assert_eq!(slice_lines(text, lines(9, 9)), "");
        assert_eq!(whole_file(0), Some(lines(1, 1)));
    }

    #[test]
    fn best_chunk_prefers_more_terms_then_symbols() {
        let chunk = |a, b, symbol: Option<&str>, terms: &[&str]| ChunkEntry {
            lines: lines(a, b),
            bytes: 0,
            kind: "function".into(),
            symbol_path: symbol.map(str::to_owned),
            terms: terms.iter().map(|t| (*t).to_owned()).collect(),
        };
        let chunks = vec![
            chunk(1, 50, None, &["cancel", "subscription"]),
            chunk(10, 20, Some("Svc.cancel"), &["cancel", "subscription"]),
            chunk(30, 40, Some("Svc.other"), &["cancel"]),
        ];
        let terms = vec!["cancel".to_owned(), "subscription".to_owned()];
        assert_eq!(
            best_chunk(&chunks, &terms).unwrap().symbol_path.as_deref(),
            Some("Svc.cancel")
        );
        assert!(best_chunk(&chunks, &["refund".to_owned()]).is_none());
    }

    #[test]
    fn text_cache_evicts_least_recently_used() {
        let cache = TextCache::new(10);
        let a = ContentHash::of(b"a");
        let b = ContentHash::of(b"b");
        let c = ContentHash::of(b"c");
        cache.insert(a, Arc::from("aaaa"));
        cache.insert(b, Arc::from("bbbb"));
        assert!(cache.get(&a).is_some());
        cache.insert(c, Arc::from("cccc"));
        assert!(cache.get(&b).is_none(), "b was least recently used");
        assert!(cache.get(&a).is_some());
        assert!(cache.get(&c).is_some());
        cache.insert(ContentHash::of(b"big"), Arc::from("x".repeat(11)));
        assert!(cache.get(&ContentHash::of(b"big")).is_none());
    }
}
