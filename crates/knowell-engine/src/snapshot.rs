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
//! Search preparation reads paged file metadata and stored chunk/relationship
//! coordinates without reading source bodies. Selected paths are hydrated only
//! after retrieval. Tools that need a complete graph keep the full build path.
//! Full building loads text and chunks in batches of unique content hashes, plus
//! batched edges, definitions and contracts and generation-wide parsing; snapshots
//! are cached per `(view, generation)` and dropped when the view activates a
//! newer generation. Generations never change after activation, so a cached
//! snapshot is never stale for its pin.
//! An optional persisted local parse-product cache skips repeated parsing only
//! when the redacted body, full path, parser version and limits all match.

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
use crate::parse_product::ParseProductCache;

/// Metadata of one file of a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileInfo {
    pub(crate) content_hash: ContentHash,
    /// Detected at this file occurrence, never inherited from a shared blob.
    pub(crate) language: Option<String>,
    pub(crate) size_bytes: u64,
    /// Zero when legacy metadata has no count; resolved from selected source text.
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
    /// Exact parser source boundaries when indexed by the current hierarchy stage.
    /// Older indexes have none and must retain conservative source slicing.
    pub(crate) structure: Option<content::ChunkStructure>,
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
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    pub(crate) project: Name,
    pub(crate) project_id: ProjectId,
    pub(crate) pin: GenerationPin,
    pub(crate) files: BTreeMap<RepoPath, FileInfo>,
    /// Immutable occurrence counts prepared once, avoiding a catalog walk on
    /// every warm query merely to report language capabilities.
    language_counts: BTreeMap<String, u64>,
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
    /// Paths whose body has been parsed for this snapshot instance.
    /// Metadata symbols never masquerade as prepared signatures or documentation.
    pub(crate) source_ready: BTreeSet<RepoPath>,
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
        self.language_counts.clone()
    }

    /// The import edges whose target is `path`.
    pub(crate) fn imports_of(&self, path: &RepoPath) -> impl Iterator<Item = &ImportEdge> {
        self.importers
            .get(path)
            .into_iter()
            .flatten()
            .filter_map(|i| self.imports.get(*i))
    }

    /// The symbol with store id `id`.
    pub(crate) fn symbol_by_id(&self, id: SymbolId) -> Option<&SymbolEntry> {
        self.by_id.get(&id).and_then(|i| self.symbols.get(*i))
    }

    /// Relations (`references`, `calls`) that end at `id`.
    #[cfg(test)]
    pub(crate) fn uses_of(&self, id: SymbolId) -> impl Iterator<Item = &OtherEdge> {
        self.edges_into
            .get(&id)
            .into_iter()
            .flatten()
            .filter_map(|i| self.other_edges.get(*i))
            .filter(|e| matches!(e.kind.as_str(), "references" | "calls"))
    }

    /// Relations (`references`, `calls`) that start at `id`.
    #[cfg(test)]
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

    /// At most `limit` non-overlapping chunks of `path`, ordered by shared
    /// query terms, symbol presence, length and source position.
    pub(crate) fn best_chunks(
        &self,
        path: &RepoPath,
        terms: &[String],
        limit: usize,
    ) -> Vec<&ChunkEntry> {
        self.chunks
            .get(path)
            .map(|chunks| best_chunks(chunks, terms, limit))
            .unwrap_or_default()
    }
}

/// See [`Snapshot::best_chunks`]; shared with overlay files. A parent and
/// its nested chunk must not consume two candidate slots for the same text.
pub(crate) fn best_chunks<'a>(
    chunks: &'a [ChunkEntry],
    terms: &[String],
    limit: usize,
) -> Vec<&'a ChunkEntry> {
    if limit == 0 {
        return Vec::new();
    }
    let terms: BTreeSet<&str> = terms.iter().map(String::as_str).collect();
    let mut matched: Vec<_> = chunks
        .iter()
        .map(|c| {
            let shared = terms.iter().filter(|t| c.terms.contains(**t)).count();
            (c, shared)
        })
        .filter(|(_, shared)| *shared > 0)
        .collect();
    matched.sort_by(|(a, sa), (b, sb)| {
        sb.cmp(sa)
            .then_with(|| b.symbol_path.is_some().cmp(&a.symbol_path.is_some()))
            .then_with(|| a.lines.line_count().cmp(&b.lines.line_count()))
            .then_with(|| a.lines.start().cmp(&b.lines.start()))
            .then_with(|| a.lines.end().cmp(&b.lines.end()))
            .then_with(|| a.symbol_path.cmp(&b.symbol_path))
            .then_with(|| a.kind.cmp(&b.kind))
    });
    let mut selected: Vec<&ChunkEntry> = Vec::new();
    for (chunk, _) in matched {
        if selected.len() >= limit {
            break;
        }
        let overlaps = selected.iter().any(|other| {
            other.lines.start() <= chunk.lines.end() && chunk.lines.start() <= other.lines.end()
        });
        if !overlaps {
            selected.push(chunk);
        }
    }
    selected
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

/// Local parsing policy and optional persisted products for a generation.
pub(crate) struct ParseOptions {
    pub(crate) limits: ParseLimits,
    pub(crate) products: Option<Arc<ParseProductCache>>,
}

const METADATA_PAGE_FILES: usize = 1_000;

/// A partial catalog's hard scope is part of its cache identity. An empty
/// language set deliberately differs from no language restriction.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct MetadataScope {
    pub(crate) path_prefixes: Vec<String>,
    pub(crate) languages: Option<Vec<String>>,
}

impl MetadataScope {
    pub(crate) fn new(path_prefixes: &[String], languages: Option<Vec<String>>) -> Self {
        Self {
            path_prefixes: path_prefixes
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            languages: languages.map(|values| {
                values
                    .into_iter()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            }),
        }
    }

    fn store_scope(&self) -> content::FileMetadataScope<'_> {
        content::FileMetadataScope {
            path_prefixes: &self.path_prefixes,
            languages: self.languages.as_deref(),
        }
    }
}
/// Selected hydration is bounded independently of a generation's file count.
pub(crate) const MAX_HYDRATED_PATHS: usize = 1_000;
/// Hard bound on source body bytes retained by one selected hydration request.
pub(crate) const MAX_HYDRATED_SOURCE_BYTES: usize = 64 * 1024 * 1024;

fn structure_map(
    structures: Vec<content::ChunkStructure>,
) -> BTreeMap<(ContentHash, u32), content::ChunkStructure> {
    structures
        .into_iter()
        .map(|structure| {
            (
                (structure.key.content_hash, structure.key.ordinal),
                structure,
            )
        })
        .collect()
}

/// Reads paged, tenant-scoped generation metadata without any source bodies or
/// local parsing. Relationships keep their stored evidence and resolution.
/// Names are taken from pinned definition evidence or legacy chunk metadata,
/// never from the mutable logical symbol labels.
pub(crate) async fn build_metadata(
    store: &Store,
    organization: OrganizationId,
    project: Name,
    project_id: ProjectId,
    pin: GenerationPin,
) -> Result<Snapshot, EngineError> {
    build_metadata_scoped(
        store,
        organization,
        project,
        project_id,
        pin,
        &MetadataScope::default(),
    )
    .await
}

