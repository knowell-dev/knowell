//! Shared helpers for the integration tests. Fixtures are synthetic and
//! written inline in each test.

// Each test crate uses a different subset of the helpers, and the helpers
// are test code: failing loudly with context is their purpose.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use knowell_core::RepoPath;
use knowell_parse::{
    Chunk, ChunkKind, ChunkOptions, ParsedFile, Symbol, SymbolKind, chunks, parse, skeleton,
};

pub(crate) fn path(p: &str) -> RepoPath {
    RepoPath::new(p).unwrap()
}

pub(crate) fn parsed(p: &str, text: &str) -> ParsedFile {
    parse(&path(p), text)
}

/// The symbol with this qualified name and kind; panics with the full list.
pub(crate) fn symbol<'a>(file: &'a ParsedFile, kind: SymbolKind, qualified: &str) -> &'a Symbol {
    file.symbols
        .iter()
        .find(|s| s.kind == kind && s.qualified_name == qualified)
        .unwrap_or_else(|| panic!("no {kind:?} `{qualified}` in {:#?}", summary(file)))
}

/// `(kind, qualified name, first line, last line)` for every symbol.
pub(crate) fn summary(file: &ParsedFile) -> Vec<(SymbolKind, String, u32, u32)> {
    file.symbols
        .iter()
        .map(|s| {
            (
                s.kind,
                s.qualified_name.clone(),
                s.range.start(),
                s.range.end(),
            )
        })
        .collect()
}

/// Asserts that every expected `(kind, qualified, start, end)` is present.
pub(crate) fn expect_symbols(file: &ParsedFile, expected: &[(SymbolKind, &str, u32, u32)]) {
    let actual = summary(file);
    for (kind, qualified, start, end) in expected {
        assert!(
            actual
                .iter()
                .any(|(k, q, s, e)| k == kind && q == qualified && s == start && e == end),
            "missing {kind:?} `{qualified}` L{start}-L{end} in {actual:#?}"
        );
    }
}

pub(crate) fn qualified_names(file: &ParsedFile) -> Vec<&str> {
    file.symbols
        .iter()
        .map(|s| s.qualified_name.as_str())
        .collect()
}

pub(crate) fn imports(file: &ParsedFile) -> Vec<&str> {
    file.imports.iter().map(|i| i.specifier.as_str()).collect()
}

pub(crate) fn options(target: usize) -> ChunkOptions {
    ChunkOptions {
        target_chars: target,
        ..ChunkOptions::default()
    }
}

/// Chunks `text` and checks the invariants every chunk list must satisfy.
pub(crate) fn chunked(file: &ParsedFile, text: &str, options: &ChunkOptions) -> Vec<Chunk> {
    let list = chunks(file, text, options).unwrap();
    check_chunks(file, text, &list, options);
    list
}

pub(crate) fn check_chunks(file: &ParsedFile, text: &str, list: &[Chunk], options: &ChunkOptions) {
    let target = options.target_chars.max(256);
    // Header chunks (members elided) are the symbols whose chunks are the
    // parent of a chunk of another symbol.
    let headers: Vec<Option<usize>> = list
        .iter()
        .filter_map(|c| {
            let parent = list.get(c.parent?)?;
            (parent.symbol != c.symbol).then_some(parent.symbol)
        })
        .collect();
    for (index, chunk) in list.iter().enumerate() {
        assert_eq!(chunk.ordinal, index);
        assert!(!chunk.text.trim().is_empty(), "empty chunk {chunk:#?}");
        assert!(
            chunk.text.len() <= target,
            "chunk {index} is {} bytes > {target}: {chunk:#?}",
            chunk.text.len()
        );
        if let Some(parent) = chunk.parent {
            assert!(
                parent < index,
                "parent {parent} of chunk {index} is not earlier"
            );
        }
        let source = text
            .get(chunk.byte_range.clone())
            .unwrap_or_else(|| panic!("chunk {index} byte range off boundaries: {chunk:#?}"));
        assert!(chunk.range.start() >= 1 && chunk.range.end() <= file.line_count.max(1));
        let header = chunk.symbol.is_some() && headers.contains(&chunk.symbol);
        if chunk.kind != ChunkKind::TopLevel && !header {
            assert_eq!(
                chunk.text, source,
                "chunk {index} text differs from its range"
            );
        }
        if let Some(symbol) = chunk.symbol {
            let symbol = &file.symbols[symbol];
            assert_eq!(
                chunk.symbol_path.as_deref(),
                Some(symbol.qualified_name.as_str())
            );
        }
    }
}

/// Every non-whitespace byte of the file appears in some chunk's range.
pub(crate) fn assert_covers(text: &str, list: &[Chunk]) {
    for (offset, c) in text.char_indices() {
        if c.is_whitespace() {
            continue;
        }
        assert!(
            list.iter().any(|chunk| chunk.byte_range.contains(&offset)),
            "byte {offset} ({c:?}) is not covered by any chunk"
        );
    }
}

pub(crate) fn skeleton_of(file: &ParsedFile, text: &str) -> String {
    skeleton(file, text).unwrap()
}

pub(crate) fn chunk_for<'a>(list: &'a [Chunk], symbol_path: &str) -> &'a Chunk {
    list.iter()
        .find(|c| c.symbol_path.as_deref() == Some(symbol_path))
        .unwrap_or_else(|| {
            panic!(
                "no chunk for `{symbol_path}` in {:#?}",
                list.iter()
                    .map(|c| (c.ordinal, c.kind, c.symbol_path.clone(), c.parent))
                    .collect::<Vec<_>>()
            )
        })
}
