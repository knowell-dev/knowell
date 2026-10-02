//! Reading SCIP files and querying the ingested index.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::Path;

use knowell_core::{ContentHash, RepoPath};
use protobuf::Message;
use scip::types as pb;

use crate::error::ScipError;
use crate::model::{
    Document, Located, Occurrence, PositionEncoding, PreciseSymbol, Relationship, Span, SymbolId,
    SymbolKind, SymbolRoles,
};
use crate::symbol::{MAX_SYMBOL_LEN, ScipSymbol};

/// Size and count bounds applied while reading an index. SCIP files come from
/// external tools and are untrusted input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Largest accepted encoded index, in bytes.
    pub max_bytes: u64,
    /// Largest accepted number of documents.
    pub max_documents: usize,
    /// Largest accepted total number of occurrences.
    pub max_occurrences: usize,
    /// Largest accepted number of distinct symbols.
    pub max_symbols: usize,
    /// Documentation kept per symbol, in bytes (longer text is truncated).
    pub max_documentation_bytes: usize,
    /// Relationships kept per symbol (extra ones are dropped).
    pub max_relationships_per_symbol: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 512 * 1024 * 1024,
            max_documents: 500_000,
            max_occurrences: 50_000_000,
            max_symbols: 10_000_000,
            max_documentation_bytes: 16 * 1024,
            max_relationships_per_symbol: 256,
        }
    }
}

/// Provenance recorded in the index file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexMetadata {
    /// Indexer name, for example `rust-analyzer`.
    pub tool_name: String,
    /// Indexer version.
    pub tool_version: String,
    /// The project root URI the indexer recorded. Informational only: document
    /// paths are validated as relative and are never joined with it.
    pub project_root: String,
}

/// A document the reader refused, kept so the caller can report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedDocument {
    /// The path as written in the index (truncated to 256 bytes).
    pub path: String,
    /// Why it was refused.
    pub reason: String,
}

/// What the reader dropped while ingesting. Nothing is dropped silently.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestReport {
    /// Total number of documents refused for an invalid or escaping path.
    pub rejected_document_count: usize,
    /// The first refused documents (at most 100).
    pub rejected_documents: Vec<RejectedDocument>,
    /// Occurrences dropped for a malformed range, empty or oversize symbol.
    pub invalid_occurrences: usize,
    /// Enclosing ranges dropped because they were malformed (the occurrence is kept).
    pub invalid_enclosing_ranges: usize,
    /// Symbol records dropped for an empty or oversize moniker.
    pub invalid_symbols: usize,
}

/// How a document relates to current file content; see [`ScipIndex::coverage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// The document's recorded hash equals the current content hash.
    Fresh,
    /// The recorded hash differs: the file changed after indexing.
    Stale,
    /// The document is indexed but no hash is recorded, so nothing is proven.
    Unknown,
    /// The index has no document for this path.
    NotIndexed,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SymbolKey {
    scope: Option<RepoPath>,
    moniker: String,
}

/// An ingested SCIP index with cross-file queries.
///
/// Documents are sorted by path and occurrences by span, so every query result
/// has a deterministic order.
#[derive(Debug, Clone)]
pub struct ScipIndex {
    metadata: IndexMetadata,
    source_revision: Option<String>,
    documents: Vec<Document>,
    doc_by_path: HashMap<RepoPath, usize>,
    symbols: Vec<PreciseSymbol>,
    lookup: HashMap<SymbolKey, SymbolId>,
    /// Per symbol: `(document index, occurrence index)` in document order.
    by_symbol: Vec<Vec<(usize, usize)>>,
    /// Target moniker -> symbols that declare an `is_implementation` relationship to it.
    implementers: HashMap<String, Vec<SymbolId>>,
    report: IngestReport,
}

