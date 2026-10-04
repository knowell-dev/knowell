//! Explicit, version-bound import of already produced SCIP indexes.
//!
//! Importing does not run a compiler or an indexer. The caller supplies the
//! artifact's build manifest; verified occurrences are written only while the
//! matching view generation is building. SCIP documentation and source text are
//! discarded before durable staging, and compiler references become calls only
//! when their exact byte span is a callee in the stored source's syntax tree.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use knowell_core::{ContentHash, LineRange, RepoPath};
use knowell_embed::Embedder;
use knowell_parse::{ParsedFile, SymbolKind, parse_with};
use knowell_scip::{PositionEncoding, ScipIndex, Span};
use knowell_store::content;
use knowell_store::graph::{self, NewEdge, NodeRef};
use knowell_store::symbols::{self, Definition, NewOccurrence};
use knowell_store::views::{self, GenerationPin};
use knowell_store::{
    EvidenceType, GenerationState, OccurrenceRole, PgConnection, ProjectId, Resolution, SymbolId,
};
use serde::{Deserialize, Serialize};

use crate::context::ViewContext;
use crate::error::IndexError;
use crate::indexer::{Indexer, Inner};
use crate::references::identifier_scan;
use crate::split_symbol_key;

const FORMAT_VERSION: u32 = 1;
const MAX_IDENTITY_BYTES: usize = 128;

/// Caller-attested inputs captured when the external indexer produced its file.
///
/// Paths are project-relative. Document hashes describe the original bytes,
/// not the redacted representation. `build_inputs` includes all indexed
/// documents and the source, manifest and lockfile inputs represented by this
/// project; `compiler_identity` binds compiler/indexer options and other inputs
/// outside that set. It is a digest, never raw environment values or arguments.
/// This attestation is not proof that an external compiler covered every path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScipImportManifest {
    /// Full lowercase hexadecimal Git commit, 40 or 64 digits.
    pub source_revision: String,
    /// BLAKE3 hash of the encoded SCIP file before decoding.
    pub artifact_hash: ContentHash,
    /// External indexer's bounded name; must match SCIP metadata.
    pub tool_name: String,
    /// External indexer's bounded version; must match SCIP metadata.
    pub tool_version: String,
    /// Digest of compiler, options and dependency inputs captured at index time.
    pub compiler_identity: ContentHash,
    /// Build-time original-content hashes, including every indexed document.
    pub build_inputs: BTreeMap<RepoPath, ContentHash>,
    /// Build-time original-content hashes of the SCIP documents.
    pub documents: BTreeMap<RepoPath, ContentHash>,
}

/// Resource bounds for a single explicit import, in bytes and record counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScipImportLimits {
    /// Maximum encoded SCIP bytes and serialized prepared JSON bytes.
    pub max_bytes: usize,
    /// Maximum indexed documents and build-input entries.
    pub max_documents: usize,
    /// Maximum total occurrences.
    pub max_occurrences: usize,
    /// Maximum symbols.
    pub max_symbols: usize,
    /// Maximum relationships per symbol.
    pub max_relationships_per_symbol: usize,
    /// Maximum aggregate stored source bytes loaded to verify syntax.
    pub max_source_bytes: usize,
    /// Maximum generated precise edges; exceeding this fails atomically.
    pub max_edges: usize,
}