/// Loads only file occurrences admitted by the hard query scope. Origins and
/// source coordinates stay pinned; paths outside the scope are never added by
/// graph metadata. Unrestricted calls preserve the complete catalog behavior.
pub(crate) async fn build_metadata_scoped(
    store: &Store,
    organization: OrganizationId,
    project: Name,
    project_id: ProjectId,
    pin: GenerationPin,
    scope: &MetadataScope,
) -> Result<Snapshot, EngineError> {
    let parser_version = parser_version_tag();
    let mut conn = store.acquire().await?;
    let mut after = None;
    let mut files = BTreeMap::new();
    let mut chunks = BTreeMap::new();
    let mut symbol_rows = Vec::new();
    let mut imports = Vec::new();
    let mut other_edges = Vec::new();
    let mut contracts = Vec::new();
    loop {
        let page = content::files_metadata_at_scope_page(
            &mut conn,
            organization,
            pin,
            after.as_ref(),
            METADATA_PAGE_FILES,
            scope.store_scope(),
        )
        .await?;
        if page.is_empty() {
            break;
        }
        after = page.last().map(|row| row.version.path.clone());
        let paths: Vec<_> = page.iter().map(|row| row.version.path.clone()).collect();
        let hashes: Vec<_> = page.iter().map(|row| row.version.content_hash).collect();
        let rows =
            content::chunks_of_many(&mut conn, organization, &hashes, &parser_version).await?;
        let structures = structure_map(
            content::chunk_structures_of(&mut conn, organization, &hashes, &parser_version).await?,
        );
        let definitions = symbols::definitions_in_paths(&mut conn, pin, &paths).await?;
        let origins = origins_of(&paths);
        let edges = graph::edges_with_origins(&mut conn, pin, &origins).await?;
        let labels = metadata_definition_labels(&edges, pin, project_id);
        let mut definitions_by_path: BTreeMap<RepoPath, Vec<symbols::Definition>> = BTreeMap::new();
        for definition in definitions {
            definitions_by_path
                .entry(definition.path.clone())
                .or_default()
                .push(definition);
        }
        let mut by_hash: BTreeMap<ContentHash, Vec<content::Chunk>> = BTreeMap::new();
        for row in rows {
            by_hash.entry(row.chunk.content_hash).or_default().push(row);
        }
        for row in page {
            let path = row.version.path;
            let hash = row.version.content_hash;
            let chunk_rows = by_hash.get(&hash).map(Vec::as_slice).unwrap_or_default();
            symbol_rows.extend(metadata_symbols_with_labels(
                &path,
                hash,
                chunk_rows,
                &structures,
                definitions_by_path
                    .get(&path)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
                labels.get(&path),
            )?);
            chunks.insert(
                path.clone(),
                chunk_rows
                    .iter()
                    .map(|row| {
                        let chunk = &row.chunk;
                        ChunkEntry {
                            lines: chunk.lines,
                            bytes: chunk.end_byte.saturating_sub(chunk.start_byte),
                            kind: chunk.kind.clone(),
                            symbol_path: chunk.symbol_path.clone(),
                            terms: BTreeSet::new(),
                            structure: structures.get(&(hash, chunk.ordinal)).cloned(),
                        }
                    })
                    .collect(),
            );
            files.insert(
                path,
                FileInfo {
                    content_hash: hash,
                    language: row.version.language,
                    size_bytes: row.size_bytes,
                    line_count: row.line_count.unwrap_or(0),
                    has_text: row.has_text,
                },
            );
        }
        for edge in edges {
            append_edge(
                edge,
                pin,
                &files,
                project_id,
                &mut imports,
                &mut other_edges,
            );
        }
        contracts.extend(graph::contracts_with_origins(&mut conn, pin, &origins).await?);
    }
    let mut snapshot = Snapshot::assemble(SnapshotParts {
        project,
        project_id,
        pin,
        files,
        symbols: symbol_rows,
        chunks,
        imports,
        other_edges,
        contracts,
    });
    snapshot.source_ready.clear();
    tracing::debug!(
        files = snapshot.files.len(),
        "prepared source-free snapshot metadata"
    );
    Ok(snapshot)
}

fn origins_of(paths: &[RepoPath]) -> Vec<String> {
    paths
        .iter()
        .flat_map(|path| {
            [
                path.to_string(),
                format!("{}:{path}", knowell_index::LINK_STAGE_NAME),
                format!("scip:{path}"),
            ]
        })
        .collect()
}

