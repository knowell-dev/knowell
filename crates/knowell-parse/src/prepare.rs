//! The exact embedding input of a chunk and its cache hash.

use knowell_core::{ContentHash, RepoPath};
use serde::{Deserialize, Serialize};

use crate::PARSER_VERSION;
use crate::chunk::Chunk;
use crate::language::Language;
use crate::model::ParsedFile;
use crate::text::{bounded, one_line, summary};

/// Version of the prepared-input layout (header fields, order, separators).
/// Bump it whenever [`prepared_input`] output changes for the same chunk and
/// context: the embedding cache key then changes and vectors are rebuilt.
pub const PREPARED_FORMAT_VERSION: u32 = 1;

const MAX_HEADER_FIELD_BYTES: usize = 300;

/// Context placed before a chunk's text in its embedding input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkContext<'a> {
    /// Project name (empty to omit).
    pub project: &'a str,
    /// File path.
    pub path: &'a RepoPath,
    /// File language.
    pub language: Language,
    /// Signature of the enclosing symbol: the container of the chunk's
    /// symbol, or the symbol itself for continuation pieces of a split unit.
    pub container_signature: Option<&'a str>,
    /// Doc comment of that same enclosing symbol (summarised in the header).
    pub doc: Option<&'a str>,
}

impl<'a> ChunkContext<'a> {
    /// Derives the context of `chunk` from the file it was cut from.
    pub fn for_chunk(project: &'a str, parsed: &'a ParsedFile, chunk: &Chunk) -> Self {
        let symbol = chunk.symbol.and_then(|index| parsed.symbols.get(index));
        let container = match symbol {
            // A continuation piece does not contain its symbol's signature.
            Some(symbol) if chunk.byte_range.start > symbol.byte_range.start => Some(symbol),
            Some(symbol) => symbol.parent.and_then(|p| parsed.symbols.get(p)),
            None => None,
        };
        Self {
            project,
            path: &parsed.path,
            language: parsed.language,
            container_signature: container.map(|c| c.signature.as_str()),
            doc: container.and_then(|c| c.doc.as_deref()),
        }
    }
}

/// The embedding input of one chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedInput {
    /// Short title (`path · symbol`), for providers with a title field.
    pub title: String,
    /// Context header, a blank line, then the chunk text.
    pub text: String,
    /// Cache key: covers [`PARSER_VERSION`], [`PREPARED_FORMAT_VERSION`],
    /// the title and the text.
    pub hash: ContentHash,
}

/// Builds the exact text that is embedded for `chunk`:
///
/// ```text
/// path: src/billing/service.ts
/// language: typescript
/// project: billing-api
/// symbol: SubscriptionService.cancelSubscription (method)
/// container: export class SubscriptionService
/// doc: Subscription service.
///
/// <chunk text>
/// ```
///
/// Absent fields are omitted. Deterministic.
pub fn prepared_input(chunk: &Chunk, context: &ChunkContext<'_>) -> PreparedInput {
    let title = match &chunk.symbol_path {
        Some(symbol) => format!("{} · {symbol}", context.path),
        None => context.path.to_string(),
    };
    let mut text = String::with_capacity(chunk.text.len() + 256);
    text.push_str(&format!("path: {}\n", context.path));
    text.push_str(&format!("language: {}\n", context.language));
    if !context.project.trim().is_empty() {
        text.push_str(&format!("project: {}\n", one_line(context.project)));
    }
    match &chunk.symbol_path {
        Some(symbol) => text.push_str(&format!("symbol: {symbol} ({})\n", chunk.kind.as_str())),
        None => text.push_str(&format!("kind: {}\n", chunk.kind.as_str())),
    }
    if let Some(signature) = context.container_signature {
        let signature = bounded(&one_line(signature), MAX_HEADER_FIELD_BYTES);
        if !signature.is_empty() {
            text.push_str(&format!("container: {signature}\n"));
        }
    }
    if let Some(doc) = summary(context.doc, 1) {
        text.push_str(&format!("doc: {}\n", bounded(&doc, MAX_HEADER_FIELD_BYTES)));
    }
    text.push('\n');
    text.push_str(&chunk.text);
    let hash = ContentHash::of_parts([
        b"knowell.prepared".as_slice(),
        &PARSER_VERSION.to_le_bytes(),
        &PREPARED_FORMAT_VERSION.to_le_bytes(),
        title.as_bytes(),
        text.as_bytes(),
    ]);
    PreparedInput { title, text, hash }
}