/// Reads and ingests an index file.
///
/// The size is checked before the file is read into memory.
pub fn read_index_file(path: &Path, limits: &Limits) -> Result<ScipIndex, ScipError> {
    let io_err = |source| ScipError::Io {
        path: path.display().to_string(),
        source,
    };
    let file = std::fs::File::open(path).map_err(io_err)?;
    let size = file.metadata().map_err(io_err)?.len();
    if size > limits.max_bytes {
        return Err(ScipError::TooLarge {
            size,
            limit: limits.max_bytes,
        });
    }
    let mut bytes = Vec::new();
    // `take` guards against the file growing between the metadata check and the read.
    file.take(limits.max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io_err)?;
    read_index_bytes(&bytes, limits)
}

/// Decodes and ingests an index from memory.
pub fn read_index_bytes(bytes: &[u8], limits: &Limits) -> Result<ScipIndex, ScipError> {
    let size = bytes.len() as u64;
    if size > limits.max_bytes {
        return Err(ScipError::TooLarge {
            size,
            limit: limits.max_bytes,
        });
    }
    let raw = pb::Index::parse_from_bytes(bytes).map_err(|e| ScipError::Decode(e.to_string()))?;
    ScipIndex::from_message(&raw, limits)
}

impl ScipIndex {
    /// Reads an index file; see [`read_index_file`].
    pub fn read_file(path: &Path, limits: &Limits) -> Result<Self, ScipError> {
        read_index_file(path, limits)
    }

    /// Decodes an index from bytes; see [`read_index_bytes`].
    pub fn read_bytes(bytes: &[u8], limits: &Limits) -> Result<Self, ScipError> {
        read_index_bytes(bytes, limits)
    }

    /// Ingests an already decoded SCIP message.
    pub fn from_message(raw: &pb::Index, limits: &Limits) -> Result<Self, ScipError> {
        if raw.documents.len() > limits.max_documents {
            return Err(ScipError::LimitExceeded {
                what: "documents",
                limit: limits.max_documents,
            });
        }
        let mut total_occurrences = 0usize;
        for d in &raw.documents {
            total_occurrences = total_occurrences.saturating_add(d.occurrences.len());
        }
        if total_occurrences > limits.max_occurrences {
            return Err(ScipError::LimitExceeded {
                what: "occurrences",
                limit: limits.max_occurrences,
            });
        }

        let metadata = IndexMetadata {
            tool_name: raw
                .metadata
                .tool_info
                .as_ref()
                .map(|t| t.name.clone())
                .unwrap_or_default(),
            tool_version: raw
                .metadata
                .tool_info
                .as_ref()
                .map(|t| t.version.clone())
                .unwrap_or_default(),
            project_root: raw
                .metadata
                .as_ref()
                .map(|m| m.project_root.clone())
                .unwrap_or_default(),
        };

        let mut b = Builder {
            limits,
            symbols: Vec::new(),
            lookup: HashMap::new(),
            docs: BTreeMap::new(),
            report: IngestReport::default(),
        };

        for doc in &raw.documents {
            b.ingest_document(doc)?;
        }
        for info in &raw.external_symbols {
            b.ingest_symbol_info(info, None)?;
        }
        b.finish(metadata)
    }

    /// Provenance recorded by the indexer.
    pub fn metadata(&self) -> &IndexMetadata {
        &self.metadata
    }

    /// What the reader dropped while ingesting.
    pub fn report(&self) -> &IngestReport {
        &self.report
    }

    /// The commit (or other revision label) this index was built from, if the
    /// caller recorded one with [`ScipIndex::set_source_revision`].
    pub fn source_revision(&self) -> Option<&str> {
        self.source_revision.as_deref()
    }

    /// Records the commit the index was built from. SCIP files do not carry it.
    pub fn set_source_revision(&mut self, revision: impl Into<String>) {
        self.source_revision = Some(revision.into());
    }

    /// All documents, sorted by path.
    pub fn documents(&self) -> &[Document] {
        &self.documents
    }

    /// The document for `path`.
    pub fn document(&self, path: &RepoPath) -> Option<&Document> {
        self.doc_by_path
            .get(path)
            .and_then(|&i| self.documents.get(i))
    }

    /// All symbols in ingestion order (the order [`SymbolId`] refers to).
    pub fn symbols(&self) -> &[PreciseSymbol] {
        &self.symbols
    }