fn append_edge(
    edge: graph::Edge,
    pin: GenerationPin,
    files: &BTreeMap<RepoPath, FileInfo>,
    project_id: ProjectId,
    imports: &mut Vec<ImportEdge>,
    other_edges: &mut Vec<OtherEdge>,
) {
    let marked_scip = edge.edge.origin.starts_with("scip:")
        || edge
            .edge
            .evidence
            .get("analysis_kind")
            .and_then(serde_json::Value::as_str)
            == Some("scip");
    let origin = if marked_scip {
        crate::precise::scip_source_path(&edge, pin)
    } else {
        origin_path(&edge.edge.origin)
    };
    let Some(origin) = origin else {
        return;
    };
    let Some(file) = files.get(&origin) else {
        return;
    };
    if !crate::precise::admits_scip_edge(&edge, pin, file.content_hash) {
        return;
    }
    let data = edge.edge;
    let lines = evidence_lines(&data.evidence);
    match (data.kind.as_str(), &data.from, &data.to) {
        ("defines" | "contains", _, _) => {}
        (
            "imports",
            NodeRef::File {
                project: from_project,
                path: from,
            },
            NodeRef::File {
                project: to_project,
                path: to,
            },
        ) if *from_project == project_id && *to_project == project_id => imports.push(ImportEdge {
            from: from.clone(),
            to: ImportTarget::File(to.clone()),
            evidence: data.evidence_type,
            resolution: data.resolution,
            lines,
        }),
        (
            "imports",
            NodeRef::File {
                project,
                path: from,
            },
            NodeRef::Name { name, .. },
        ) if *project == project_id => imports.push(ImportEdge {
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

/// Parsed labels live in versioned `defines` evidence independently of chunk
/// grouping. Whole-file and grouped chunks therefore cannot erase names.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
struct PinnedSourceLabel {
    version: u32,
    symbol_id: SymbolId,
    name: String,
    qualified_name: String,
    kind: SymbolKind,
    name_line: u32,
    bytes: [u64; 2],
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceLabelEvidence {
    project: ProjectId,
    content_hash: ContentHash,
    label: PinnedSourceLabel,
}

#[derive(Debug, Default)]
struct DefinitionLabels {
    /// A present but invalid versioned label must not silently fall back to a
    /// chunk label. Its logical id remains marked even if its range is invalid.
    labeled_ids: BTreeSet<SymbolId>,
    /// Conflicting claims at one occurrence are deliberately kept as `None`.
    occurrences: BTreeMap<(SymbolId, LineRange), Option<SourceLabelEvidence>>,
}

fn metadata_definition_labels(
    edges: &[graph::Edge],
    pin: GenerationPin,
    project_id: ProjectId,
) -> BTreeMap<RepoPath, DefinitionLabels> {
    let mut labels: BTreeMap<RepoPath, DefinitionLabels> = BTreeMap::new();
    for edge in edges {
        let data = &edge.edge;
        if pin.generation <= 0
            || edge.view != pin.view
            || edge.valid_from <= 0
            || edge.valid_from > pin.generation
            || edge
                .valid_to
                .is_some_and(|end| end <= pin.generation || end <= edge.valid_from)
            || data.kind != "defines"
            || data.evidence_type != EvidenceType::Syntactic
            || data.resolution != Resolution::Resolved
        {
            continue;
        }
        let (NodeRef::File { project, path }, NodeRef::Symbol(id)) = (&data.from, &data.to) else {
            continue;
        };
        if *project != project_id || data.origin != path.as_str() {
            continue;
        }
        let Some(value) = data.evidence.get("source_label") else {
            continue;
        };
        let bound = labels.entry(path.clone()).or_default();
        bound.labeled_ids.insert(*id);
        let Some(lines) = data
            .evidence
            .get("lines")
            .and_then(serde_json::Value::as_array)
            .filter(|values| values.len() == 2)
            .and_then(|_| evidence_lines(&data.evidence))
        else {
            continue;
        };
        let parsed = source_label_evidence(value, &data.evidence, project_id, path, *id, lines);
        let occurrence = bound
            .occurrences
            .entry((*id, lines))
            .or_insert_with(|| parsed.clone());
        if *occurrence != parsed {
            *occurrence = None;
        }
    }
    labels
}

fn source_label_evidence(
    value: &serde_json::Value,
    evidence: &serde_json::Value,
    project: ProjectId,
    path: &RepoPath,
    id: SymbolId,
    lines: LineRange,
) -> Option<SourceLabelEvidence> {
    let label: PinnedSourceLabel = serde_json::from_value(value.clone()).ok()?;
    if label.version != 1
        || label.symbol_id != id
        || label.name.trim().is_empty()
        || label.qualified_name.trim().is_empty()
        || label.name.chars().any(char::is_control)
        || label.qualified_name.chars().any(char::is_control)
        || label.name_line < lines.start()
        || label.name_line > lines.end()
        || label.bytes.first()? >= label.bytes.get(1)?
        || evidence.get("path")?.as_str()? != path.as_str()
    {
        return None;
    }
    // Bytes refer to redacted parser input, not the original file's stored size.
    // The source-free path can verify ordering and identity, not byte contents.
    usize::try_from(*label.bytes.first()?).ok()?;
    usize::try_from(*label.bytes.get(1)?).ok()?;
    let content_hash = serde_json::from_value(evidence.get("content_hash")?.clone()).ok()?;
    Some(SourceLabelEvidence {
        project,
        content_hash,
        label,
    })
}

fn metadata_symbols_with_labels(
    path: &RepoPath,
    hash: ContentHash,
    rows: &[content::Chunk],
    structures: &BTreeMap<(ContentHash, u32), content::ChunkStructure>,
    definitions: &[symbols::Definition],
    labels: Option<&DefinitionLabels>,
) -> Result<Vec<SymbolEntry>, EngineError> {
    let Some(labels) = labels else {
        return metadata_symbols(path, hash, rows, structures, definitions);
    };
    let mut entries = Vec::new();
    for definition in definitions
        .iter()
        .filter(|row| row.path == *path && row.content_hash == hash)
    {
        let Some(Some(evidence)) = labels
            .occurrences
            .get(&(definition.symbol.id, definition.lines))
        else {
            continue;
        };
        let label = &evidence.label;
        if evidence.project != definition.symbol.project || evidence.content_hash != hash {
            continue;
        }
        entries.push(SymbolEntry {
            key: symbol_key(path, &label.qualified_name),
            local: label.qualified_name.clone(),
            name: label.name.clone(),
            kind: label.kind,
            path: path.clone(),
            lines: definition.lines,
            name_line: label.name_line,
            signature: String::new(),
            doc: None,
            parent: None,
            store_id: Some(label.symbol_id),
        });
    }
    entries.extend(metadata_symbols_legacy(
        path,
        hash,
        rows,
        structures,
        definitions,
        Some(&labels.labeled_ids),
    )?);
    entries.sort_by(|a, b| {
        a.lines
            .cmp(&b.lines)
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| a.store_id.cmp(&b.store_id))
    });
    entries.dedup();
    Ok(entries)
}

/// Stored logical symbols can be renamed after this pin. Legacy metadata needs
/// a uniquely bound immutable chunk label to recover the name without source.
fn metadata_symbols(
    path: &RepoPath,
    hash: ContentHash,
    rows: &[content::Chunk],
    structures: &BTreeMap<(ContentHash, u32), content::ChunkStructure>,
    definitions: &[symbols::Definition],
) -> Result<Vec<SymbolEntry>, EngineError> {
    metadata_symbols_legacy(path, hash, rows, structures, definitions, None)
}

fn metadata_symbols_legacy(
    path: &RepoPath,
    hash: ContentHash,
    rows: &[content::Chunk],
    structures: &BTreeMap<(ContentHash, u32), content::ChunkStructure>,
    definitions: &[symbols::Definition],
    labeled_ids: Option<&BTreeSet<SymbolId>>,
) -> Result<Vec<SymbolEntry>, EngineError> {
    let mut entries = Vec::new();
    let mut definitions_per_range: BTreeMap<LineRange, usize> = BTreeMap::new();
    for definition in definitions
        .iter()
        .filter(|row| row.path == *path && row.content_hash == hash)
    {
        let count = definitions_per_range.entry(definition.lines).or_default();
        *count = count.saturating_add(1);
    }
    for definition in definitions
        .iter()
        .filter(|row| row.path == *path && row.content_hash == hash)
        .filter(|row| !labeled_ids.is_some_and(|ids| ids.contains(&row.symbol.id)))
    {
        // A line anchor cannot distinguish same-line declarations. Do not attach
        // a source label to an arbitrary logical id before parsing exact bytes.
        if definitions_per_range.get(&definition.lines) != Some(&1) {
            continue;
        }
        let matching: BTreeSet<_> = rows
            .iter()
            .filter(|row| row.chunk.symbol_path.is_some())
            .filter(|row| {
                match structures
                    .get(&(hash, row.chunk.ordinal))
                    .and_then(|item| item.declaration)
                {
                    Some(declaration) => declaration.lines == definition.lines,
                    None => {
                        row.chunk.lines.start() == definition.lines.start()
                            && row.chunk.lines.end() <= definition.lines.end()
                    }
                }
            })
            .filter_map(|row| row.chunk.symbol_path.as_ref())
            .collect();
        // Continuation chunks may repeat one label. Distinct labels sharing the
        // range are ambiguous even when only one definition was persisted.
        if matching.len() != 1 {
            continue;
        }
        let Some(local) = matching.first().copied() else {
            continue;
        };
        let kind =
            serde_json::from_value(serde_json::Value::String(definition.symbol.kind.clone()))
                .map_err(|_| {
                    EngineError::internal("stored symbol kind is not supported by this parser")
                })?;
        entries.push(SymbolEntry {
            key: symbol_key(path, local),
            local: local.clone(),
            name: metadata_short_name(local, kind).to_owned(),
            kind,
            path: path.clone(),
            lines: definition.lines,
            name_line: definition.lines.start(),
            signature: String::new(),
            doc: None,
            parent: None,
            store_id: Some(definition.symbol.id),
        });
    }
    Ok(entries)
}

fn metadata_short_name(local: &str, kind: SymbolKind) -> &str {
    // The parser's qualified-name separators differ for documentation and tests.
    if matches!(kind, SymbolKind::Heading | SymbolKind::Test) {
        return local.rsplit(" > ").next().unwrap_or(local);
    }
    if kind == SymbolKind::Rule {
        return local;
    }
    local.rsplit('.').next().unwrap_or(local)
}

/// Hydrates only selected, source-authorized paths of an immutable snapshot.
/// No path or blob outside its pinned manifest can be substituted. Returned
/// metadata/relations stay complete while parser products cover selected paths.
pub(crate) async fn hydrate_paths(
    store: &Store,
    texts: &TextCache,
    organization: OrganizationId,
    snapshot: &Snapshot,
    paths: &[RepoPath],
    parse: ParseOptions,
) -> Result<Snapshot, EngineError> {
    let selected: BTreeSet<_> = paths.iter().cloned().collect();
    if selected.len() > MAX_HYDRATED_PATHS {
        return Err(EngineError::Invalid(
            "selected source paths exceed the hydration bound".into(),
        ));
    }
    if selected
        .iter()
        .any(|path| !snapshot.files.contains_key(path))
    {
        return Err(EngineError::NotFound(
            "selected source is absent from the pinned generation".into(),
        ));
    }
    let pending: Vec<_> = selected
        .into_iter()
        .filter(|path| !snapshot.source_ready.contains(path))
        .collect();
    if pending.is_empty() {
        return Ok(snapshot.clone());
    }
    let hashes: Vec<_> = pending
        .iter()
        .filter_map(|path| snapshot.file(path).map(|file| file.content_hash))
        .collect();
    let parser_version = parser_version_tag();
    let mut conn = store.acquire().await?;
    let mut by_hash: BTreeMap<ContentHash, Option<Arc<str>>> = BTreeMap::new();
    let mut missing = BTreeSet::new();
    let mut source_bytes = 0usize;
    for hash in &hashes {
        if by_hash.contains_key(hash) || missing.contains(hash) {
            continue;
        }
        if let Some(text) = texts.get(hash) {
            source_bytes = charge_source_bytes(source_bytes, text.len())?;
            by_hash.insert(*hash, Some(text));
        } else {
            missing.insert(*hash);
        }
    }
    // One body at a time prevents a batch of large selected files from being
    // materialized before its aggregate byte bound can be checked.
    for hash in missing {
        let remaining = MAX_HYDRATED_SOURCE_BYTES.saturating_sub(source_bytes);
        let stored = content::get_content_bounded(&mut conn, organization, &hash, remaining).await
            .map_err(|error| match error {
                knowell_store::StoreError::InvalidInput(_) => EngineError::Unavailable(
                    "selected source bodies exceed the hydration byte bound; narrow the query scope".into(),
                ),
                other => EngineError::from(other),
            })?;
        if let Some(blob) = stored {
            if let Some(text) = &blob.redacted_text {
                source_bytes = charge_source_bytes(source_bytes, text.len())?;
            }
            by_hash.insert(blob.hash, blob.redacted_text.map(Arc::from));
        }
    }
    let definitions = symbols::definitions_in_paths(&mut conn, snapshot.pin, &pending).await?;
    let structures = structure_map(
        content::chunk_structures_of(&mut conn, organization, &hashes, &parser_version).await?,
    );
    let mut chunks_by_hash: BTreeMap<ContentHash, Vec<content::Chunk>> = BTreeMap::new();
    for row in content::chunks_of_many(&mut conn, organization, &hashes, &parser_version).await? {
        chunks_by_hash
            .entry(row.chunk.content_hash)
            .or_default()
            .push(row);
    }
    drop(conn);
    let mut files = snapshot.files.clone();
    let mut bodies = Vec::new();
    let mut chunk_rows = BTreeMap::new();
    for path in &pending {
        let Some(info) = files.get_mut(path) else {
            return Err(EngineError::internal(
                "selected source metadata disappeared",
            ));
        };
        let text = by_hash.get(&info.content_hash).ok_or_else(|| {
            EngineError::Unavailable(
                "selected source content is missing from the pinned index".into(),
            )
        })?;
        if let Some(text) = text {
            info.line_count = line_count(text);
            info.has_text = true;
            texts.insert(info.content_hash, Arc::clone(text));
            bodies.push((path.clone(), Arc::clone(text)));
        } else {
            info.line_count = 0;
            info.has_text = false;
        }
        chunk_rows.insert(
            path.clone(),
            chunks_by_hash
                .get(&info.content_hash)
                .cloned()
                .unwrap_or_default(),
        );
    }
    let ParseOptions { limits, products } = parse;
    let (parsed_symbols, parsed_chunks) = tokio::task::spawn_blocking(move || {
        analyse(bodies, chunk_rows, structures, limits, products.as_deref())
    })
    .await
    .map_err(|error| EngineError::internal(format!("parsing selected source: {error}")))?;
    let pending_set: BTreeSet<_> = pending.iter().collect();
    let mut symbol_rows = retain_symbols(&snapshot.symbols, |symbol| {
        !pending_set.contains(&symbol.path)
    });
    let base = symbol_rows.len();
    let mut ids: BTreeMap<_, Vec<SymbolId>> = BTreeMap::new();
    for definition in &definitions {
        ids.entry((
            definition.path.clone(),
            definition.content_hash,
            definition.lines,
            definition.symbol.kind.clone(),
        ))
        .or_default()
        .push(definition.symbol.id);
    }
    for mut symbol in parsed_symbols {
        symbol.parent = symbol.parent.and_then(|parent| parent.checked_add(base));
        symbol.store_id = files.get(&symbol.path).and_then(|file| {
            let matches = ids.get(&(
                symbol.path.clone(),
                file.content_hash,
                symbol.lines,
                symbol.kind.as_str().to_owned(),
            ))?;
            // Same-line declarations can share a line range. An ambiguous source
            // anchor must not pick an arbitrary logical symbol or fabricate edges.
            if matches.len() == 1 {
                matches.first().copied()
            } else {
                None
            }
        });
        symbol_rows.push(symbol);
    }
    let mut chunks = snapshot.chunks.clone();
    chunks.extend(parsed_chunks);
    let mut out = Snapshot::assemble(SnapshotParts {
        project: snapshot.project.clone(),
        project_id: snapshot.project_id,
        pin: snapshot.pin,
        files,
        symbols: symbol_rows,
        chunks,
        imports: snapshot.imports.clone(),
        other_edges: snapshot.other_edges.clone(),
        contracts: snapshot.contracts.clone(),
    });
    out.source_ready = snapshot.source_ready.clone();
    out.source_ready.extend(pending);
    Ok(out)
}

fn charge_source_bytes(current: usize, added: usize) -> Result<usize, EngineError> {
    current
        .checked_add(added)
        .filter(|bytes| *bytes <= MAX_HYDRATED_SOURCE_BYTES)
        .ok_or_else(|| {
            EngineError::Unavailable(
                "selected source bodies exceed the hydration byte bound; narrow the query scope"
                    .into(),
            )
        })
}

fn retain_symbols(
    symbols: &[SymbolEntry],
    keep: impl Fn(&SymbolEntry) -> bool,
) -> Vec<SymbolEntry> {
    let mut retained = Vec::new();
    let mut indices = BTreeMap::new();
    for (index, symbol) in symbols.iter().enumerate() {
        if keep(symbol) {
            indices.insert(index, retained.len());
            retained.push(symbol.clone());
        }
    }
    for symbol in &mut retained {
        symbol.parent = symbol
            .parent
            .and_then(|parent| indices.get(&parent).copied());
    }
    retained
}

/// Reads and parses one generation. See the module docs for the cost.
pub(crate) async fn build(
    store: &Store,
    texts: &TextCache,
    organization: OrganizationId,
    project: Name,
    project_id: ProjectId,
    pin: GenerationPin,
    parse: ParseOptions,
) -> Result<Snapshot, EngineError> {
    let ParseOptions { limits, products } = parse;
    let parser_version = parser_version_tag();
    let mut conn = store.acquire().await?;
    let versions = content::files_at(&mut conn, pin).await?;
    let hashes: Vec<_> = versions
        .iter()
        .map(|version| version.content_hash)
        .collect();
    let stored = content::get_contents(&mut conn, organization, &hashes).await?;
    let mut by_hash = BTreeMap::new();
    for blob in stored {
        let text: Option<Arc<str>> = blob.redacted_text.map(Arc::from);
        let info = FileInfo {
            content_hash: blob.hash,
            language: None,
            size_bytes: blob.size_bytes,
            line_count: text.as_deref().map_or(0, line_count),
            has_text: text.is_some(),
        };
        if let Some(text) = &text {
            texts.insert(blob.hash, Arc::clone(text));
        }
        by_hash.insert(blob.hash, (info, text));
    }
    let mut chunks_by_hash: BTreeMap<ContentHash, Vec<content::Chunk>> = BTreeMap::new();
    for chunk in content::chunks_of_many(&mut conn, organization, &hashes, &parser_version).await? {
        chunks_by_hash
            .entry(chunk.chunk.content_hash)
            .or_default()
            .push(chunk);
    }
    let structures = structure_map(
        content::chunk_structures_of(&mut conn, organization, &hashes, &parser_version).await?,
    );
    let mut files = BTreeMap::new();
    let mut texts_by_path: Vec<(RepoPath, Arc<str>)> = Vec::new();
    let mut chunk_rows: BTreeMap<RepoPath, Vec<content::Chunk>> = BTreeMap::new();
    for version in versions {
        let path = version.path.clone();
        let hash = version.content_hash;
        let (mut info, text) = by_hash.get(&hash).cloned().ok_or_else(|| {
            EngineError::Unavailable("source content is missing from the pinned index".into())
        })?;
        info.language = version.language.clone();
        files.insert(path.clone(), info);
        if let Some(text) = text {
            texts_by_path.push((path.clone(), text));
        }
        let chunks = chunks_by_hash.get(&hash).cloned().unwrap_or_default();
        chunk_rows.insert(path.clone(), chunks);
    }
    tracing::debug!(
        files = files.len(),
        unique_blobs = by_hash.len(),
        "loaded snapshot content and chunks in batches"
    );
    let paths: Vec<RepoPath> = files.keys().cloned().collect();
    // Rows are grouped by origin: T1 writes the bare path, relation stages
    // prefix their name (`link:<path>`).
    let origins = origins_of(&paths);
    let mut imports = Vec::new();
    let mut other_edges = Vec::new();
    for edge in graph::edges_with_origins(&mut conn, pin, &origins).await? {
        append_edge(
            edge,
            pin,
            &files,
            project_id,
            &mut imports,
            &mut other_edges,
        );
    }
    let definitions = symbols::definitions_in_paths(&mut conn, pin, &paths).await?;
    let contracts = graph::contracts_with_origins(&mut conn, pin, &origins).await?;
    drop(conn);
    let parsed = tokio::task::spawn_blocking(move || {
        analyse(
            texts_by_path,
            chunk_rows,
            structures,
            limits,
            products.as_deref(),
        )
    })
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
        let mut language_counts = BTreeMap::<String, u64>::new();
        for file in files.values() {
            if let Some(language) = &file.language {
                let count = language_counts.entry(language.clone()).or_default();
                *count = count.saturating_add(1);
            }
        }
        Snapshot {
            project,
            project_id,
            pin,
            source_ready: files.keys().cloned().collect(),
            files,
            language_counts,
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

/// Loads or parses each local product and computes chunk terms (blocking).
fn analyse(
    texts: Vec<(RepoPath, Arc<str>)>,
    mut chunk_rows: BTreeMap<RepoPath, Vec<content::Chunk>>,
    structures: BTreeMap<(ContentHash, u32), content::ChunkStructure>,
    limits: ParseLimits,
    products: Option<&ParseProductCache>,
) -> Analysed {
    let mut symbols = Vec::new();
    let mut chunks: BTreeMap<RepoPath, Vec<ChunkEntry>> = BTreeMap::new();
    let mut reused = 0usize;
    let mut parsed_files = 0usize;
    for (path, text) in texts {
        let (parsed, cache_hit) = match products {
            Some(cache) => cache.parse(&path, &text, limits),
            None => (parse_with(&path, &text, &limits, None), false),
        };
        if cache_hit {
            reused = reused.saturating_add(1);
        } else {
            parsed_files = parsed_files.saturating_add(1);
        }
        let base = symbols.len();
        symbols.extend(symbol_entries(&path, &parsed, base));
        let rows = chunk_rows.remove(&path).unwrap_or_default();
        let entries = rows
            .into_iter()
            .map(|row| {
                let chunk = row.chunk;
                ChunkEntry {
                    structure: structures
                        .get(&(chunk.content_hash, chunk.ordinal))
                        .cloned(),
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
                    structure: structures
                        .get(&(row.chunk.content_hash, row.chunk.ordinal))
                        .cloned(),
                    bytes: row.chunk.end_byte.saturating_sub(row.chunk.start_byte),
                    lines: row.chunk.lines,
                    kind: row.chunk.kind,
                    symbol_path: row.chunk.symbol_path,
                    terms: BTreeSet::new(),
                })
                .collect()
        });
    }
    tracing::debug!(
        parsed_files,
        reused,
        "prepared snapshot local parse products"
    );
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
    cells: BTreeMap<(GenerationPin, SnapshotKind), (SnapshotCell, u64)>,
    tick: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SnapshotKind {
    Full,
    Metadata(MetadataScope),
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
        self.cell_of(pin, SnapshotKind::Full)
    }

    /// A separate metadata-only cell; it cannot replace a full graph snapshot.
    pub(crate) fn metadata_cell(&self, pin: GenerationPin) -> SnapshotCell {
        self.metadata_scoped_cell(pin, MetadataScope::default())
    }

    /// Partial catalogs cannot replace complete metadata or another hard scope.
    /// All scopes share the same bounded LRU cell capacity.
    pub(crate) fn metadata_scoped_cell(
        &self,
        pin: GenerationPin,
        scope: MetadataScope,
    ) -> SnapshotCell {
        self.cell_of(pin, SnapshotKind::Metadata(scope))
    }

    fn cell_of(&self, pin: GenerationPin, kind: SnapshotKind) -> SnapshotCell {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.tick = state.tick.wrapping_add(1);
        let tick = state.tick;
        let key = (pin, kind);
        let cell = match state.cells.get_mut(&key) {
            Some(entry) => {
                entry.1 = tick;
                Arc::clone(&entry.0)
            }
            None => {
                let cell: SnapshotCell = Arc::new(OnceCell::new());
                state.cells.insert(key.clone(), (Arc::clone(&cell), tick));
                cell
            }
        };
        while state.cells.len() > self.capacity {
            let oldest = state
                .cells
                .iter()
                .filter(|(p, _)| *p != &key)
                .min_by_key(|(_, (_, t))| *t)
                .map(|(p, _)| p.clone());
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
            .retain(|(pin, _), _| pin.view != view || pin.generation >= generation);
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

    struct PinnedMetadataFixture {
        path: RepoPath,
        hash: ContentHash,
        project: ProjectId,
        pin: GenerationPin,
        parsed: knowell_parse::ParsedFile,
        definitions: Vec<symbols::Definition>,
        edges: Vec<graph::Edge>,
        chunks: Vec<content::Chunk>,
    }

    impl PinnedMetadataFixture {
        fn new(path: &str, text: &str) -> Self {
            use knowell_store::{EdgeId, ViewId};
            use time::OffsetDateTime;
            use uuid::Uuid;

            let path = RepoPath::new(path).unwrap();
            let hash = ContentHash::of(text.as_bytes());
            let project = ProjectId(Uuid::from_u128(1));
            let pin = GenerationPin {
                view: ViewId(Uuid::from_u128(2)),
                generation: 7,
            };
            let parsed = parse_with(&path, text, &ParseLimits::default(), None);
            let mut definitions = Vec::new();
            let mut edges = Vec::new();
            for (index, symbol) in parsed.symbols.iter().enumerate() {
                let index = u128::try_from(index).unwrap();
                let id = SymbolId(Uuid::from_u128(index + 100));
                definitions.push(symbols::Definition {
                    symbol: symbols::Symbol {
                        id,
                        project,
                        qualified_name: symbol_key(&path, &symbol.qualified_name),
                        kind: symbol.kind.as_str().into(),
                        created_at: OffsetDateTime::UNIX_EPOCH,
                        updated_at: OffsetDateTime::UNIX_EPOCH,
                    },
                    path: path.clone(),
                    content_hash: hash,
                    lines: symbol.range,
                });
                edges.push(graph::Edge {
                    id: EdgeId(Uuid::from_u128(index + 200)),
                    view: pin.view,
                    valid_from: pin.generation,
                    valid_to: None,
                    edge: graph::NewEdge {
                        from: NodeRef::File {
                            project,
                            path: path.clone(),
                        },
                        to: NodeRef::Symbol(id),
                        kind: "defines".into(),
                        evidence_type: EvidenceType::Syntactic,
                        resolution: Resolution::Resolved,
                        origin: path.to_string(),
                        evidence: serde_json::json!({
                            "path":path, "content_hash":hash,
                            "lines":[symbol.range.start(),symbol.range.end()],
                            "source_label": {
                                "version":1, "symbol_id":id, "name":symbol.name,
                                "qualified_name":symbol.qualified_name,
                                "kind":symbol.kind.as_str(), "name_line":symbol.name_line,
                                "bytes":[symbol.byte_range.start,symbol.byte_range.end],
                            },
                        }),
                    },
                });
            }
            let chunks =
                knowell_parse::chunks(&parsed, text, &knowell_parse::ChunkOptions::default())
                    .unwrap()
                    .into_iter()
                    .map(|chunk| content::Chunk {
                        organization: OrganizationId(Uuid::from_u128(3)),
                        chunk: content::NewChunk {
                            content_hash: hash,
                            parser_version: parser_version_tag(),
                            ordinal: u32::try_from(chunk.ordinal).unwrap(),
                            lines: chunk.range,
                            start_byte: u64::try_from(chunk.byte_range.start).unwrap(),
                            end_byte: u64::try_from(chunk.byte_range.end).unwrap(),
                            kind: chunk.kind.as_str().into(),
                            symbol_path: chunk.symbol_path,
                            prepared_input_hash: ContentHash::of(chunk.text.as_bytes()),
                        },
                    })
                    .collect();
            Self {
                path,
                hash,
                project,
                pin,
                parsed,
                definitions,
                edges,
                chunks,
            }
        }

        fn entries(
            &self,
            edges: &[graph::Edge],
            definitions: &[symbols::Definition],
        ) -> Vec<SymbolEntry> {
            self.entries_at(self.pin, edges, definitions)
        }

        fn entries_at(
            &self,
            pin: GenerationPin,
            edges: &[graph::Edge],
            definitions: &[symbols::Definition],
        ) -> Vec<SymbolEntry> {
            let labels = metadata_definition_labels(edges, pin, self.project);
            metadata_symbols_with_labels(
                &self.path,
                self.hash,
                &self.chunks,
                &BTreeMap::new(),
                definitions,
                labels.get(&self.path),
            )
            .unwrap()
        }

        fn label_entries_at(
            &self,
            pin: GenerationPin,
            edges: &[graph::Edge],
            definitions: &[symbols::Definition],
        ) -> Vec<SymbolEntry> {
            let labels = metadata_definition_labels(edges, pin, self.project);
            // A valid pinned chunk can independently prove a name even when an
            // edge belongs to another generation. Isolate label admission here.
            metadata_symbols_with_labels(
                &self.path,
                self.hash,
                &[],
                &BTreeMap::new(),
                definitions,
                labels.get(&self.path),
            )
            .unwrap()
        }
    }

    #[test]
    fn pinned_definition_labels_survive_whole_file_and_grouped_chunks() {
        let grouped = (0..20)
            .map(|index| format!("pub fn probe_{index}() -> usize {{ {index} }}\n"))
            .collect::<String>();
        let fixtures = [
            PinnedMetadataFixture::new(
                "src/short.rs",
                "pub fn to_document(limit: usize) -> usize { limit + 99 }\npub fn other_format_probe() -> usize { 41 }\n",
            ),
            PinnedMetadataFixture::new("src/grouped.rs", &grouped),
        ];
        assert_eq!(fixtures[0].chunks.first().unwrap().chunk.kind, "file");
        assert!(
            fixtures[1]
                .chunks
                .iter()
                .any(|row| row.chunk.kind == "group")
        );
        for fixture in fixtures {
            assert!(
                fixture
                    .chunks
                    .iter()
                    .all(|row| row.chunk.symbol_path.is_none())
            );
            assert!(
                metadata_symbols(
                    &fixture.path,
                    fixture.hash,
                    &fixture.chunks,
                    &BTreeMap::new(),
                    &fixture.definitions
                )
                .unwrap()
                .is_empty()
            );
            let entries = fixture.entries(&fixture.edges, &fixture.definitions);
            assert_eq!(entries.len(), fixture.parsed.symbols.len());
            for symbol in &fixture.parsed.symbols {
                let entry = entries
                    .iter()
                    .find(|entry| entry.local == symbol.qualified_name)
                    .unwrap();
                assert_eq!(entry.name, symbol.name);
                assert_eq!(entry.kind, symbol.kind);
                assert_eq!(entry.lines, symbol.range);
                assert_eq!(entry.name_line, symbol.name_line);
                assert!(entry.signature.is_empty() && entry.doc.is_none());
            }
        }
    }

    #[test]
    fn pinned_definition_labels_keep_historical_names_and_parser_specific_leaf_names() {
        for fixture in [
            PinnedMetadataFixture::new(
                "src/original.rs",
                "/// Original documentation\n#[inline]\npub fn original() {}\n",
            ),
            PinnedMetadataFixture::new("docs/original.md", "# Root\n## Child\nBody.\n"),
        ] {
            let mut renamed = fixture.definitions.clone();
            for definition in &mut renamed {
                definition.symbol.qualified_name = "src/moved.rs#current_name".into();
                // Current APIs preserve kind, but pinned parsed facts must not
                // depend on a future logical metadata migration preserving it.
                definition.symbol.kind = "class".into();
            }
            let mut historical_edges = fixture.edges.clone();
            for edge in &mut historical_edges {
                edge.valid_from = 3;
                edge.valid_to = Some(8);
            }
            let entries = fixture.entries(&historical_edges, &renamed);
            assert_eq!(entries.len(), fixture.parsed.symbols.len());
            for symbol in &fixture.parsed.symbols {
                let entry = entries
                    .iter()
                    .find(|entry| entry.local == symbol.qualified_name)
                    .unwrap();
                assert_eq!(entry.key, symbol_key(&fixture.path, &symbol.qualified_name));
                assert_eq!(entry.name, symbol.name);
                assert_eq!(entry.kind, symbol.kind);
                assert_eq!(entry.name_line, symbol.name_line);
            }
            for generation in [2, 8] {
                let pin = GenerationPin {
                    generation,
                    ..fixture.pin
                };
                assert!(
                    fixture
                        .label_entries_at(pin, &historical_edges, &renamed)
                        .is_empty()
                );
                assert_eq!(
                    fixture.entries_at(pin, &historical_edges, &renamed),
                    metadata_symbols(
                        &fixture.path,
                        fixture.hash,
                        &fixture.chunks,
                        &BTreeMap::new(),
                        &renamed
                    )
                    .unwrap(),
                    "inactive edge labels must not alter independent legacy evidence",
                );
            }
        }
    }

    #[test]
    fn pinned_same_line_definition_labels_preserve_distinct_call_targets() {
        let fixture = PinnedMetadataFixture::new(
            "src/same_line.rs",
            "fn alpha() {} fn beta() {}\nfn call_alpha() { alpha(); }\nfn call_beta() { beta(); }\n",
        );
        let entries = fixture.entries(&fixture.edges, &fixture.definitions);
        assert_eq!(entries.len(), 4);
        let id = |name: &str| {
            entries
                .iter()
                .find(|entry| entry.name == name)
                .unwrap()
                .store_id
                .unwrap()
        };
        let alpha = id("alpha");
        let beta = id("beta");
        let caller = id("call_beta");
        assert_ne!(alpha, beta);
        let mut duplicated = fixture.definitions.clone();
        duplicated.extend(fixture.definitions.clone());
        assert_eq!(fixture.entries(&fixture.edges, &duplicated), entries);
        let snapshot = Snapshot::assemble(SnapshotParts {
            project: Name::new("fixture").unwrap(),
            project_id: fixture.project,
            pin: fixture.pin,
            files: BTreeMap::from([(
                fixture.path.clone(),
                FileInfo {
                    content_hash: fixture.hash,
                    language: Some("rust".into()),
                    size_bytes: 94,
                    line_count: 3,
                    has_text: true,
                },
            )]),
            symbols: entries,
            chunks: BTreeMap::new(),
            imports: Vec::new(),
            contracts: Vec::new(),
            other_edges: vec![OtherEdge {
                origin: fixture.path.clone(),
                from: NodeRef::Symbol(caller),
                to: NodeRef::Symbol(beta),
                kind: "calls".into(),
                evidence: EvidenceType::Syntactic,
                resolution: Resolution::Resolved,
                lines: Some(lines(3, 3)),
            }],
        });
        let relation = snapshot.uses_of(beta).next().unwrap();
        assert_eq!(
            snapshot.place_of(&relation.to).unwrap().2.unwrap().name,
            "beta"
        );
        assert_eq!(snapshot.symbol_by_id(alpha).unwrap().name, "alpha");
        assert_eq!(snapshot.symbol_by_id(caller).unwrap().name, "call_beta");
    }

    #[test]
    fn pinned_definition_labels_reject_malformed_or_conflicting_claims_without_chunk_fallback() {
        let mut fixture = PinnedMetadataFixture::new("src/labels.rs", "pub fn original() {}\n");
        fixture.chunks.first_mut().unwrap().chunk.symbol_path = Some("original".into());
        let original = fixture.edges.first().unwrap().clone();
        let mut unlabeled = original.clone();
        unlabeled
            .edge
            .evidence
            .as_object_mut()
            .unwrap()
            .remove("source_label");
        assert_eq!(
            fixture
                .entries(&[unlabeled], &fixture.definitions)
                .first()
                .unwrap()
                .name,
            "original"
        );
        let corruptions = [
            ("", serde_json::Value::Null),
            ("version", serde_json::json!(2)),
            ("symbol_id", serde_json::json!("not-an-id")),
            (
                "symbol_id",
                serde_json::json!("00000000-0000-0000-0000-000000000999"),
            ),
            ("name", serde_json::json!("")),
            ("qualified_name", serde_json::json!("\nspoof")),
            ("kind", serde_json::json!("not-a-kind")),
            ("name_line", serde_json::json!(0)),
            ("bytes", serde_json::json!([0, 0])),
            ("bytes", serde_json::json!([20, 10])),
            ("bytes", serde_json::json!([-1, 20])),
            ("bytes", serde_json::json!([0])),
            ("bytes", serde_json::json!([0, 20, 30])),
        ];
        for (field, value) in corruptions {
            let mut malformed = original.clone();
            if field.is_empty() {
                malformed.edge.evidence["source_label"] = value;
            } else {
                malformed.edge.evidence["source_label"][field] = value;
            }
            assert!(
                fixture
                    .entries(&[malformed], &fixture.definitions)
                    .is_empty(),
                "accepted invalid {field}"
            );
        }
        let mut conflicting = original.clone();
        conflicting.edge.evidence["source_label"]["qualified_name"] =
            serde_json::json!("another_name");
        for edges in [
            vec![original.clone(), conflicting.clone()],
            vec![conflicting, original.clone()],
        ] {
            assert!(fixture.entries(&edges, &fixture.definitions).is_empty());
        }
        assert_eq!(
            fixture
                .entries(&[original.clone(), original], &fixture.definitions)
                .len(),
            1
        );
    }

    #[test]
    fn pinned_definition_labels_require_every_source_and_occurrence_binding() {
        use knowell_store::ViewId;
        use uuid::Uuid;
        let fixture = PinnedMetadataFixture::new("src/labels.rs", "pub fn original() {}\n");
        let original = fixture.edges.first().unwrap();
        let mut invalid_edges = Vec::new();
        for (field, value) in [
            ("path", serde_json::json!("src/another.rs")),
            (
                "content_hash",
                serde_json::json!(ContentHash::of(b"another body")),
            ),
            ("lines", serde_json::json!([2, 2])),
            ("lines", serde_json::json!([1, 1, 1])),
        ] {
            let mut edge = original.clone();
            edge.edge.evidence[field] = value;
            invalid_edges.push(edge);
        }
        let mut edge = original.clone();
        edge.view = ViewId(Uuid::from_u128(999));
        invalid_edges.push(edge);
        let mut edge = original.clone();
        edge.valid_from = 8;
        invalid_edges.push(edge);
        let mut edge = original.clone();
        edge.valid_to = Some(7);
        invalid_edges.push(edge);
        let mut edge = original.clone();
        edge.edge.origin = "src/another.rs".into();
        invalid_edges.push(edge);
        let mut edge = original.clone();
        edge.edge.from = NodeRef::File {
            project: ProjectId(Uuid::from_u128(999)),
            path: fixture.path.clone(),
        };
        invalid_edges.push(edge);
        let mut edge = original.clone();
        edge.edge.to = NodeRef::Symbol(SymbolId(Uuid::from_u128(999)));
        invalid_edges.push(edge);
        let mut edge = original.clone();
        edge.edge.evidence_type = EvidenceType::SemanticResolved;
        invalid_edges.push(edge);
        let mut edge = original.clone();
        edge.edge.resolution = Resolution::Unresolved;
        invalid_edges.push(edge);
        for edge in invalid_edges {
            let labels = metadata_definition_labels(
                std::slice::from_ref(&edge),
                fixture.pin,
                fixture.project,
            );
            assert!(
                fixture
                    .label_entries_at(
                        fixture.pin,
                        std::slice::from_ref(&edge),
                        &fixture.definitions
                    )
                    .is_empty()
            );
            if labels.get(&fixture.path).is_some_and(|bound| {
                bound
                    .labeled_ids
                    .contains(&fixture.definitions.first().unwrap().symbol.id)
            }) {
                assert!(
                    fixture.entries(&[edge], &fixture.definitions).is_empty(),
                    "invalid present label must suppress its chunk fallback"
                );
            } else {
                assert_eq!(
                    fixture.entries(&[edge], &fixture.definitions),
                    metadata_symbols(
                        &fixture.path,
                        fixture.hash,
                        &fixture.chunks,
                        &BTreeMap::new(),
                        &fixture.definitions
                    )
                    .unwrap(),
                    "unrelated edge cannot invalidate a separate pinned chunk proof"
                );
            }
        }
        let original_definition = fixture.definitions.first().unwrap();
        let mut invalid_definitions = Vec::new();
        let mut definition = original_definition.clone();
        definition.symbol.project = ProjectId(Uuid::from_u128(999));
        invalid_definitions.push(definition);
        let mut definition = original_definition.clone();
        definition.symbol.id = SymbolId(Uuid::from_u128(999));
        invalid_definitions.push(definition);
        let mut definition = original_definition.clone();
        definition.path = RepoPath::new("src/another.rs").unwrap();
        invalid_definitions.push(definition);
        let mut definition = original_definition.clone();
        definition.content_hash = ContentHash::of(b"another body");
        invalid_definitions.push(definition);
        let mut definition = original_definition.clone();
        definition.lines = lines(2, 2);
        invalid_definitions.push(definition);
        for definition in invalid_definitions {
            assert!(
                fixture
                    .label_entries_at(fixture.pin, &fixture.edges, &[definition])
                    .is_empty()
            );
        }
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
            structure: None,
        };
        let chunks = vec![
            chunk(1, 50, None, &["cancel", "subscription"]),
            chunk(10, 20, Some("Svc.cancel"), &["cancel", "subscription"]),
            chunk(30, 40, Some("Svc.other"), &["cancel"]),
        ];
        let terms = vec!["cancel".to_owned(), "subscription".to_owned()];
        assert_eq!(
            best_chunks(&chunks, &terms, 1)
                .first()
                .unwrap()
                .symbol_path
                .as_deref(),
            Some("Svc.cancel")
        );
        assert!(best_chunks(&chunks, &["refund".to_owned()], 3).is_empty());
    }

    #[test]
    fn lexical_spans_keep_separate_implementations_without_repeating_parent_text() {
        let chunk = |a, b, symbol: Option<&str>, text: &str| ChunkEntry {
            lines: lines(a, b),
            bytes: u64::try_from(text.len()).unwrap(),
            kind: "function".into(),
            symbol_path: symbol.map(str::to_owned),
            terms: terms_of(text),
            structure: None,
        };
        let mut chunks = vec![
            chunk(1, 100, None, "decode container"),
            chunk(5, 12, Some("decode_header"), "decode container"),
            chunk(30, 45, Some("decode_objects"), "decode container"),
            chunk(70, 78, Some("mapping"), "document mapping"),
        ];
        let terms = vec!["decode".into(), "container".into(), "decode".into()];
        let selected = best_chunks(&chunks, &terms, 3);
        assert_eq!(
            selected.iter().map(|chunk| chunk.lines).collect::<Vec<_>>(),
            vec![lines(5, 12), lines(30, 45)],
        );
        let expected: Vec<_> = selected.into_iter().cloned().collect();
        chunks.reverse();
        let reordered: Vec<_> = best_chunks(&chunks, &terms, 3)
            .into_iter()
            .cloned()
            .collect();
        assert_eq!(expected, reordered);
        assert!(best_chunks(&chunks, &terms, 0).is_empty());
        assert_eq!(best_chunks(&chunks, &terms, 1).len(), 1);
    }

    #[test]
    fn lexical_span_selection_does_not_count_repeated_query_terms_twice() {
        let chunk = |a, text: &str| ChunkEntry {
            lines: lines(a, a),
            bytes: u64::try_from(text.len()).unwrap(),
            kind: "section".into(),
            symbol_path: None,
            terms: terms_of(text),
            structure: None,
        };
        let chunks = vec![chunk(1, "decode"), chunk(2, "container objects")];
        let terms = vec![
            "decode".into(),
            "decode".into(),
            "decode".into(),
            "container".into(),
            "objects".into(),
        ];
        assert_eq!(
            best_chunks(&chunks, &terms, 1).first().unwrap().lines,
            lines(2, 2)
        );
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

    #[test]
    fn metadata_names_remain_bound_to_pinned_source_after_logical_symbol_rename() {
        use time::OffsetDateTime;
        use uuid::Uuid;

        let path = RepoPath::new("src/fixture.rs").unwrap();
        let hash = ContentHash::of(b"pub fn original() {}\n");
        let organization = OrganizationId(Uuid::from_u128(1));
        let symbol_id = SymbolId(Uuid::from_u128(2));
        let definition = symbols::Definition {
            symbol: symbols::Symbol {
                id: symbol_id,
                project: ProjectId(Uuid::from_u128(3)),
                qualified_name: "src/renamed.rs#renamed".into(),
                kind: "function".into(),
                created_at: OffsetDateTime::UNIX_EPOCH,
                updated_at: OffsetDateTime::UNIX_EPOCH,
            },
            path: path.clone(),
            content_hash: hash,
            lines: lines(1, 1),
        };
        let row = content::Chunk {
            organization,
            chunk: content::NewChunk {
                content_hash: hash,
                parser_version: parser_version_tag(),
                ordinal: 0,
                lines: lines(1, 1),
                start_byte: 0,
                end_byte: 20,
                kind: "function".into(),
                symbol_path: Some("original".into()),
                prepared_input_hash: ContentHash::of(b"prepared"),
            },
        };
        let entries = metadata_symbols(
            &path,
            hash,
            std::slice::from_ref(&row),
            &BTreeMap::new(),
            std::slice::from_ref(&definition),
        )
        .unwrap();
        assert_eq!(entries.len(), 1);
        let entry = entries.first().unwrap();
        assert_eq!(entry.key, "src/fixture.rs#original");
        assert_eq!(entry.name, "original");
        assert_eq!(entry.store_id, Some(symbol_id));
        assert!(entry.signature.is_empty());
        assert!(entry.doc.is_none());
        let mut wrong = definition.clone();
        wrong.content_hash = ContentHash::of(b"another source");
        assert!(
            metadata_symbols(&path, hash, &[row], &BTreeMap::new(), &[wrong])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn metadata_without_a_pinned_name_does_not_reuse_current_logical_name() {
        use time::OffsetDateTime;
        use uuid::Uuid;
        let path = RepoPath::new("src/fixture.rs").unwrap();
        let hash = ContentHash::of(b"fixture");
        let definition = symbols::Definition {
            symbol: symbols::Symbol {
                id: SymbolId(Uuid::from_u128(1)),
                project: ProjectId(Uuid::from_u128(2)),
                qualified_name: "src/fixture.rs#current".into(),
                kind: "function".into(),
                created_at: OffsetDateTime::UNIX_EPOCH,
                updated_at: OffsetDateTime::UNIX_EPOCH,
            },
            path: path.clone(),
            content_hash: hash,
            lines: lines(2, 9),
        };
        assert!(
            metadata_symbols(&path, hash, &[], &BTreeMap::new(), &[definition])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn same_line_declarations_never_relabel_distinct_call_targets() {
        use time::OffsetDateTime;
        use uuid::Uuid;

        let path = RepoPath::new("src/same_line.rs").unwrap();
        let hash = ContentHash::of(b"fn alpha() {} fn beta() {}\nfn call_alpha() { alpha(); }\nfn call_beta() { beta(); }\n");
        let organization = OrganizationId(Uuid::from_u128(1));
        let project_id = ProjectId(Uuid::from_u128(2));
        let definition = |label: &str, line: u32, id: u128| symbols::Definition {
            symbol: symbols::Symbol {
                id: SymbolId(Uuid::from_u128(id)),
                project: project_id,
                qualified_name: format!("{path}#{label}"),
                kind: "function".into(),
                created_at: OffsetDateTime::UNIX_EPOCH,
                updated_at: OffsetDateTime::UNIX_EPOCH,
            },
            path: path.clone(),
            content_hash: hash,
            lines: lines(line, line),
        };
        let definitions = vec![
            definition("alpha", 1, 10),
            definition("beta", 1, 11),
            definition("call_alpha", 2, 12),
            definition("call_beta", 3, 13),
        ];
        let rows: Vec<_> = definitions
            .iter()
            .enumerate()
            .map(|(ordinal, definition)| content::Chunk {
                organization,
                chunk: content::NewChunk {
                    content_hash: hash,
                    parser_version: parser_version_tag(),
                    ordinal: u32::try_from(ordinal).unwrap(),
                    lines: definition.lines,
                    start_byte: u64::try_from(ordinal * 20).unwrap(),
                    end_byte: u64::try_from(ordinal * 20 + 20).unwrap(),
                    kind: "function".into(),
                    symbol_path: Some(
                        definition
                            .symbol
                            .qualified_name
                            .split_once('#')
                            .unwrap()
                            .1
                            .into(),
                    ),
                    prepared_input_hash: ContentHash::of(b"prepared fixture"),
                },
            })
            .collect();
        let structures = structure_map(
            rows.iter()
                .map(|row| content::ChunkStructure {
                    key: content::ChunkKey {
                        content_hash: hash,
                        parser_version: parser_version_tag(),
                        ordinal: row.chunk.ordinal,
                    },
                    parent_ordinal: None,
                    declaration: Some(content::SourceRange {
                        lines: row.chunk.lines,
                        start_byte: row.chunk.start_byte,
                        end_byte: row.chunk.end_byte,
                    }),
                    enclosing: None,
                    source_exact: true,
                })
                .collect(),
        );
        for structure in [BTreeMap::new(), structures] {
            let symbols = metadata_symbols(&path, hash, &rows, &structure, &definitions).unwrap();
            assert_eq!(
                symbols
                    .iter()
                    .map(|symbol| symbol.local.as_str())
                    .collect::<Vec<_>>(),
                vec!["call_alpha", "call_beta"]
            );
            // Even a partial occurrence catalog cannot select the first of two
            // distinct source labels merely because its ordinal is smaller.
            assert!(
                metadata_symbols(
                    &path,
                    hash,
                    &rows,
                    &structure,
                    definitions.get(1..2).unwrap()
                )
                .unwrap()
                .is_empty()
            );
            let call = |caller: u128, target: u128, line: u32| OtherEdge {
                origin: path.clone(),
                from: NodeRef::Symbol(SymbolId(Uuid::from_u128(caller))),
                to: NodeRef::Symbol(SymbolId(Uuid::from_u128(target))),
                kind: "calls".into(),
                evidence: EvidenceType::Syntactic,
                resolution: Resolution::Resolved,
                lines: Some(lines(line, line)),
            };
            let snapshot = Snapshot::assemble(SnapshotParts {
                project: Name::new("fixture").unwrap(),
                project_id,
                pin: GenerationPin {
                    view: knowell_store::ViewId(Uuid::from_u128(3)),
                    generation: 1,
                },
                files: BTreeMap::from([(
                    path.clone(),
                    FileInfo {
                        content_hash: hash,
                        language: Some("rust".into()),
                        size_bytes: 80,
                        line_count: 3,
                        has_text: true,
                    },
                )]),
                symbols,
                chunks: BTreeMap::new(),
                imports: Vec::new(),
                other_edges: vec![call(12, 10, 2), call(13, 11, 3)],
                contracts: Vec::new(),
            });
            assert!(
                snapshot
                    .symbol_by_id(SymbolId(Uuid::from_u128(10)))
                    .is_none()
            );
            assert!(
                snapshot
                    .symbol_by_id(SymbolId(Uuid::from_u128(11)))
                    .is_none()
            );
            let beta_call = snapshot
                .uses_of(SymbolId(Uuid::from_u128(11)))
                .next()
                .unwrap();
            assert_eq!(
                beta_call.from,
                NodeRef::Symbol(SymbolId(Uuid::from_u128(13)))
            );
            assert!(snapshot.place_of(&beta_call.to).is_none());
            assert_eq!(
                snapshot
                    .symbol_by_id(SymbolId(Uuid::from_u128(13)))
                    .unwrap()
                    .name,
                "call_beta"
            );
        }
    }

    #[test]
    fn retaining_selected_symbols_remaps_parents_without_cross_file_inference() {
        let first_path = RepoPath::new("src/first.rs").unwrap();
        let second_path = RepoPath::new("src/second.rs").unwrap();
        let text = "struct Fixture;\nimpl Fixture {\n fn action(&self) {}\n}\n";
        let first = parse_with(&first_path, text, &ParseLimits::default(), None);
        let second = parse_with(&second_path, text, &ParseLimits::default(), None);
        let mut entries = symbol_entries(&first_path, &first, 0);
        entries.extend(symbol_entries(&second_path, &second, entries.len()));
        let retained = retain_symbols(&entries, |symbol| symbol.path == second_path);
        let method = retained
            .iter()
            .find(|symbol| symbol.name == "action")
            .unwrap();
        let parent = retained.get(method.parent.unwrap()).unwrap();
        assert_eq!(parent.path, second_path);
        assert_eq!(parent.kind, SymbolKind::Impl);
        assert!(
            retain_symbols(&entries, |symbol| symbol.name == "action")
                .iter()
                .all(|symbol| symbol.parent.is_none())
        );
    }

    #[test]
    fn selected_source_byte_bound_is_checked_and_does_not_overflow() {
        assert_eq!(
            charge_source_bytes(0, MAX_HYDRATED_SOURCE_BYTES).unwrap(),
            MAX_HYDRATED_SOURCE_BYTES
        );
        assert!(matches!(
            charge_source_bytes(MAX_HYDRATED_SOURCE_BYTES, 1),
            Err(EngineError::Unavailable(_))
        ));
        assert!(matches!(
            charge_source_bytes(usize::MAX, 1),
            Err(EngineError::Unavailable(_))
        ));
    }

    #[test]
    fn full_and_metadata_snapshots_have_distinct_cells_and_retire_together() {
        use uuid::Uuid;
        let cache = SnapshotCache::new(4);
        let view = knowell_store::ViewId(Uuid::from_u128(1));
        let pin = GenerationPin {
            view,
            generation: 1,
        };
        let full = cache.cell(pin);
        let metadata = cache.metadata_cell(pin);
        assert!(!Arc::ptr_eq(&full, &metadata));
        assert!(Arc::ptr_eq(&full, &cache.cell(pin)));
        assert!(Arc::ptr_eq(&metadata, &cache.metadata_cell(pin)));
        let current = GenerationPin {
            view,
            generation: 2,
        };
        let current_cell = cache.metadata_cell(current);
        cache.retire_older(view, 2);
        assert!(Arc::ptr_eq(&current_cell, &cache.metadata_cell(current)));
        assert!(!Arc::ptr_eq(&metadata, &cache.metadata_cell(pin)));
        assert!(!Arc::ptr_eq(&full, &cache.cell(pin)));
    }

    #[test]
    fn metadata_cache_scope_is_normalized_isolated_and_capacity_bounded() {
        use uuid::Uuid;
        let cache = SnapshotCache::new(3);
        let pin = GenerationPin {
            view: knowell_store::ViewId(Uuid::from_u128(2)),
            generation: 1,
        };
        let scope = MetadataScope::new(
            &["src/b/".into(), "src/a/".into(), "src/a/".into()],
            Some(vec!["rust".into(), "typescript".into(), "rust".into()]),
        );
        let equivalent = MetadataScope::new(
            &["src/a/".into(), "src/b/".into()],
            Some(vec!["typescript".into(), "rust".into()]),
        );
        let partial = cache.metadata_scoped_cell(pin, scope.clone());
        assert!(Arc::ptr_eq(
            &partial,
            &cache.metadata_scoped_cell(pin, equivalent)
        ));
        let global = cache.metadata_cell(pin);
        assert!(!Arc::ptr_eq(&partial, &global));
        let none = cache.metadata_scoped_cell(pin, MetadataScope::new(&[], Some(Vec::new())));
        assert!(!Arc::ptr_eq(&none, &global));
        assert_eq!(cache.state.lock().unwrap().cells.len(), 3);
        let full = cache.cell(pin);
        assert_eq!(cache.state.lock().unwrap().cells.len(), 3);
        assert!(!Arc::ptr_eq(
            &partial,
            &cache.metadata_scoped_cell(pin, scope)
        ));
        assert!(!Arc::ptr_eq(&full, &global));
        let current = GenerationPin {
            generation: 2,
            ..pin
        };
        let current_scope = MetadataScope::new(&["src/new/".into()], None);
        let current_cell = cache.metadata_scoped_cell(current, current_scope.clone());
        cache.retire_older(pin.view, 2);
        assert!(Arc::ptr_eq(
            &current_cell,
            &cache.metadata_scoped_cell(current, current_scope)
        ));
        assert!(
            cache
                .state
                .lock()
                .unwrap()
                .cells
                .keys()
                .all(|(key, _)| key.generation == 2)
        );
    }

    #[test]
    fn snapshot_admits_scip_origins_only_with_exact_pin_path_and_source_hash() {
        use knowell_store::{EdgeId, ViewId};
        use uuid::Uuid;
        let project = ProjectId(Uuid::from_u128(3));
        let path = RepoPath::new("src/compiler.rs").unwrap();
        let hash = ContentHash::of(b"synthetic compiler source");
        let pin = GenerationPin {
            view: ViewId(Uuid::from_u128(4)),
            generation: 7,
        };
        let files = [(
            path.clone(),
            FileInfo {
                content_hash: hash,
                language: Some("rust".into()),
                size_bytes: 25,
                line_count: 1,
                has_text: true,
            },
        )]
        .into();
        let edge = graph::Edge {
            id: EdgeId(Uuid::from_u128(5)),
            view: pin.view,
            valid_from: pin.generation,
            valid_to: None,
            edge: graph::NewEdge {
                from: NodeRef::File {
                    project,
                    path: path.clone(),
                },
                to: NodeRef::File {
                    project,
                    path: path.clone(),
                },
                kind: "references".into(),
                evidence_type: EvidenceType::SemanticResolved,
                resolution: Resolution::Resolved,
                origin: format!("scip:{path}"),
                evidence: serde_json::json!({
                    "analysis_kind":"scip", "analysis_format":1, "view":pin.view,
                    "generation":pin.generation, "path":path, "content_hash":hash,
                    "artifact_hash":hash, "analysis_input_hash":hash, "compiler_identity":hash,
                    "source_revision":"a".repeat(40), "lines":[1,1],
                }),
            },
        };
        let mut imports = Vec::new();
        let mut relations = Vec::new();
        append_edge(
            edge.clone(),
            pin,
            &files,
            project,
            &mut imports,
            &mut relations,
        );
        assert_eq!(relations.len(), 1);
        assert_eq!(
            relations[0].origin,
            RepoPath::new("src/compiler.rs").unwrap()
        );
        relations.clear();
        let mut wrong_hash = edge.clone();
        wrong_hash.edge.evidence["content_hash"] =
            serde_json::json!(ContentHash::of(b"other synthetic body"));
        let mut wrong_path = edge.clone();
        wrong_path.edge.evidence["path"] = serde_json::json!("src/not-in-catalog.rs");
        wrong_path.edge.origin = "scip:src/not-in-catalog.rs".into();
        let mut stale = edge;
        stale.edge.evidence["generation"] = serde_json::json!(pin.generation - 1);
        for invalid in [wrong_hash, wrong_path, stale] {
            append_edge(invalid, pin, &files, project, &mut imports, &mut relations);
        }
        assert!(relations.is_empty() && imports.is_empty());
    }
}