impl Default for ScipImportLimits {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024 * 1024,
            max_documents: 100_000,
            max_occurrences: 2_000_000,
            max_symbols: 500_000,
            max_relationships_per_symbol: 256,
            max_source_bytes: 128 * 1024 * 1024,
            max_edges: 1_000_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Encoding {
    Unknown,
    Utf8,
    Utf16,
    Utf32,
}

impl From<PositionEncoding> for Encoding {
    fn from(value: PositionEncoding) -> Self {
        match value {
            PositionEncoding::Unspecified => Self::Unknown,
            PositionEncoding::Utf8 => Self::Utf8,
            PositionEncoding::Utf16 => Self::Utf16,
            PositionEncoding::Utf32 => Self::Utf32,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PositionSpan {
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
}

impl From<Span> for PositionSpan {
    fn from(value: Span) -> Self {
        Self {
            start_line: value.start_line,
            start_character: value.start_character,
            end_line: value.end_line,
            end_character: value.end_character,
        }
    }
}

impl PositionSpan {
    fn valid(self) -> bool {
        (self.start_line, self.start_character) <= (self.end_line, self.end_character)
            && self.end_line < u32::MAX
    }

    fn lines(self) -> Option<LineRange> {
        if !self.valid() {
            return None;
        }
        // An exclusive end at column zero does not include the following line.
        let start = self.start_line.checked_add(1)?;
        let end = if self.end_line > self.start_line && self.end_character == 0 {
            self.end_line
        } else {
            self.end_line.checked_add(1)?
        };
        LineRange::new(start, end).ok()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedOccurrence {
    symbol: usize,
    span: PositionSpan,
    definition: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedDocument {
    path: RepoPath,
    content_hash: ContentHash,
    encoding: Encoding,
    occurrences: Vec<PreparedOccurrence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedSymbol {
    callable: bool,
    implementations: Vec<usize>,
}

/// Sanitized durable import payload; contains no SCIP source, docs or monikers.
///
/// Use [`Self::from_bytes`] at the explicit import boundary and
/// [`Self::from_json`] when resuming a staged build. Private fields prevent
/// callers from bypassing the build-manifest and structural checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedScipImport {
    format_version: u32,
    manifest: ScipImportManifest,
    documents: Vec<PreparedDocument>,
    symbols: Vec<PreparedSymbol>,
}

impl PreparedScipImport {
    /// Decodes bounded untrusted SCIP bytes, verifies their build attestation,
    /// and removes source text, documentation and raw symbol names. Repeated
    /// project-relative document paths are rejected, including identical copies:
    /// each occurrence must have one source hash and one position encoding.
    pub fn from_bytes(
        bytes: &[u8],
        manifest: ScipImportManifest,
        limits: &ScipImportLimits,
    ) -> Result<Self, IndexError> {
        validate_manifest(&manifest, limits)?;
        if bytes.len() > limits.max_bytes {
            return Err(invalid("encoded artifact exceeds the byte limit"));
        }
        if ContentHash::of(bytes) != manifest.artifact_hash {
            return Err(invalid("artifact hash does not match the build manifest"));
        }
        let encoded_documents = encoded_document_count(bytes, limits.max_documents)?;
        let reader_limits = knowell_scip::Limits {
            max_bytes: u64::try_from(limits.max_bytes).unwrap_or(u64::MAX),
            max_documents: limits.max_documents,
            max_occurrences: limits.max_occurrences,
            max_symbols: limits.max_symbols,
            max_documentation_bytes: 0,
            // One extra unique relationship is enough to reject an oversize
            // symbol, while bounding the reader's linear deduplication scans.
            max_relationships_per_symbol: limits.max_relationships_per_symbol.saturating_add(1),
        };
        let mut index = ScipIndex::read_bytes(bytes, &reader_limits)
            .map_err(|_| invalid("artifact is malformed or exceeds an ingestion limit"))?;
        let report = index.report();
        if report.rejected_document_count != 0
            || report.invalid_occurrences != 0
            || report.invalid_enclosing_ranges != 0
            || report.invalid_symbols != 0
        {
            return Err(invalid(
                "artifact contains rejected documents or malformed records",
            ));
        }
        if index.metadata().tool_name != manifest.tool_name
            || index.metadata().tool_version != manifest.tool_version
        {
            return Err(invalid(
                "indexer identity does not match the build manifest",
            ));
        }
        if encoded_documents != index.documents().len() {
            // The general reader merges repeated paths, retaining the first
            // hash and encoding. That cannot authenticate later occurrences.
            return Err(invalid("artifact contains repeated document paths"));
        }
        if index.documents().len() != manifest.documents.len() {
            return Err(invalid("document set does not match the build manifest"));
        }
        if index
            .symbols()
            .iter()
            .any(|symbol| symbol.relationships.len() > limits.max_relationships_per_symbol)
        {
            return Err(invalid(
                "artifact exceeds the relationships-per-symbol limit",
            ));
        }
        index.apply_manifest(manifest.documents.clone());
        let mut documents = Vec::with_capacity(index.documents().len());
        for document in index.documents() {
            let expected = manifest
                .documents
                .get(&document.path)
                .ok_or_else(|| invalid("document is absent from the build manifest"))?;
            if document.content_hash != Some(*expected) {
                return Err(invalid("embedded source differs from its build-time hash"));
            }
            documents.push(PreparedDocument {
                path: document.path.clone(),
                content_hash: *expected,
                encoding: document.position_encoding.into(),
                occurrences: document
                    .occurrences
                    .iter()
                    .map(|occurrence| PreparedOccurrence {
                        symbol: occurrence.symbol.index(),
                        span: occurrence.span.into(),
                        definition: occurrence.roles.is_definition(),
                    })
                    .collect(),
            });
        }
        let symbols = index
            .symbols()
            .iter()
            .map(|symbol| PreparedSymbol {
                callable: matches!(
                    symbol.kind,
                    knowell_scip::SymbolKind::Function
                        | knowell_scip::SymbolKind::Method
                        | knowell_scip::SymbolKind::Constructor
                ),
                implementations: symbol
                    .relationships
                    .iter()
                    .filter(|relationship| relationship.is_implementation)
                    .filter_map(|relationship| index.find_symbol(&relationship.symbol))
                    .map(knowell_scip::SymbolId::index)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
            })
            .collect();
        let prepared = Self {
            format_version: FORMAT_VERSION,
            manifest,
            documents,
            symbols,
        };
        prepared.validate(limits)?;
        prepared.to_json(limits)?;
        Ok(prepared)
    }

    /// The verified build attestation, suitable for scoped staging metadata.
    pub fn manifest(&self) -> &ScipImportManifest {
        &self.manifest
    }

    /// Deterministic digest binding indexer/compiler identity and source inputs.
    pub fn analysis_input_hash(&self) -> Result<ContentHash, IndexError> {
        let bytes = serde_json::to_vec(&self.manifest)
            .map_err(|_| invalid("cannot serialize the build manifest"))?;
        Ok(ContentHash::of_parts([
            b"knowell-scip-input-v1".as_slice(),
            &bytes,
        ]))
    }

    /// Serializes the sanitized payload, enforcing the durable byte bound.
    pub fn to_json(&self, limits: &ScipImportLimits) -> Result<Vec<u8>, IndexError> {
        self.validate(limits)?;
        let bytes = serde_json::to_vec(self)
            .map_err(|_| invalid("cannot serialize the prepared artifact"))?;
        if bytes.len() > limits.max_bytes {
            return Err(invalid("prepared artifact exceeds the byte limit"));
        }
        Ok(bytes)
    }

    /// Reads and validates a previously staged bounded payload. Error text never
    /// echoes rejected JSON, paths, source or indexer metadata.
    pub fn from_json(bytes: &[u8], limits: &ScipImportLimits) -> Result<Self, IndexError> {
        if bytes.len() > limits.max_bytes {
            return Err(invalid("prepared artifact exceeds the byte limit"));
        }
        let prepared: Self =
            serde_json::from_slice(bytes).map_err(|_| invalid("prepared artifact is malformed"))?;
        prepared.validate(limits)?;
        Ok(prepared)
    }

    fn validate(&self, limits: &ScipImportLimits) -> Result<(), IndexError> {
        validate_manifest(&self.manifest, limits)?;
        if self.format_version != FORMAT_VERSION {
            return Err(invalid("prepared artifact format is unsupported"));
        }
        if self.documents.len() > limits.max_documents || self.symbols.len() > limits.max_symbols {
            return Err(invalid("prepared artifact exceeds a record count limit"));
        }
        if self.documents.len() != self.manifest.documents.len() {
            return Err(invalid(
                "prepared documents do not match the build manifest",
            ));
        }
        let mut paths = BTreeSet::new();
        let mut occurrences = 0usize;
        for document in &self.documents {
            if !paths.insert(&document.path)
                || self.manifest.documents.get(&document.path) != Some(&document.content_hash)
            {
                return Err(invalid("prepared document identity is inconsistent"));
            }
            occurrences = occurrences.saturating_add(document.occurrences.len());
            if occurrences > limits.max_occurrences {
                return Err(invalid("prepared artifact exceeds the occurrence limit"));
            }
            if document.occurrences.iter().any(|occurrence| {
                occurrence.symbol >= self.symbols.len() || !occurrence.span.valid()
            }) {
                return Err(invalid("prepared occurrence is invalid"));
            }
        }
        for symbol in &self.symbols {
            if symbol.implementations.len() > limits.max_relationships_per_symbol
                || symbol
                    .implementations
                    .iter()
                    .any(|target| *target >= self.symbols.len())
            {
                return Err(invalid("prepared symbol relationship is invalid"));
            }
        }
        Ok(())
    }
}

/// Observable omissions and rows from an explicit import. None of these counts
/// implies exhaustive compiler coverage or runtime-call coverage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScipImportReport {
    /// Source documents admitted by policy and verified against the generation.
    pub documents_verified: usize,
    /// Documents excluded by the registered project's source policy.
    pub documents_excluded: usize,
    /// Documents whose unavailable/redacted source cannot verify positions.
    pub documents_without_exact_source: usize,
    /// Definition monikers mapped unambiguously to indexed source symbols.
    pub symbols_mapped: usize,
    /// Occurrences whose target has no unique local indexed definition.
    pub occurrences_unmapped: usize,
    /// Occurrences with coordinates that do not name valid source bytes.
    pub occurrences_invalid_position: usize,
    /// Precise reference observations written.
    pub references_written: usize,
    /// Static call-site observations written, combining SCIP with AST syntax.
    pub calls_written: usize,
    /// Implementation relationships written between known local definitions.
    pub implementations_written: usize,
}

fn invalid(reason: &'static str) -> IndexError {
    IndexError::invalid("scip import", reason)
}

// Count Index.documents (field 2) before the general reader merges paths.
// Only top-level framing is inspected; document contents remain the reader's
// responsibility. Reject groups, which the SCIP schema does not use, rather
// than introducing a second recursive protobuf decoder at this boundary.
fn encoded_document_count(mut bytes: &[u8], maximum: usize) -> Result<usize, IndexError> {
    let malformed = || invalid("artifact wire structure is malformed or unsupported");
    let mut count = 0usize;
    while !bytes.is_empty() {
        let tag = wire_varint(&mut bytes).ok_or_else(malformed)?;
        let field = tag >> 3;
        let wire = tag & 7;
        if field == 0 || field > 0x1fff_ffff || (field == 2 && wire != 2) {
            return Err(malformed());
        }
        let length = match wire {
            0 => {
                wire_varint(&mut bytes).ok_or_else(malformed)?;
                0
            }
            1 => 8,
            2 => usize::try_from(wire_varint(&mut bytes).ok_or_else(malformed)?)
                .map_err(|_| malformed())?,
            5 => 4,
            _ => return Err(malformed()),
        };
        bytes = bytes.get(length..).ok_or_else(malformed)?;
        if field == 2 {
            count = count.checked_add(1).ok_or_else(malformed)?;
            if count > maximum {
                return Err(invalid("artifact exceeds the document count limit"));
            }
        }
    }
    Ok(count)
}

fn wire_varint(bytes: &mut &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let (&byte, remaining) = bytes.split_first()?;
        *bytes = remaining;
        if shift == 63 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn validate_manifest(
    manifest: &ScipImportManifest,
    limits: &ScipImportLimits,
) -> Result<(), IndexError> {
    if !matches!(manifest.source_revision.len(), 40 | 64)
        || !manifest
            .source_revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(
            "source revision must be a full lowercase git commit",
        ));
    }
    for identity in [&manifest.tool_name, &manifest.tool_version] {
        if identity.is_empty()
            || identity.len() > MAX_IDENTITY_BYTES
            || !identity
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.+/ ()".contains(&byte))
            || !knowell_secrets::scan(identity).is_empty()
        {
            return Err(invalid(
                "indexer identity must be bounded non-sensitive text",
            ));
        }
    }
    if manifest.documents.is_empty()
        || manifest.documents.len() > limits.max_documents
        || manifest.build_inputs.len() > limits.max_documents
    {
        return Err(invalid(
            "build manifest exceeds its document count limit or is empty",
        ));
    }
    for (path, hash) in &manifest.documents {
        if manifest.build_inputs.get(path) != Some(hash) {
            return Err(invalid("every document must be included in build inputs"));
        }
    }
    Ok(())
}

/// Converts a position without splitting UTF-8 or UTF-16 characters. Unknown
/// position encodings are refused even when the visible source happens to be ASCII.
#[cfg(test)]
fn byte_position(text: &str, line: u32, character: u32, encoding: Encoding) -> Option<usize> {
    byte_position_in_source(text, &line_offsets(text), line, character, encoding)
}

fn line_offsets(text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(
            text.match_indices('\n')
                .map(|(byte, _)| byte.saturating_add(1)),
        )
        .collect()
}

fn byte_position_in_source(
    text: &str,
    offsets: &[usize],
    line: u32,
    character: u32,
    encoding: Encoding,
) -> Option<usize> {
    if encoding == Encoding::Unknown {
        return None;
    }
    let row = usize::try_from(line).ok()?;
    let start = *offsets.get(row)?;
    let end = offsets
        .get(row.checked_add(1)?)
        .copied()
        .unwrap_or(text.len());
    let part = text.get(start..end)?;
    let body = part.strip_suffix('\n').unwrap_or(part);
    character_offset(body, character, encoding).and_then(|col| start.checked_add(col))
}

fn character_offset(line: &str, character: u32, encoding: Encoding) -> Option<usize> {
    let requested = usize::try_from(character).ok()?;
    if encoding == Encoding::Utf8 {
        return line.is_char_boundary(requested).then_some(requested);
    }
    let mut units = 0usize;
    for (byte, ch) in line.char_indices() {
        if units == requested {
            return Some(byte);
        }
        units = units.checked_add(if encoding == Encoding::Utf16 {
            ch.len_utf16()
        } else {
            1
        })?;
        if units > requested {
            return None;
        }
    }
    (units == requested).then_some(line.len())
}

#[cfg(test)]
fn byte_span(text: &str, span: PositionSpan, encoding: Encoding) -> Option<Range<usize>> {
    byte_span_in_source(text, &line_offsets(text), span, encoding)
}

fn byte_span_in_source(
    text: &str,
    offsets: &[usize],
    span: PositionSpan,
    encoding: Encoding,
) -> Option<Range<usize>> {
    if !span.valid() {
        return None;
    }
    let start = byte_position_in_source(
        text,
        offsets,
        span.start_line,
        span.start_character,
        encoding,
    )?;
    let end = byte_position_in_source(text, offsets, span.end_line, span.end_character, encoding)?;
    (start < end).then_some(start..end)
}

// Declaration lookup uses a one-based name line and the exact source spelling.
type DeclarationKey = (u32, String);
// Regions are half-open UTF-8 byte offsets in the verified stored source.
type SymbolByteRegion = (Range<usize>, SymbolId);

struct VerifiedDocument {
    text: String,
    line_offsets: Vec<usize>,
    declarations: BTreeMap<DeclarationKey, Vec<SymbolByteRegion>>,
    owners: Vec<SymbolByteRegion>,
    call_spans: BTreeSet<(usize, usize)>,
    syntax_truncated: bool,
    syntax_unavailable: bool,
}

impl VerifiedDocument {
    fn new(
        text: String,
        parsed: ParsedFile,
        definitions: Vec<Definition>,
        call_spans: BTreeSet<(usize, usize)>,
    ) -> Self {
        let path = &parsed.path;
        let hash = ContentHash::of(text.as_bytes());
        let mut counts: BTreeMap<(&str, &str), usize> = BTreeMap::new();
        for symbol in &parsed.symbols {
            *counts
                .entry((symbol.kind.as_str(), &symbol.qualified_name))
                .or_default() += 1;
        }
        let stored: BTreeMap<_, _> = definitions
            .iter()
            .filter_map(|definition| {
                let (definition_path, local) = split_symbol_key(&definition.symbol.qualified_name)?;
                (&definition_path == path && definition.content_hash == hash).then_some((
                    (definition.symbol.kind.as_str(), local),
                    definition.symbol.id,
                ))
            })
            .collect();
        let mut declarations: BTreeMap<_, Vec<_>> = BTreeMap::new();
        let mut events: BTreeMap<usize, Vec<(bool, usize, SymbolId)>> = BTreeMap::new();
        for symbol in &parsed.symbols {
            let key = (symbol.kind.as_str(), symbol.qualified_name.as_str());
            // Syntactic keys can collapse overloads or multiple trait impls.
            // Do not promote either declaration into a compiler identity.
            if counts.get(&key) != Some(&1) {
                continue;
            }
            let Some(id) = stored.get(&key) else {
                continue;
            };
            declarations
                .entry((symbol.name_line, symbol.name.clone()))
                .or_default()
                .push((symbol.byte_range.clone(), *id));
            if matches!(
                symbol.kind,
                SymbolKind::Function | SymbolKind::Method | SymbolKind::Constructor
            ) && !symbol.byte_range.is_empty()
            {
                let size = symbol.byte_range.len();
                events
                    .entry(symbol.byte_range.start)
                    .or_default()
                    .push((true, size, *id));
                events
                    .entry(symbol.byte_range.end)
                    .or_default()
                    .push((false, size, *id));
            }
        }
        let mut owners = Vec::new();
        let mut active = BTreeSet::new();
        let mut previous: Option<(usize, Option<SymbolId>)> = None;
        for (position, changes) in events {
            if let Some((start, Some(id))) = previous
                && start < position
            {
                owners.push((start..position, id));
            }
            for (added, size, id) in changes {
                if added {
                    active.insert((size, id));
                } else {
                    active.remove(&(size, id));
                }
            }
            let mut candidates = active.iter();
            let best = candidates.next().copied();
            let owner = best.and_then(|(size, id)| {
                (!candidates
                    .next()
                    .is_some_and(|(other_size, _)| *other_size == size))
                .then_some(id)
            });
            previous = Some((position, owner));
        }
        Self {
            line_offsets: line_offsets(&text),
            text,
            declarations,
            owners,
            call_spans,
            syntax_truncated: false,
            syntax_unavailable: false,
        }
    }
}

fn definition_at(
    verified: &VerifiedDocument,
    _path: &RepoPath,
    bytes: &Range<usize>,
    line: u32,
) -> Option<SymbolId> {
    let token = verified.text.get(bytes.clone())?;
    let candidates: BTreeSet<_> = verified
        .declarations
        .get(&(line, token.to_owned()))?
        .iter()
        .filter_map(|(range, id)| {
            (range.start <= bytes.start && bytes.end <= range.end).then_some(*id)
        })
        .collect();
    (candidates.len() == 1)
        .then(|| candidates.first().copied())
        .flatten()
}

fn owner_at(
    verified: &VerifiedDocument,
    _path: &RepoPath,
    bytes: &Range<usize>,
) -> Option<SymbolId> {
    let next = verified
        .owners
        .partition_point(|(range, _)| range.start <= bytes.start);
    let (range, id) = verified.owners.get(next.checked_sub(1)?)?;
    (range.start <= bytes.start && bytes.end <= range.end).then_some(*id)
}

impl<E: Embedder + 'static> Indexer<E> {
    /// Imports verified precise relationships after T1 has written definitions,
    /// and before `pin` is activated. The write is atomic and generation-fenced.
    /// It does not run jobs, activate the view, read the working tree or invoke
    /// an external language tool. Use the staged indexing flow for durable jobs.
    pub async fn import_scip_into_generation(
        &self,
        pin: GenerationPin,
        prepared: &PreparedScipImport,
    ) -> Result<ScipImportReport, IndexError> {
        let ctx = self.inner.context(pin.view)?;
        let _guard = self.inner.view_lock(pin.view).await;
        let mut transaction = self.inner.store.begin().await?;
        let report = self
            .inner
            .apply_scip_import(&mut transaction, &ctx, pin, prepared)
            .await?;
        transaction
            .commit()
            .await
            .map_err(knowell_store::StoreError::from)?;
        Ok(report)
    }
}

impl<E: Embedder + 'static> Inner<E> {
    /// T1 hook used by durable builds; the caller already holds the view lock.
    pub(crate) async fn apply_scip_import(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        pin: GenerationPin,
        prepared: &PreparedScipImport,
    ) -> Result<ScipImportReport, IndexError> {
        let limits = ScipImportLimits::default();
        prepared.validate(&limits)?;
        if pin.view != ctx.view {
            return Err(invalid("import view does not match the registered project"));
        }
        let generation = views::get_generation(conn, pin.view, pin.generation)
            .await?
            .ok_or_else(|| invalid("import generation is missing"))?;
        if generation.state != GenerationState::Building
            || generation.resolved_commit.as_deref()
                != Some(prepared.manifest.source_revision.as_str())
        {
            return Err(invalid(
                "import requires the matching building source revision",
            ));
        }
        let input_paths: Vec<_> = prepared.manifest.build_inputs.keys().cloned().collect();
        if input_paths
            .iter()
            .any(|path| ctx.policy.check(path).is_some())
        {
            return Err(invalid(
                "build inputs include a source excluded by project policy",
            ));
        }
        let metadata =
            content::files_metadata_in_paths(conn, ctx.organization, pin, &input_paths).await?;
        let files: BTreeMap<_, _> = metadata
            .into_iter()
            .map(|file| (file.version.path.clone(), file))
            .collect();
        for (path, hash) in &prepared.manifest.build_inputs {
            if files.get(path).map(|file| file.version.content_hash) != Some(*hash) {
                return Err(invalid(
                    "build input hashes differ from the pinned generation",
                ));
            }
        }
        let mut verified = BTreeMap::new();
        let mut source_bytes = 0usize;
        let mut report = ScipImportReport::default();
        for document in &prepared.documents {
            if ctx.policy.check(&document.path).is_some() {
                report.documents_excluded += 1;
                continue;
            }
            let Some(file) = files.get(&document.path) else {
                return Err(invalid("document is missing from the pinned generation"));
            };
            let max_bytes = usize::try_from(self.config.limits.max_file_bytes)
                .unwrap_or(usize::MAX)
                .min(limits.max_source_bytes.saturating_sub(source_bytes));
            let stored = content::get_content_bounded(
                conn,
                ctx.organization,
                &file.version.content_hash,
                max_bytes,
            )
            .await?;
            let Some(text) = stored.and_then(|blob| blob.redacted_text) else {
                report.documents_without_exact_source += 1;
                continue;
            };
            source_bytes = source_bytes.saturating_add(text.len());
            if ContentHash::of(text.as_bytes()) != document.content_hash {
                // Redaction can move byte columns. Do not interpret old compiler
                // offsets against replacement text or read unredacted content.
                report.documents_without_exact_source += 1;
                continue;
            }
            let parsed = parse_with(&document.path, &text, &self.config.parse_limits, None);
            let scan = identifier_scan(parsed.language, &text, &self.config.parse_limits);
            let syntax_truncated = scan.coverage.truncated;
            let syntax_unavailable = scan.coverage.parse_unavailable || parsed.degraded.is_some();
            let call_spans = scan
                .identifiers
                .into_iter()
                .filter(|identifier| identifier.is_call)
                .map(|identifier| (identifier.start_byte, identifier.end_byte))
                .collect();
            let mut definitions =
                symbols::definitions_in_paths(conn, pin, std::slice::from_ref(&document.path))
                    .await?;
            definitions.retain(|definition| definition.symbol.project == ctx.project);
            let mut source = VerifiedDocument::new(text, parsed, definitions, call_spans);
            source.syntax_truncated = syntax_truncated;
            source.syntax_unavailable = syntax_unavailable;
            verified.insert(document.path.clone(), source);
            report.documents_verified += 1;
        }
        let analysis_hash = prepared.analysis_input_hash()?;
        let (edges, produced_report) = precise_edges(
            ctx.project,
            pin,
            prepared,
            &verified,
            analysis_hash,
            limits.max_edges,
        )?;
        report.symbols_mapped = produced_report.symbols_mapped;
        report.occurrences_unmapped = produced_report.occurrences_unmapped;
        report.occurrences_invalid_position = produced_report.occurrences_invalid_position;
        report.references_written = produced_report.references_written;
        report.calls_written = produced_report.calls_written;
        report.implementations_written = produced_report.implementations_written;
        let origins: Vec<_> = verified.keys().map(|path| format!("scip:{path}")).collect();
        graph::replace_edges(conn, pin.view, pin.generation, &origins, &edges).await?;
        let mut by_origin: BTreeMap<&str, Vec<&NewEdge>> = BTreeMap::new();
        for edge in &edges {
            by_origin.entry(&edge.origin).or_default().push(edge);
        }
        for document in &prepared.documents {
            let origin = format!("scip:{}", document.path);
            let observations = by_origin.get(origin.as_str()).cloned().unwrap_or_default();
            let source = verified.get(&document.path);
            let mut occurrences = Vec::new();
            let mut seen = BTreeSet::new();
            for edge in observations.iter().filter(|edge| edge.kind == "references") {
                let NodeRef::Symbol(symbol) = &edge.to else {
                    continue;
                };
                let Some(lines) = edge
                    .evidence
                    .get("lines")
                    .and_then(serde_json::Value::as_array)
                else {
                    continue;
                };
                let Some((start, end)) = lines
                    .first()
                    .and_then(serde_json::Value::as_u64)
                    .zip(lines.get(1).and_then(serde_json::Value::as_u64))
                else {
                    continue;
                };
                let Some(lines) = u32::try_from(start)
                    .ok()
                    .zip(u32::try_from(end).ok())
                    .and_then(|(start, end)| LineRange::new(start, end).ok())
                else {
                    continue;
                };
                if seen.insert((*symbol, lines.start(), lines.end())) {
                    occurrences.push(NewOccurrence {
                        symbol: *symbol,
                        path: document.path.clone(),
                        content_hash: document.content_hash,
                        lines,
                        role: OccurrenceRole::Reference,
                    });
                }
            }
            knowell_store::analysis::replace_scip_occurrences(
                conn,
                ctx.organization,
                pin,
                &origin,
                &occurrences,
            )
            .await?;
            let details = serde_json::json!({
                "view": pin.view,
                "generation": pin.generation,
                "content_hash": document.content_hash,
                "source_revision": prepared.manifest.source_revision,
                "artifact_hash": prepared.manifest.artifact_hash,
                "analysis_input_hash": analysis_hash,
                "compiler_identity": prepared.manifest.compiler_identity,
                "tool_name": prepared.manifest.tool_name,
                "tool_version": prepared.manifest.tool_version,
                "encoding_unknown": document.encoding == Encoding::Unknown,
                "source_unavailable": source.is_none(),
                "syntax_truncated": source.is_none_or(|source| source.syntax_truncated),
                "syntax_unavailable": source.is_none_or(|source| source.syntax_unavailable),
                "occurrences": document.occurrences.len(),
                "call_sites": source.map(|source| source.call_spans.len()),
                "references_written": observations.iter().filter(|edge| edge.kind == "references").count(),
                "calls_written": observations.iter().filter(|edge| edge.kind == "calls").count(),
                "implementations_written": observations.iter().filter(|edge| edge.kind == "implements").count(),
                "references_complete": false,
                "calls_complete": false,
            });
            knowell_store::analysis::upsert_coverage(
                conn,
                ctx.organization,
                pin,
                &document.path,
                &document.content_hash,
                "scip",
                &details,
            )
            .await?;
        }
        Ok(report)
    }
}

fn precise_edges(
    project: ProjectId,
    pin: GenerationPin,
    prepared: &PreparedScipImport,
    verified: &BTreeMap<RepoPath, VerifiedDocument>,
    analysis_hash: ContentHash,
    max_edges: usize,
) -> Result<(Vec<NewEdge>, ScipImportReport), IndexError> {
    let mut mapping: BTreeMap<usize, BTreeSet<SymbolId>> = BTreeMap::new();
    let mut definition_locations: BTreeMap<usize, Vec<(&PreparedDocument, PositionSpan)>> =
        BTreeMap::new();
    let mut report = ScipImportReport::default();
    for document in &prepared.documents {
        let Some(source) = verified.get(&document.path) else {
            continue;
        };
        for occurrence in document.occurrences.iter().filter(|item| item.definition) {
            let Some(bytes) = byte_span_in_source(
                &source.text,
                &source.line_offsets,
                occurrence.span,
                document.encoding,
            ) else {
                report.occurrences_invalid_position += 1;
                continue;
            };
            let Some(line) = occurrence.span.start_line.checked_add(1) else {
                continue;
            };
            if let Some(symbol) = definition_at(source, &document.path, &bytes, line) {
                mapping.entry(occurrence.symbol).or_default().insert(symbol);
                definition_locations
                    .entry(occurrence.symbol)
                    .or_default()
                    .push((document, occurrence.span));
            }
        }
    }
    let mapping: BTreeMap<_, _> = mapping
        .into_iter()
        .filter_map(|(moniker, candidates)| {
            (candidates.len() == 1)
                .then(|| candidates.first().copied().map(|symbol| (moniker, symbol)))
                .flatten()
        })
        .collect();
    let mut aliases: BTreeMap<SymbolId, usize> = BTreeMap::new();
    for symbol in mapping.values() {
        *aliases.entry(*symbol).or_default() += 1;
    }
    let mapping: BTreeMap<_, _> = mapping
        .into_iter()
        .filter(|(_, symbol)| aliases.get(symbol) == Some(&1))
        .collect();
    report.symbols_mapped = mapping.len();
    let mut edges = Vec::new();
    for document in &prepared.documents {
        let Some(source) = verified.get(&document.path) else {
            continue;
        };
        for occurrence in document.occurrences.iter().filter(|item| !item.definition) {
            let Some(target) = mapping.get(&occurrence.symbol) else {
                report.occurrences_unmapped += 1;
                continue;
            };
            let Some(bytes) = byte_span_in_source(
                &source.text,
                &source.line_offsets,
                occurrence.span,
                document.encoding,
            ) else {
                report.occurrences_invalid_position += 1;
                continue;
            };
            let from = owner_at(source, &document.path, &bytes)
                .map(NodeRef::Symbol)
                .unwrap_or_else(|| NodeRef::File {
                    project,
                    path: document.path.clone(),
                });
            edges.push(precise_edge(
                pin,
                prepared,
                analysis_hash,
                document,
                occurrence.span,
                from.clone(),
                *target,
                "references",
            ));
            report.references_written += 1;
            if source.call_spans.contains(&(bytes.start, bytes.end))
                && prepared
                    .symbols
                    .get(occurrence.symbol)
                    .is_some_and(|symbol| symbol.callable)
            {
                edges.push(precise_edge(
                    pin,
                    prepared,
                    analysis_hash,
                    document,
                    occurrence.span,
                    from,
                    *target,
                    "calls",
                ));
                report.calls_written += 1;
            }
            if edges.len() > max_edges {
                return Err(invalid("precise relationships exceed the edge count limit"));
            }
        }
    }
    for (index, symbol) in prepared.symbols.iter().enumerate() {
        let Some(from) = mapping.get(&index) else {
            continue;
        };
        let Some((document, span)) = definition_locations
            .get(&index)
            .and_then(|list| list.first())
        else {
            continue;
        };
        for target in symbol
            .implementations
            .iter()
            .filter_map(|target| mapping.get(target))
        {
            edges.push(precise_edge(
                pin,
                prepared,
                analysis_hash,
                document,
                *span,
                NodeRef::Symbol(*from),
                *target,
                "implements",
            ));
            report.implementations_written += 1;
            if edges.len() > max_edges {
                return Err(invalid("precise relationships exceed the edge count limit"));
            }
        }
    }
    Ok((edges, report))
}

#[allow(clippy::too_many_arguments)]
fn precise_edge(
    pin: GenerationPin,
    prepared: &PreparedScipImport,
    analysis_hash: ContentHash,
    document: &PreparedDocument,
    span: PositionSpan,
    from: NodeRef,
    target: SymbolId,
    kind: &'static str,
) -> NewEdge {
    NewEdge {
        from,
        to: NodeRef::Symbol(target),
        kind: kind.to_owned(),
        evidence_type: EvidenceType::SemanticResolved,
        resolution: Resolution::Resolved,
        evidence: serde_json::json!({
            "analysis_kind": "scip",
            "analysis_format": FORMAT_VERSION,
            "view": pin.view,
            "generation": pin.generation,
            "path": document.path,
            "content_hash": document.content_hash,
            "lines": span.lines().map(|lines| [lines.start(), lines.end()]),
            "position": span,
            "position_encoding": document.encoding,
            "source_revision": prepared.manifest.source_revision,
            "artifact_hash": prepared.manifest.artifact_hash,
            "analysis_input_hash": analysis_hash,
            "compiler_identity": prepared.manifest.compiler_identity,
            "tool_name": prepared.manifest.tool_name,
            "tool_version": prepared.manifest.tool_version,
            "call_semantics": if kind == "calls" { Some("static_symbol_at_call_site") } else { None },
        }),
        origin: format!("scip:{}", document.path),
    }
}

#[cfg(test)]
mod tests {
    use knowell_parse::ParseLimits;
    use knowell_store::symbols::Symbol;
    use protobuf::{EnumOrUnknown, Message, MessageField};
    use scip::types as pb;
    use time::OffsetDateTime;

    use super::*;

    fn span(sl: u32, sc: u32, el: u32, ec: u32) -> PositionSpan {
        PositionSpan {
            start_line: sl,
            start_character: sc,
            end_line: el,
            end_character: ec,
        }
    }

    #[test]
    fn source_positions_respect_utf8_utf16_utf32_and_boundaries() {
        let source = "a😀é\ncall();\n";
        assert_eq!(byte_position(source, 0, 5, Encoding::Utf8), Some(5));
        assert_eq!(byte_position(source, 0, 3, Encoding::Utf16), Some(5));
        assert_eq!(byte_position(source, 0, 2, Encoding::Utf32), Some(5));
        assert_eq!(byte_position(source, 0, 2, Encoding::Utf8), None);
        assert_eq!(byte_position(source, 0, 2, Encoding::Utf16), None);
        assert_eq!(byte_position(source, 0, 99, Encoding::Utf32), None);
        assert_eq!(byte_position(source, 0, 0, Encoding::Unknown), None);
        assert_eq!(
            byte_span(source, span(1, 0, 1, 4), Encoding::Utf8),
            Some(8..12)
        );
        assert_eq!(
            byte_position(source, 2, 0, Encoding::Utf8),
            Some(source.len())
        );
        assert_eq!(byte_position(source, 3, 0, Encoding::Utf8), None);
    }

    #[test]
    fn exclusive_line_end_does_not_inflate_source_range() {
        assert_eq!(
            span(0, 1, 2, 0).lines(),
            Some(LineRange::new(1, 2).unwrap())
        );
        assert_eq!(
            span(0, 1, 2, 1).lines(),
            Some(LineRange::new(1, 3).unwrap())
        );
        assert!(byte_span("abc", span(0, 2, 0, 1), Encoding::Utf8).is_none());
        assert!(byte_span("abc", span(0, 1, 0, 1), Encoding::Utf8).is_none());
        assert!(span(0, 0, u32::MAX, 0).lines().is_none());
    }

    fn prepared() -> PreparedScipImport {
        let path = RepoPath::new("src/lib.rs").unwrap();
        let hash = ContentHash::of(b"fn f() {}\n");
        PreparedScipImport {
            format_version: FORMAT_VERSION,
            manifest: ScipImportManifest {
                source_revision: "a".repeat(40),
                artifact_hash: ContentHash::of(b"synthetic scip"),
                tool_name: "synthetic-indexer".to_owned(),
                tool_version: "1.0".to_owned(),
                compiler_identity: ContentHash::of(b"synthetic compiler configuration"),
                build_inputs: BTreeMap::from([(path.clone(), hash)]),
                documents: BTreeMap::from([(path.clone(), hash)]),
            },
            documents: vec![PreparedDocument {
                path,
                content_hash: hash,
                encoding: Encoding::Utf8,
                occurrences: vec![PreparedOccurrence {
                    symbol: 0,
                    span: span(0, 3, 0, 4),
                    definition: true,
                }],
            }],
            symbols: vec![PreparedSymbol {
                callable: true,
                implementations: Vec::new(),
            }],
        }
    }

    #[test]
    fn durable_payload_is_bounded_strict_and_identity_sensitive() {
        let original = prepared();
        let limits = ScipImportLimits::default();
        let bytes = original.to_json(&limits).unwrap();
        let decoded = PreparedScipImport::from_json(&bytes, &limits).unwrap();
        assert_eq!(
            original.analysis_input_hash().unwrap(),
            decoded.analysis_input_hash().unwrap()
        );
        let mut changed = decoded.clone();
        changed.manifest.compiler_identity = ContentHash::of(b"different compiler options");
        assert_ne!(
            original.analysis_input_hash().unwrap(),
            changed.analysis_input_hash().unwrap()
        );
        let small = ScipImportLimits {
            max_bytes: 2,
            ..limits
        };
        assert!(PreparedScipImport::from_json(&bytes, &small).is_err());
        assert!(PreparedScipImport::from_json(b"{", &limits).is_err());
        let mut hostile: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        hostile["documents"][0]["occurrences"][0]["symbol"] = serde_json::json!(usize::MAX);
        assert!(
            PreparedScipImport::from_json(&serde_json::to_vec(&hostile).unwrap(), &limits).is_err()
        );
        hostile = serde_json::from_slice(&bytes).unwrap();
        hostile["source_text"] = serde_json::json!("untrusted repository instructions");
        assert!(
            PreparedScipImport::from_json(&serde_json::to_vec(&hostile).unwrap(), &limits).is_err()
        );
    }

    #[test]
    fn manifest_requires_exact_document_inputs_and_safe_metadata() {
        let mut value = prepared();
        let limits = ScipImportLimits::default();
        value.manifest.build_inputs.clear();
        assert!(value.validate(&limits).is_err());
        value = prepared();
        value.manifest.tool_version = "version\nprivate configuration".to_owned();
        let error = value.validate(&limits).unwrap_err().to_string();
        assert!(!error.contains("private configuration"));
        value = prepared();
        value.documents.push(value.documents[0].clone());
        assert!(value.validate(&limits).is_err());
        value = prepared();
        value.symbols[0].implementations.push(usize::MAX);
        assert!(value.validate(&limits).is_err());
    }

    const TARGET: &str = "synthetic-indexer cargo fixture 1 src/lib/target().";
    const CALLER: &str = "synthetic-indexer cargo fixture 1 src/lib/caller().";
    const CALLBACK: &str = "local 1";

    fn raw_fixture() -> (pb::Index, ScipImportManifest) {
        let source = "fn target() {}\nfn caller(callback: fn()) {\n    target();\n    let taken = target;\n    callback();\n}\n";
        let path = RepoPath::new("src/lib.rs").unwrap();
        let hash = ContentHash::of(source.as_bytes());
        let occurrence = |row: usize, name: &str, moniker: &str, definition: bool| {
            let line = source.lines().nth(row).unwrap();
            let column = line.find(name).unwrap();
            pb::Occurrence {
                range: vec![row as i32, column as i32, (column + name.len()) as i32],
                symbol: moniker.to_owned(),
                symbol_roles: i32::from(definition),
                ..Default::default()
            }
        };
        let symbols = [
            (TARGET, "target", pb::symbol_information::Kind::Function),
            (CALLER, "caller", pb::symbol_information::Kind::Function),
            (
                CALLBACK,
                "callback",
                pb::symbol_information::Kind::Parameter,
            ),
        ]
        .into_iter()
        .map(|(symbol, name, kind)| pb::SymbolInformation {
            symbol: symbol.to_owned(),
            display_name: name.to_owned(),
            kind: EnumOrUnknown::new(kind),
            documentation: vec!["SCIP_DOCUMENTATION_MUST_NOT_BE_STAGED".to_owned()],
            ..Default::default()
        })
        .collect();
        let message = pb::Index {
            metadata: MessageField::some(pb::Metadata {
                tool_info: MessageField::some(pb::ToolInfo {
                    name: "synthetic-indexer".to_owned(),
                    version: "1.0".to_owned(),
                    ..Default::default()
                }),
                project_root: "file:///SCIP_ROOT_MUST_NOT_BE_STAGED".to_owned(),
                ..Default::default()
            }),
            documents: vec![pb::Document {
                relative_path: path.to_string(),
                text: source.to_owned(),
                language: "rust".to_owned(),
                position_encoding: EnumOrUnknown::new(
                    pb::PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
                ),
                symbols,
                occurrences: vec![
                    occurrence(0, "target", TARGET, true),
                    occurrence(1, "caller", CALLER, true),
                    occurrence(1, "callback", CALLBACK, true),
                    occurrence(2, "target", TARGET, false),
                    occurrence(3, "target", TARGET, false),
                    occurrence(4, "callback", CALLBACK, false),
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let manifest = ScipImportManifest {
            source_revision: "a".repeat(40),
            artifact_hash: ContentHash::of(&message.write_to_bytes().unwrap()),
            tool_name: "synthetic-indexer".to_owned(),
            tool_version: "1.0".to_owned(),
            compiler_identity: ContentHash::of(b"synthetic compiler options"),
            build_inputs: BTreeMap::from([(path.clone(), hash)]),
            documents: BTreeMap::from([(path, hash)]),
        };
        (message, manifest)
    }

    fn verified_fixture(prepared: &PreparedScipImport, source: &str) -> VerifiedDocument {
        let path = &prepared.documents[0].path;
        let parsed = parse_with(path, source, &ParseLimits::default(), None);
        let definitions = parsed
            .symbols
            .iter()
            .enumerate()
            .map(|(position, symbol)| Definition {
                symbol: Symbol {
                    id: SymbolId(uuid::Uuid::from_u128(position as u128 + 10)),
                    project: ProjectId(uuid::Uuid::from_u128(1)),
                    qualified_name: crate::symbol_key(path, &symbol.qualified_name),
                    kind: symbol.kind.as_str().to_owned(),
                    created_at: OffsetDateTime::UNIX_EPOCH,
                    updated_at: OffsetDateTime::UNIX_EPOCH,
                },
                path: path.clone(),
                content_hash: ContentHash::of(source.as_bytes()),
                lines: symbol.range,
            })
            .collect();
        let call_spans = identifier_scan(parsed.language, source, &ParseLimits::default())
            .identifiers
            .into_iter()
            .filter(|identifier| identifier.is_call)
            .map(|identifier| (identifier.start_byte, identifier.end_byte))
            .collect();
        VerifiedDocument::new(source.to_owned(), parsed, definitions, call_spans)
    }

    #[test]
    fn encoded_import_strips_source_docs_monikers_and_rejects_mismatches() {
        let (message, manifest) = raw_fixture();
        let bytes = message.write_to_bytes().unwrap();
        let limits = ScipImportLimits::default();
        let prepared = PreparedScipImport::from_bytes(&bytes, manifest.clone(), &limits).unwrap();
        let durable = String::from_utf8(prepared.to_json(&limits).unwrap()).unwrap();
        assert!(!durable.contains("fn target"));
        assert!(!durable.contains(TARGET));
        assert!(!durable.contains("SCIP_DOCUMENTATION_MUST_NOT_BE_STAGED"));
        assert!(!durable.contains("SCIP_ROOT_MUST_NOT_BE_STAGED"));
        assert!(
            PreparedScipImport::from_bytes(b"truncated artifact", manifest.clone(), &limits)
                .is_err()
        );
        let truncated = bytes.get(..bytes.len().saturating_sub(1)).unwrap();
        let mut malformed_manifest = manifest.clone();
        malformed_manifest.artifact_hash = ContentHash::of(truncated);
        assert!(PreparedScipImport::from_bytes(truncated, malformed_manifest, &limits).is_err());
        let mut changed = manifest.clone();
        changed.tool_version = "2.0".to_owned();
        assert!(PreparedScipImport::from_bytes(&bytes, changed, &limits).is_err());
        changed = manifest;
        changed.documents.insert(
            RepoPath::new("src/lib.rs").unwrap(),
            ContentHash::of(b"old source"),
        );
        changed.build_inputs = changed.documents.clone();
        assert!(PreparedScipImport::from_bytes(&bytes, changed, &limits).is_err());
    }

    #[test]
    fn encoded_document_framing_is_bounded_and_rejects_hostile_wire_input() {
        // A document plus unknown varint, fixed64, byte-string and fixed32
        // fields; their payloads must not be mistaken for document tags.
        let mut encoded = vec![0x12, 0, 0xa0, 0x06, 1, 0xa9, 0x06];
        encoded.extend_from_slice(&[0x12; 8]);
        encoded.extend_from_slice(&[0xb2, 0x06, 2, 0x12, 0, 0xbd, 0x06]);
        encoded.extend_from_slice(&[0x12; 4]);
        assert_eq!(encoded_document_count(&encoded, 1).unwrap(), 1);
        assert!(encoded_document_count(&encoded, 0).is_err());
        assert!(encoded_document_count(&[0x12, 0, 0x12, 0], 1).is_err());
        for malformed in [
            vec![0],                      // field number zero
            vec![0x12],                   // missing document length
            vec![0x12, 2, 0],             // truncated document payload
            vec![0x10, 0],                // document encoded as a varint
            vec![0xa9, 0x06, 0],          // truncated fixed64
            vec![0xbd, 0x06, 0],          // truncated fixed32
            vec![0x1b, 0x1c],             // unsupported deprecated groups
            vec![0x1f],                   // invalid wire type
            vec![0x80; 10],               // unterminated tag varint
            vec![0xff; 10],               // overflowing tag varint
            vec![0x12, 0xff, 0xff, 0xff], // unterminated length varint
        ] {
            assert!(encoded_document_count(&malformed, 10).is_err());
        }
    }

    #[test]
    fn duplicate_scip_documents_never_merge_source_or_position_identities() {
        let (mut original, mut manifest) = raw_fixture();
        let limits = ScipImportLimits::default();
        original.documents[0].text = original.documents[0]
            .text
            .replace("    target();", "    /*😀*/ target();");
        let column = original.documents[0]
            .text
            .lines()
            .nth(2)
            .unwrap()
            .find("target")
            .unwrap();
        original.documents[0].occurrences[3].range =
            vec![2, column as i32, (column + "target".len()) as i32];
        let path = RepoPath::new("src/lib.rs").unwrap();
        let hash = ContentHash::of(original.documents[0].text.as_bytes());
        manifest.documents.insert(path.clone(), hash);
        manifest.build_inputs.insert(path, hash);
        let bytes = original.write_to_bytes().unwrap();
        manifest.artifact_hash = ContentHash::of(&bytes);
        assert!(PreparedScipImport::from_bytes(&bytes, manifest.clone(), &limits).is_ok());
        let mut conflicting_source = original.documents[0].clone();
        conflicting_source.text = "fn unrelated() {}\n".to_owned();
        let mut absent_source = original.documents[0].clone();
        absent_source.text.clear();
        let mut conflicting_encoding = original.documents[0].clone();
        conflicting_encoding.position_encoding =
            EnumOrUnknown::new(pb::PositionEncoding::UTF16CodeUnitOffsetFromLineStart);
        let line = conflicting_encoding.text.lines().nth(2).unwrap();
        let utf16_column = line.get(..column).unwrap().encode_utf16().count();
        conflicting_encoding.occurrences[3].range = vec![
            2,
            utf16_column as i32,
            (utf16_column + "target".len()) as i32,
        ];
        let mut conflicting_language = original.documents[0].clone();
        conflicting_language.language = "typescript".to_owned();
        // Identical duplicates are also rejected. Package-specific documents
        // must be combined with one verified source/encoding before import.
        for duplicate in [
            original.documents[0].clone(),
            conflicting_source,
            absent_source,
            conflicting_encoding,
            conflicting_language,
        ] {
            let mut message = original.clone();
            message.documents.push(duplicate);
            let bytes = message.write_to_bytes().unwrap();
            let mut attestation = manifest.clone();
            attestation.artifact_hash = ContentHash::of(&bytes);
            let error = PreparedScipImport::from_bytes(&bytes, attestation, &limits)
                .unwrap_err()
                .to_string();
            assert!(error.contains("repeated document paths"));
            assert!(!error.contains("fn unrelated"));
        }
    }

    #[test]
    fn durable_import_rejects_duplicate_paths_even_with_a_matching_record_count() {
        let original = prepared();
        let mut value = serde_json::to_value(&original).unwrap();
        let other = RepoPath::new("src/other.rs").unwrap();
        let hash = original.documents[0].content_hash;
        // Matching the number of manifest entries must not make repeated paths
        // valid when the second expected document is missing from the payload.
        value["manifest"]["documents"][other.as_str()] = serde_json::to_value(hash).unwrap();
        value["manifest"]["build_inputs"][other.as_str()] = serde_json::to_value(hash).unwrap();
        let mut duplicate = value["documents"][0].clone();
        duplicate["encoding"] = serde_json::json!("utf16");
        value["documents"].as_array_mut().unwrap().push(duplicate);
        let limits = ScipImportLimits::default();
        assert!(
            PreparedScipImport::from_json(&serde_json::to_vec(&value).unwrap(), &limits).is_err()
        );
        value["documents"][1]["encoding"] = serde_json::json!("utf8");
        assert!(
            PreparedScipImport::from_json(&serde_json::to_vec(&value).unwrap(), &limits).is_err()
        );
        value["documents"][1]["content_hash"] =
            serde_json::to_value(ContentHash::of(b"other source")).unwrap();
        assert!(
            PreparedScipImport::from_json(&serde_json::to_vec(&value).unwrap(), &limits).is_err()
        );
    }

    #[test]
    fn relationship_overflow_is_rejected_with_a_bounded_reader_sentinel() {
        let (mut message, mut manifest) = raw_fixture();
        let limits = ScipImportLimits {
            max_relationships_per_symbol: 4,
            ..ScipImportLimits::default()
        };
        message.documents[0].symbols[0].relationships = (0..4)
            .map(|position| pb::Relationship {
                symbol: format!("synthetic-indexer cargo fixture 1 src/lib/trait{position}()."),
                is_implementation: true,
                ..Default::default()
            })
            .collect();
        let bytes = message.write_to_bytes().unwrap();
        manifest.artifact_hash = ContentHash::of(&bytes);
        assert!(PreparedScipImport::from_bytes(&bytes, manifest.clone(), &limits).is_ok());
        message.documents[0].symbols[0]
            .relationships
            .extend((4..2048).map(|position| pb::Relationship {
                symbol: format!("synthetic-indexer cargo fixture 1 src/lib/trait{position}()."),
                is_implementation: true,
                ..Default::default()
            }));
        let bytes = message.write_to_bytes().unwrap();
        manifest.artifact_hash = ContentHash::of(&bytes);
        let error = PreparedScipImport::from_bytes(&bytes, manifest, &limits)
            .unwrap_err()
            .to_string();
        assert!(error.contains("relationships-per-symbol limit"));
    }

    #[test]
    fn only_compiler_binding_at_actual_callable_callee_becomes_a_call() {
        let (message, manifest) = raw_fixture();
        let prepared = PreparedScipImport::from_bytes(
            &message.write_to_bytes().unwrap(),
            manifest,
            &ScipImportLimits::default(),
        )
        .unwrap();
        let source = &message.documents[0].text;
        let verified = BTreeMap::from([(
            prepared.documents[0].path.clone(),
            verified_fixture(&prepared, source),
        )]);
        let project = ProjectId(uuid::Uuid::from_u128(1));
        let pin = GenerationPin {
            view: knowell_store::ViewId(uuid::Uuid::from_u128(2)),
            generation: 3,
        };
        let (edges, report) = precise_edges(
            project,
            pin,
            &prepared,
            &verified,
            prepared.analysis_input_hash().unwrap(),
            100,
        )
        .unwrap();
        assert_eq!(report.references_written, 2);
        assert_eq!(report.calls_written, 1);
        assert_eq!(report.occurrences_unmapped, 1);
        assert_eq!(edges.iter().filter(|edge| edge.kind == "calls").count(), 1);
        assert!(
            edges
                .iter()
                .all(|edge| edge.evidence_type == EvidenceType::SemanticResolved)
        );
        let call = edges.iter().find(|edge| edge.kind == "calls").unwrap();
        assert!(matches!(call.from, NodeRef::Symbol(_)));
        assert_eq!(call.evidence["lines"], serde_json::json!([3, 3]));
        assert_eq!(
            call.evidence["call_semantics"],
            "static_symbol_at_call_site"
        );
        assert!(
            precise_edges(
                project,
                pin,
                &prepared,
                &verified,
                prepared.analysis_input_hash().unwrap(),
                0
            )
            .is_err()
        );
    }

    #[test]
    fn ambiguous_compiler_monikers_cannot_collapse_onto_one_syntactic_symbol() {
        let (mut message, mut manifest) = raw_fixture();
        let mut alias = message.documents[0].occurrences[0].clone();
        alias.symbol = "synthetic-indexer cargo fixture 2 src/lib/target().".to_owned();
        message.documents[0].occurrences.push(alias);
        manifest.artifact_hash = ContentHash::of(&message.write_to_bytes().unwrap());
        let prepared = PreparedScipImport::from_bytes(
            &message.write_to_bytes().unwrap(),
            manifest,
            &ScipImportLimits::default(),
        )
        .unwrap();
        let verified = BTreeMap::from([(
            prepared.documents[0].path.clone(),
            verified_fixture(&prepared, &message.documents[0].text),
        )]);
        let pin = GenerationPin {
            view: knowell_store::ViewId(uuid::Uuid::from_u128(2)),
            generation: 3,
        };
        let (_, report) = precise_edges(
            ProjectId(uuid::Uuid::from_u128(1)),
            pin,
            &prepared,
            &verified,
            prepared.analysis_input_hash().unwrap(),
            100,
        )
        .unwrap();
        assert_eq!(report.calls_written, 0);
        assert_eq!(report.references_written, 0);
    }

    #[test]
    fn implementations_link_distinct_known_source_definitions() {
        let (mut message, mut manifest) = raw_fixture();
        let source = "trait Runnable { fn run(&self); }\nstruct Worker;\nimpl Runnable for Worker { fn run(&self) {} }\n";
        let trait_method = "synthetic-indexer cargo fixture 1 src/lib/Runnable#run().";
        let impl_method = "synthetic-indexer cargo fixture 1 src/lib/Worker#run().";
        let occurrence = |row: usize, symbol: &str| {
            let col = source.lines().nth(row).unwrap().find("run").unwrap();
            pb::Occurrence {
                range: vec![row as i32, col as i32, (col + 3) as i32],
                symbol: symbol.to_owned(),
                symbol_roles: 1,
                ..Default::default()
            }
        };
        let information = |symbol: &str| pb::SymbolInformation {
            symbol: symbol.to_owned(),
            display_name: "run".to_owned(),
            kind: EnumOrUnknown::new(pb::symbol_information::Kind::Method),
            ..Default::default()
        };
        let mut implementation = information(impl_method);
        implementation.relationships.push(pb::Relationship {
            symbol: trait_method.to_owned(),
            is_implementation: true,
            ..Default::default()
        });
        message.documents[0].text = source.to_owned();
        message.documents[0].occurrences =
            vec![occurrence(0, trait_method), occurrence(2, impl_method)];
        message.documents[0].symbols = vec![information(trait_method), implementation];
        let hash = ContentHash::of(source.as_bytes());
        let path = RepoPath::new("src/lib.rs").unwrap();
        manifest.documents = BTreeMap::from([(path.clone(), hash)]);
        manifest.build_inputs = manifest.documents.clone();
        manifest.artifact_hash = ContentHash::of(&message.write_to_bytes().unwrap());
        let prepared = PreparedScipImport::from_bytes(
            &message.write_to_bytes().unwrap(),
            manifest,
            &ScipImportLimits::default(),
        )
        .unwrap();
        let verified = BTreeMap::from([(path, verified_fixture(&prepared, source))]);
        let pin = GenerationPin {
            view: knowell_store::ViewId(uuid::Uuid::from_u128(2)),
            generation: 3,
        };
        let (edges, report) = precise_edges(
            ProjectId(uuid::Uuid::from_u128(1)),
            pin,
            &prepared,
            &verified,
            prepared.analysis_input_hash().unwrap(),
            100,
        )
        .unwrap();
        assert_eq!(report.implementations_written, 1);
        let implementation = edges.iter().find(|edge| edge.kind == "implements").unwrap();
        assert_ne!(implementation.from, implementation.to);
        assert_eq!(implementation.evidence["lines"], serde_json::json!([3, 3]));
    }

    #[test]
    fn unknown_encoding_and_out_of_bounds_positions_do_not_become_precise_calls() {
        let (message, manifest) = raw_fixture();
        let mut prepared = PreparedScipImport::from_bytes(
            &message.write_to_bytes().unwrap(),
            manifest,
            &ScipImportLimits::default(),
        )
        .unwrap();
        prepared.documents[0].encoding = Encoding::Unknown;
        let verified = BTreeMap::from([(
            prepared.documents[0].path.clone(),
            verified_fixture(&prepared, &message.documents[0].text),
        )]);
        let pin = GenerationPin {
            view: knowell_store::ViewId(uuid::Uuid::from_u128(2)),
            generation: 3,
        };
        let (edges, report) = precise_edges(
            ProjectId(uuid::Uuid::from_u128(1)),
            pin,
            &prepared,
            &verified,
            prepared.analysis_input_hash().unwrap(),
            100,
        )
        .unwrap();
        assert!(edges.is_empty());
        assert!(report.occurrences_invalid_position > 0);
        prepared.documents[0].encoding = Encoding::Utf8;
        prepared.documents[0].occurrences[0].span = span(200, 0, 200, 6);
        let (edges, report) = precise_edges(
            ProjectId(uuid::Uuid::from_u128(1)),
            pin,
            &prepared,
            &verified,
            prepared.analysis_input_hash().unwrap(),
            100,
        )
        .unwrap();
        assert!(edges.is_empty());
        assert!(report.occurrences_invalid_position > 0);
    }
}