    /// The symbol behind an id.
    pub fn symbol(&self, id: SymbolId) -> Option<&PreciseSymbol> {
        self.symbols.get(id.0)
    }

    /// Finds a global symbol by its SCIP moniker.
    pub fn find_symbol(&self, moniker: &str) -> Option<SymbolId> {
        self.lookup
            .get(&SymbolKey {
                scope: None,
                moniker: moniker.to_owned(),
            })
            .copied()
    }

    /// Finds a `local N` symbol inside one document.
    pub fn find_local_symbol(&self, path: &RepoPath, moniker: &str) -> Option<SymbolId> {
        self.lookup
            .get(&SymbolKey {
                scope: Some(path.clone()),
                moniker: moniker.to_owned(),
            })
            .copied()
    }

    /// Definition occurrences of a symbol (the `Definition` role bit), in path/position order.
    pub fn definitions(&self, id: SymbolId) -> Vec<Located<'_>> {
        self.occurrences_where(id, |o| o.roles.is_definition())
    }

    /// Reference occurrences of a symbol: every occurrence that is not a
    /// definition. Forward declarations are included.
    pub fn references(&self, id: SymbolId) -> Vec<Located<'_>> {
        self.occurrences_where(id, |o| !o.roles.is_definition())
    }

    /// All occurrences of a symbol, definitions included.
    pub fn occurrences(&self, id: SymbolId) -> Vec<Located<'_>> {
        self.occurrences_where(id, |_| true)
    }

    /// Symbols that declare an implementation relationship to `id`
    /// (for example the impls of a trait or the overrides of a method), sorted by id.
    pub fn implementing_symbols(&self, id: SymbolId) -> Vec<SymbolId> {
        let Some(sym) = self.symbol(id) else {
            return Vec::new();
        };
        if sym.is_local() {
            return Vec::new();
        }
        self.implementers
            .get(&sym.scip_symbol)
            .cloned()
            .unwrap_or_default()
    }

    /// Definition occurrences of every symbol that implements `id`.
    pub fn implementations(&self, id: SymbolId) -> Vec<Located<'_>> {
        let mut out = Vec::new();
        for imp in self.implementing_symbols(id) {
            out.extend(self.definitions(imp));
        }
        out.sort_by(|a, b| (a.path, a.occurrence.span).cmp(&(b.path, b.occurrence.span)));
        out
    }

    /// The symbol occurrence at a 0-based position, using the document's own
    /// character encoding. When occurrences nest, the innermost wins; ties go
    /// to the definition, then to the earlier span. Returns `None` when
    /// nothing covers the position or the document is not indexed.
    pub fn symbol_at(&self, path: &RepoPath, line: u32, character: u32) -> Option<Located<'_>> {
        let doc = self.document(path)?;
        let best = doc
            .occurrences
            .iter()
            .filter(|o| o.span.contains(line, character))
            .min_by_key(|o| (o.span.extent(), !o.roles.is_definition()))?;
        Some(Located {
            path: &doc.path,
            occurrence: best,
        })
    }

    /// How the index relates to a file's current content.
    ///
    /// Contract: SCIP documents may embed their text; the reader keeps only its
    /// BLAKE3 hash. When the indexer did not embed text, supply the hashes the
    /// index was built from with [`ScipIndex::apply_manifest`]. A document
    /// without any recorded hash is [`Freshness::Unknown`], never `Fresh`.
    pub fn coverage(&self, path: &RepoPath, current: &ContentHash) -> Freshness {
        match self.document(path) {
            None => Freshness::NotIndexed,
            Some(doc) => match &doc.content_hash {
                None => Freshness::Unknown,
                Some(h) if h == current => Freshness::Fresh,
                Some(_) => Freshness::Stale,
            },
        }
    }

    /// `true` only when the index provably describes `current` for `path`
    /// ([`Freshness::Fresh`]). Callers drop occurrences of files for which
    /// this is `false`.
    pub fn covers(&self, path: &RepoPath, current: &ContentHash) -> bool {
        self.coverage(path, current) == Freshness::Fresh
    }

    /// Supplies content hashes for documents that have none (indexers that do
    /// not embed text). Hashes recorded from embedded text are never replaced.
    /// Returns how many documents were updated.
    pub fn apply_manifest(
        &mut self,
        manifest: impl IntoIterator<Item = (RepoPath, ContentHash)>,
    ) -> usize {
        let mut updated = 0;
        for (path, hash) in manifest {
            let Some(&i) = self.doc_by_path.get(&path) else {
                continue;
            };
            if let Some(doc) = self.documents.get_mut(i)
                && doc.content_hash.is_none()
            {
                doc.content_hash = Some(hash);
                updated += 1;
            }
        }
        updated
    }

    /// Removes the documents (and their occurrences) for which `keep` returns
    /// `false`, then rebuilds the lookup tables. Symbols stay known. Used to
    /// drop stale files after [`ScipIndex::coverage`] checks.
    pub fn retain_documents(&mut self, mut keep: impl FnMut(&Document) -> bool) {
        self.documents.retain(|d| keep(d));
        self.rebuild_tables();
    }

    fn rebuild_tables(&mut self) {
        self.doc_by_path = self
            .documents
            .iter()
            .enumerate()
            .map(|(i, d)| (d.path.clone(), i))
            .collect();
        let mut by_symbol: Vec<Vec<(usize, usize)>> = vec![Vec::new(); self.symbols.len()];
        for (di, doc) in self.documents.iter().enumerate() {
            for (oi, occ) in doc.occurrences.iter().enumerate() {
                if let Some(list) = by_symbol.get_mut(occ.symbol.0) {
                    list.push((di, oi));
                }
            }
        }
        self.by_symbol = by_symbol;
    }

    fn occurrences_where(
        &self,
        id: SymbolId,
        pred: impl Fn(&Occurrence) -> bool,
    ) -> Vec<Located<'_>> {
        let Some(list) = self.by_symbol.get(id.0) else {
            return Vec::new();
        };
        list.iter()
            .filter_map(|&(di, oi)| {
                let doc = self.documents.get(di)?;
                let occurrence = doc.occurrences.get(oi)?;
                pred(occurrence).then_some(Located {
                    path: &doc.path,
                    occurrence,
                })
            })
            .collect()
    }
}

struct Builder<'a> {
    limits: &'a Limits,
    symbols: Vec<PreciseSymbol>,
    lookup: HashMap<SymbolKey, SymbolId>,
    docs: BTreeMap<RepoPath, Document>,
    report: IngestReport,
}

impl Builder<'_> {
    fn reject_document(&mut self, path: &str, reason: String) {
        self.report.rejected_document_count += 1;
        if self.report.rejected_documents.len() < 100 {
            let mut p = path.to_owned();
            truncate_to_boundary(&mut p, 256);
            self.report
                .rejected_documents
                .push(RejectedDocument { path: p, reason });
        }
    }

    fn intern(
        &mut self,
        moniker: &str,
        scope: Option<&RepoPath>,
    ) -> Result<Option<SymbolId>, ScipError> {
        if moniker.is_empty() || moniker.len() > MAX_SYMBOL_LEN {
            return Ok(None);
        }
        let is_local = moniker.starts_with("local ");
        let scope = if is_local { scope } else { None };
        if is_local && scope.is_none() {
            // A local symbol outside any document has no meaning.
            return Ok(None);
        }
        let key = SymbolKey {
            scope: scope.cloned(),
            moniker: moniker.to_owned(),
        };
        if let Some(&id) = self.lookup.get(&key) {
            return Ok(Some(id));
        }
        if self.symbols.len() >= self.limits.max_symbols {
            return Err(ScipError::LimitExceeded {
                what: "symbols",
                limit: self.limits.max_symbols,
            });
        }
        let id = SymbolId(self.symbols.len());
        self.symbols.push(PreciseSymbol {
            scip_symbol: moniker.to_owned(),
            scope: key.scope.clone(),
            display_name: String::new(),
            kind: SymbolKind::Unspecified,
            documentation: String::new(),
            relationships: Vec::new(),
        });
        self.lookup.insert(key, id);
        Ok(Some(id))
    }

    fn ingest_symbol_info(
        &mut self,
        info: &pb::SymbolInformation,
        scope: Option<&RepoPath>,
    ) -> Result<(), ScipError> {
        let Some(id) = self.intern(&info.symbol, scope)? else {
            self.report.invalid_symbols += 1;
            return Ok(());
        };
        let max_doc = self.limits.max_documentation_bytes;
        let max_rel = self.limits.max_relationships_per_symbol;
        let Some(sym) = self.symbols.get_mut(id.0) else {
            return Ok(());
        };
        if sym.display_name.is_empty() {
            sym.display_name = info.display_name.clone();
        }
        if sym.kind == SymbolKind::Unspecified {
            sym.kind = map_kind(info.kind.enum_value_or_default());
        }
        if sym.documentation.is_empty() && !info.documentation.is_empty() {
            let mut text = info.documentation.join("\n\n");
            truncate_to_boundary(&mut text, max_doc);
            sym.documentation = text;
        }
        for r in &info.relationships {
            if sym.relationships.len() >= max_rel {
                break;
            }
            if r.symbol.is_empty() || r.symbol.len() > MAX_SYMBOL_LEN {
                continue;
            }
            let rel = Relationship {
                symbol: r.symbol.clone(),
                is_reference: r.is_reference,
                is_implementation: r.is_implementation,
                is_type_definition: r.is_type_definition,
                is_definition: r.is_definition,
            };
            if !sym.relationships.contains(&rel) {
                sym.relationships.push(rel);
            }
        }
        Ok(())
    }

    fn ingest_document(&mut self, raw: &pb::Document) -> Result<(), ScipError> {
        let path = match RepoPath::new(raw.relative_path.clone()) {
            Ok(p) => p,
            Err(e) => {
                self.reject_document(&raw.relative_path, e.to_string());
                return Ok(());
            }
        };
        let encoding = match raw.position_encoding.enum_value_or_default() {
            pb::PositionEncoding::UTF8CodeUnitOffsetFromLineStart => PositionEncoding::Utf8,
            pb::PositionEncoding::UTF16CodeUnitOffsetFromLineStart => PositionEncoding::Utf16,
            pb::PositionEncoding::UTF32CodeUnitOffsetFromLineStart => PositionEncoding::Utf32,
            pb::PositionEncoding::UnspecifiedPositionEncoding => PositionEncoding::Unspecified,
        };
        let hash = (!raw.text.is_empty()).then(|| ContentHash::of(raw.text.as_bytes()));

        for info in &raw.symbols {
            self.ingest_symbol_info(info, Some(&path))?;
        }

        let mut occurrences = Vec::with_capacity(raw.occurrences.len());
        for o in &raw.occurrences {
            let Some(id) = self.intern(&o.symbol, Some(&path))? else {
                self.report.invalid_occurrences += 1;
                continue;
            };
            let span = match Span::from_scip(&o.range) {
                Ok(s) => s,
                Err(_) => {
                    self.report.invalid_occurrences += 1;
                    continue;
                }
            };
            let Some(line_range) = span.line_range() else {
                self.report.invalid_occurrences += 1;
                continue;
            };
            let (enclosing_span, enclosing_range) = if o.enclosing_range.is_empty() {
                (None, None)
            } else {
                match Span::from_scip(&o.enclosing_range) {
                    Ok(s) => (Some(s), s.line_range()),
                    Err(_) => {
                        self.report.invalid_enclosing_ranges += 1;
                        (None, None)
                    }
                }
            };
            let roles = SymbolRoles(o.symbol_roles);
            occurrences.push(Occurrence {
                symbol: id,
                span,
                line_range,
                roles,
                role: roles.primary(),
                enclosing_span,
                enclosing_range,
            });
        }

        // The same path can appear twice (indexers that emit per-package
        // documents); merge instead of letting one overwrite the other.
        match self.docs.get_mut(&path) {
            Some(existing) => {
                existing.occurrences.extend(occurrences);
                if existing.content_hash.is_none() {
                    existing.content_hash = hash;
                }
            }
            None => {
                self.docs.insert(
                    path.clone(),
                    Document {
                        path,
                        language: raw.language.clone(),
                        position_encoding: encoding,
                        content_hash: hash,
                        occurrences,
                    },
                );
            }
        }
        Ok(())
    }

    fn finish(self, metadata: IndexMetadata) -> Result<ScipIndex, ScipError> {
        let Builder {
            symbols,
            lookup,
            docs,
            report,
            ..
        } = self;
        let mut documents: Vec<Document> = docs.into_values().collect();
        for d in &mut documents {
            d.occurrences.sort_by_key(|o| (o.span, o.symbol, o.roles.0));
            d.occurrences.dedup();
        }
        let mut implementers: HashMap<String, Vec<SymbolId>> = HashMap::new();
        for (i, s) in symbols.iter().enumerate() {
            if s.is_local() {
                continue;
            }
            for r in s.relationships.iter().filter(|r| r.is_implementation) {
                if r.symbol.starts_with("local ") {
                    continue;
                }
                implementers
                    .entry(r.symbol.clone())
                    .or_default()
                    .push(SymbolId(i));
            }
        }
        for list in implementers.values_mut() {
            list.sort();
            list.dedup();
        }

        let mut index = ScipIndex {
            metadata,
            source_revision: None,
            documents,
            doc_by_path: HashMap::new(),
            symbols,
            lookup,
            by_symbol: Vec::new(),
            implementers,
            report,
        };
        index.fill_missing_display_names();
        index.rebuild_tables();
        Ok(index)
    }
}

impl ScipIndex {
    fn fill_missing_display_names(&mut self) {
        for s in &mut self.symbols {
            if s.display_name.is_empty() {
                s.display_name = ScipSymbol::parse(&s.scip_symbol)
                    .ok()
                    .and_then(|p| p.short_name().map(str::to_owned))
                    .unwrap_or_else(|| s.scip_symbol.clone());
            }
        }
    }
}

fn truncate_to_boundary(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

fn map_kind(kind: pb::symbol_information::Kind) -> SymbolKind {
    use pb::symbol_information::Kind as K;
    match kind {
        K::UnspecifiedKind => SymbolKind::Unspecified,
        K::Class | K::SingletonClass | K::Object | K::Extension | K::Mixin => SymbolKind::Class,
        K::Struct | K::Union => SymbolKind::Struct,
        K::Interface | K::Protocol | K::Trait | K::TypeClass | K::Concept | K::Contract => {
            SymbolKind::Interface
        }
        K::Enum => SymbolKind::Enum,
        K::EnumMember => SymbolKind::EnumMember,
        K::Function => SymbolKind::Function,
        K::Method
        | K::AbstractMethod
        | K::Accessor
        | K::Getter
        | K::Setter
        | K::MethodAlias
        | K::MethodSpecification
        | K::ProtocolMethod
        | K::PureVirtualMethod
        | K::SingletonMethod
        | K::StaticMethod
        | K::TraitMethod
        | K::TypeClassMethod
        | K::Delegate => SymbolKind::Method,
        K::Constructor => SymbolKind::Constructor,
        K::Field
        | K::Property
        | K::Attribute
        | K::StaticDataMember
        | K::StaticField
        | K::StaticProperty
        | K::Key => SymbolKind::Field,
        K::Variable | K::StaticVariable | K::Value => SymbolKind::Variable,
        K::Constant => SymbolKind::Constant,
        K::Parameter
        | K::SelfParameter
        | K::ThisParameter
        | K::ParameterLabel
        | K::MethodReceiver => SymbolKind::Parameter,
        K::Type | K::TypeAlias | K::AssociatedType | K::TypeFamily | K::DataFamily => {
            SymbolKind::Type
        }
        K::TypeParameter => SymbolKind::TypeParameter,
        K::Module | K::Namespace | K::Package | K::PackageObject | K::File | K::Library => {
            SymbolKind::Module
        }
        K::Macro | K::Quasiquoter => SymbolKind::Macro,
        _ => SymbolKind::Other,
    }
}
