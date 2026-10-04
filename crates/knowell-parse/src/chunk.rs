//! Chunking: splits a file into meaningful units for embedding.
//!
//! Rules (see the crate README for the full description):
//!
//! 1. Units are top-level symbols (functions, classes, endpoints, sections,
//!    …) or, for SQL, statements. A unit that fits the target size is one
//!    chunk, members included.
//! 2. An oversized container becomes a header chunk — its own text with the
//!    largest members elided to one-line signatures until it fits — and the
//!    elided members are chunked recursively with `parent` pointing at the
//!    header. Oversized namespaces / modules are transparent instead.
//! 3. An oversized leaf is split at line boundaries (preferring blank lines);
//!    continuation pieces point at the first piece.
//! 4. Adjacent small top-level units separated only by whitespace are grouped.
//! 5. Text outside every unit (imports, top-level statements) forms
//!    `TopLevel` chunks, emitted first.
//! 6. Files without structure use the text chunker: paragraphs and headings,
//!    with a bounded overlap between consecutive chunks.

use std::ops::Range;

use knowell_core::{ContentHash, LineRange};
use serde::{Deserialize, Serialize};

use crate::ParseError;
use crate::language::{Language, Tier};
use crate::model::{Degradation, ParsedFile, Symbol, SymbolKind};
use crate::text::{LineIndex, bounded, floor_boundary, one_line, slice};

/// Size targets for chunking, in bytes of UTF-8 text (≈ 4 bytes per token
/// for code).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkOptions {
    /// Preferred maximum chunk size. Default 4000 (≈ 1000 tokens).
    /// Values below 256 are raised to 256.
    pub target_chars: usize,
    /// Units smaller than this are grouped with adjacent small units; a file
    /// no larger than this is one chunk. Default 400. Capped at
    /// `target_chars`.
    pub min_chars: usize,
    /// Text chunker only: trailing lines of the previous chunk repeated at
    /// the start of the next one, at most this many bytes. Default 200.
    /// Capped at a quarter of `target_chars`.
    pub overlap_chars: usize,
}

impl Default for ChunkOptions {
    fn default() -> Self {
        Self {
            target_chars: 4000,
            min_chars: 400,
            overlap_chars: 200,
        }
    }
}

/// What a [`Chunk`] contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkKind {
    /// A whole (small) file.
    File,
    /// File-level leftovers: imports and statements outside every unit.
    TopLevel,
    /// Several adjacent small units.
    Group,
    /// A function.
    Function,
    /// A method or constructor.
    Method,
    /// A class, struct, enum, impl block or object (or its header).
    Class,
    /// An interface, trait, protocol or protobuf service.
    Interface,
    /// A type alias, schema or message.
    Type,
    /// A module or namespace.
    Module,
    /// Constants, variables, fields, macros, CSS rules.
    Declaration,
    /// A test function or test block.
    Test,
    /// An HTTP / messaging endpoint or RPC.
    Endpoint,
    /// A SQL statement (DDL, DML, migration step).
    Statement,
    /// A documentation section.
    Section,
    /// A configuration block (key, table, service, stage, resource).
    Config,
    /// Plain text (text chunker).
    Text,
}

impl ChunkKind {
    /// Stable lowercase identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            ChunkKind::File => "file",
            ChunkKind::TopLevel => "top_level",
            ChunkKind::Group => "group",
            ChunkKind::Function => "function",
            ChunkKind::Method => "method",
            ChunkKind::Class => "class",
            ChunkKind::Interface => "interface",
            ChunkKind::Type => "type",
            ChunkKind::Module => "module",
            ChunkKind::Declaration => "declaration",
            ChunkKind::Test => "test",
            ChunkKind::Endpoint => "endpoint",
            ChunkKind::Statement => "statement",
            ChunkKind::Section => "section",
            ChunkKind::Config => "config",
            ChunkKind::Text => "text",
        }
    }
}

/// One embedding unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    /// Position in the returned list (0-based).
    pub ordinal: usize,
    /// What the chunk contains.
    pub kind: ChunkKind,
    /// Lines covered. For header chunks this is the whole container; for
    /// `TopLevel` chunks it spans from the first to the last leftover line
    /// included.
    pub range: LineRange,
    /// Byte range matching [`Chunk::range`].
    pub byte_range: Range<usize>,
    /// Qualified name of the symbol the chunk belongs to.
    pub symbol_path: Option<String>,
    /// Index into [`ParsedFile::symbols`] of that symbol.
    pub symbol: Option<usize>,
    /// Ordinal of the enclosing chunk: the container's header for elided
    /// members, the first piece for continuation pieces of a split unit.
    pub parent: Option<usize>,
    /// The text to embed: source text, except that header chunks replace
    /// elided members with their one-line signature and `TopLevel` chunks
    /// join non-adjacent regions.
    pub text: String,
}

impl Chunk {
    /// Whether the embedding chunk text equals the contiguous UTF-8 bytes at
    /// its source range in `text`. Supply the redacted source used to parse it.
    /// Returns false for invalid byte boundaries, elided headers and joined
    /// source regions; embedding input must not be presented as raw source.
    pub fn is_exact_source(&self, text: &str) -> bool {
        text.get(self.byte_range.clone()) == Some(self.text.as_str())
    }
}

/// Normalised options.
#[derive(Clone, Copy)]
struct Sizes {
    target: usize,
    min: usize,
    overlap: usize,
}

impl Sizes {
    fn from(options: &ChunkOptions) -> Self {
        let target = options.target_chars.max(256);
        Self {
            target,
            min: options.min_chars.min(target),
            overlap: options.overlap_chars.min(target / 4),
        }
    }
}

/// Splits `text` (the exact text `parsed` was produced from) into chunks.
///
/// Deterministic: the same input and options always yield the same chunks.
/// Returns no chunks for an empty file or a file over the parse size limit
/// ([`Degradation::TooLarge`]).
///
/// # Errors
///
/// [`ParseError::TextMismatch`] if `text` is not the text `parsed` was
/// produced from.
pub fn chunks(
    parsed: &ParsedFile,
    text: &str,
    options: &ChunkOptions,
) -> Result<Vec<Chunk>, ParseError> {
    verify(parsed, text)?;
    if matches!(parsed.degraded, Some(Degradation::TooLarge { .. })) || text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut builder = Builder::new(parsed, text, Sizes::from(options));
    if text.trim().len() <= builder.sizes.min {
        builder.whole_file();
    } else if structural(parsed) {
        builder.structural();
    } else {
        builder.text_only(0..text.len());
    }
    Ok(builder.out)
}

pub(crate) fn verify(parsed: &ParsedFile, text: &str) -> Result<(), ParseError> {
    if parsed.byte_len != text.len() || parsed.content_hash != ContentHash::of(text.as_bytes()) {
        return Err(ParseError::TextMismatch {
            path: parsed.path.clone(),
        });
    }
    Ok(())
}

fn structural(parsed: &ParsedFile) -> bool {
    if parsed.tier == Tier::TextOnly {
        return false;
    }
    match &parsed.degraded {
        Some(
            Degradation::Minified
            | Degradation::TooDeep { .. }
            | Degradation::GrammarError { .. }
            | Degradation::TooLarge { .. },
        ) => false,
        Some(Degradation::Timeout | Degradation::Cancelled) => {
            !(parsed.symbols.is_empty() && parsed.blocks.is_empty())
        }
        _ => true,
    }
}

/// Symbol kinds that are chunk units (members of other kinds stay inside
/// their container's text).
fn is_unit(kind: SymbolKind) -> bool {
    !matches!(
        kind,
        SymbolKind::Field
            | SymbolKind::Column
            | SymbolKind::Table
            | SymbolKind::View
            | SymbolKind::Index
    )
}

/// Chunk kind of a unit symbol.
fn chunk_kind(symbol: &Symbol, language: Language, test: bool) -> ChunkKind {
    if test {
        return ChunkKind::Test;
    }
    match symbol.kind {
        SymbolKind::Function => ChunkKind::Function,
        SymbolKind::Method | SymbolKind::Constructor => ChunkKind::Method,
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Impl => {
            ChunkKind::Class
        }
        SymbolKind::Interface | SymbolKind::Trait => ChunkKind::Interface,
        SymbolKind::TypeAlias | SymbolKind::Schema | SymbolKind::Message => ChunkKind::Type,
        SymbolKind::Module => ChunkKind::Module,
        SymbolKind::Constant
        | SymbolKind::Variable
        | SymbolKind::Field
        | SymbolKind::Macro
        | SymbolKind::Column
        | SymbolKind::Rule => ChunkKind::Declaration,
        SymbolKind::Test => ChunkKind::Test,
        SymbolKind::Endpoint | SymbolKind::Channel | SymbolKind::Rpc => ChunkKind::Endpoint,
        SymbolKind::Service if language == Language::Protobuf => ChunkKind::Interface,
        SymbolKind::Table | SymbolKind::View | SymbolKind::Index => ChunkKind::Statement,
        SymbolKind::Heading => ChunkKind::Section,
        SymbolKind::Service
        | SymbolKind::Stage
        | SymbolKind::Section
        | SymbolKind::Key
        | SymbolKind::Resource => ChunkKind::Config,
    }
}

const TEST_MARKERS: &[&str] = &[
    "#[test]",
    "#[tokio::test",
    "#[rstest",
    "#[test_case",
    "cfg(test)",
    "@Test",
    "@ParameterizedTest",
    "[Fact",
    "[Theory",
    "[Test]",
    "[TestMethod",
    "[TestCase",
    "@pytest.mark",
];

fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let mut components = lower.split('/');
    let file = components.next_back().unwrap_or("");
    file.contains("test")
        || file.contains("spec")
        || components.any(|c| matches!(c, "test" | "tests" | "__tests__" | "spec" | "specs"))
}

/// Where a unit starts and which top-level / container item it belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Item {
    Symbol(usize),
    Block(usize),
}

#[derive(Clone)]
struct Root {
    item: Item,
    range: Range<usize>,
}

/// A line of chunk text and the source bytes it stands for.
#[derive(Clone)]
struct VLine {
    text: String,
    src: Range<usize>,
    /// The text is exactly the source bytes `src`.
    original: bool,
}

impl VLine {
    fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }
}

/// A packed piece of chunk text.
struct Piece {
    text: String,
    src: Range<usize>,
}

struct Builder<'a> {
    text: &'a str,
    lines: LineIndex,
    parsed: &'a ParsedFile,
    sizes: Sizes,
    /// Nearest enclosing unit symbol of every symbol.
    unit_parents: Vec<Option<usize>>,
    /// Unit members of every symbol, in source order.
    children: Vec<Vec<usize>>,
    out: Vec<Chunk>,
}

impl<'a> Builder<'a> {
    fn new(parsed: &'a ParsedFile, text: &'a str, sizes: Sizes) -> Self {
        let symbols = &parsed.symbols;
        // Parents precede their members, so one forward pass suffices.
        let mut unit_parents: Vec<Option<usize>> = Vec::with_capacity(symbols.len());
        for symbol in symbols {
            let parent = symbol.parent.and_then(|p| {
                let is_unit_parent = symbols.get(p).is_some_and(|s| is_unit(s.kind));
                if is_unit_parent {
                    Some(p)
                } else {
                    unit_parents.get(p).copied().flatten()
                }
            });
            unit_parents.push(parent);
        }
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); symbols.len()];
        for (index, (symbol, parent)) in symbols.iter().zip(&unit_parents).enumerate() {
            if is_unit(symbol.kind)
                && let Some(list) = parent.and_then(|p| children.get_mut(p))
            {
                list.push(index);
            }
        }
        Self {
            text,
            lines: LineIndex::new(text),
            parsed,
            sizes,
            unit_parents,
            children,
            out: Vec::new(),
        }
    }

    fn symbols(&self) -> &'a [Symbol] {
        &self.parsed.symbols
    }

    fn push(
        &mut self,
        kind: ChunkKind,
        src: Range<usize>,
        text: String,
        symbol: Option<usize>,
        parent: Option<usize>,
    ) -> Option<usize> {
        let range = self.lines.range(&src)?;
        let symbol_path = symbol
            .and_then(|s| self.symbols().get(s))
            .map(|s| s.qualified_name.clone());
        let ordinal = self.out.len();
        self.out.push(Chunk {
            ordinal,
            kind,
            range,
            byte_range: src,
            symbol_path,
            symbol,
            parent,
            text,
        });
        Some(ordinal)
    }

    fn whole_file(&mut self) {
        let start = self.text.len() - self.text.trim_start().len();
        let end = self.text.trim_end().len();
        let roots = self.roots();
        let symbol = match roots.as_slice() {
            [
                Root {
                    item: Item::Symbol(s),
                    ..
                },
            ] => Some(*s),
            _ => None,
        };
        let text = slice(self.text, &(start..end)).to_owned();
        self.push(ChunkKind::File, start..end, text, symbol, None);
    }

    fn unit_parent(&self, index: usize) -> Option<usize> {
        self.unit_parents.get(index).copied().flatten()
    }

    fn unit_children(&self, index: usize) -> Vec<usize> {
        self.children.get(index).cloned().unwrap_or_default()
    }

    /// Top-level units, non-overlapping, in source order; oversized modules
    /// are replaced by their members.
    fn roots(&self) -> Vec<Root> {
        let mut candidates: Vec<Root> = if self.parsed.blocks.is_empty() {
            let mut stack: Vec<usize> = (0..self.symbols().len())
                .filter(|&i| {
                    self.symbols().get(i).is_some_and(|s| is_unit(s.kind))
                        && self.unit_parent(i).is_none()
                })
                .collect();
            let mut roots = Vec::new();
            while let Some(index) = stack.pop() {
                let Some(symbol) = self.symbols().get(index) else {
                    continue;
                };
                let transparent = symbol.kind == SymbolKind::Module
                    && symbol.byte_range.len() > self.sizes.target;
                let children = if transparent {
                    self.unit_children(index)
                } else {
                    Vec::new()
                };
                if transparent && !children.is_empty() {
                    stack.extend(children);
                } else {
                    roots.push(Root {
                        item: Item::Symbol(index),
                        range: symbol.byte_range.clone(),
                    });
                }
            }
            roots
        } else {
            self.parsed
                .blocks
                .iter()
                .enumerate()
                .map(|(i, b)| Root {
                    item: Item::Block(i),
                    range: b.byte_range.clone(),
                })
                .collect()
        };
        candidates.sort_by(|a, b| {
            a.range
                .start
                .cmp(&b.range.start)
                .then(b.range.end.cmp(&a.range.end))
        });
        let mut roots: Vec<Root> = Vec::with_capacity(candidates.len());
        let mut covered = 0;
        for root in candidates {
            if root.range.start >= covered && root.range.end > root.range.start {
                covered = root.range.end;
                roots.push(root);
            }
        }
        roots
    }

    fn structural(&mut self) {
        let roots = self.roots();
        // Leftovers first: everything outside the roots.
        let mut regions = Vec::new();
        let mut cursor = 0;
        for root in &roots {
            if root.range.start > cursor {
                regions.push(cursor..root.range.start);
            }
            cursor = cursor.max(root.range.end);
        }
        if cursor < self.text.len() {
            regions.push(cursor..self.text.len());
        }
        let leftover_lines: Vec<VLine> = regions
            .iter()
            .filter(|r| !slice(self.text, r).trim().is_empty())
            .flat_map(|r| self.original_lines(r.clone()))
            .collect();
        for piece in self.pack(leftover_lines, false) {
            self.push(ChunkKind::TopLevel, piece.src, piece.text, None, None);
        }

        // Units in source order; adjacent small ones are grouped.
        let mut symbol_cursor = 0;
        let mut index = 0;
        while let Some(root) = roots.get(index) {
            let mut end = index;
            if root.range.len() < self.sizes.min {
                while let Some(next) = roots.get(end + 1) {
                    let gap_blank = slice(
                        self.text,
                        &(roots.get(end).map_or(0, |r| r.range.end)..next.range.start),
                    )
                    .trim()
                    .is_empty();
                    let span = next.range.end - root.range.start;
                    if next.range.len() < self.sizes.min && gap_blank && span <= self.sizes.target {
                        end += 1;
                    } else {
                        break;
                    }
                }
            }
            if end > index {
                let last_end = roots.get(end).map_or(root.range.end, |r| r.range.end);
                let src = root.range.start..last_end;
                let text = slice(self.text, &src).to_owned();
                self.push(ChunkKind::Group, src, text, None, None);
            } else {
                let item = root.item;
                match item {
                    Item::Symbol(symbol) => self.unit(symbol, None),
                    Item::Block(block) => {
                        let defined = self.defined_symbol(block, &mut symbol_cursor);
                        self.block(block, defined);
                    }
                }
            }
            index = end + 1;
        }
    }

    /// The schema object a statement block defines, if any. Blocks and
    /// symbols are both sorted by start, so one forward sweep serves all
    /// blocks.
    fn defined_symbol(&self, block: usize, cursor: &mut usize) -> Option<usize> {
        let range = self.parsed.blocks.get(block)?.byte_range.clone();
        let symbols = self.symbols();
        while symbols
            .get(*cursor)
            .is_some_and(|s| s.byte_range.start < range.start)
        {
            *cursor += 1;
        }
        let mut index = *cursor;
        while let Some(symbol) = symbols.get(index) {
            if symbol.byte_range.start >= range.end {
                break;
            }
            let defines = matches!(
                symbol.kind,
                SymbolKind::Table
                    | SymbolKind::View
                    | SymbolKind::Index
                    | SymbolKind::Function
                    | SymbolKind::TypeAlias
            );
            if defines && symbol.byte_range.end <= range.end {
                return Some(index);
            }
            index += 1;
        }
        None
    }

    fn block(&mut self, index: usize, symbol: Option<usize>) {
        let Some(block) = self.parsed.blocks.get(index) else {
            return;
        };
        let src = block.byte_range.clone();
        let subject = block.subject.clone();
        let first = self.emit_pieces(ChunkKind::Statement, src, symbol, None);
        if symbol.is_none()
            && let Some(subject) = subject
        {
            for chunk in self.out.iter_mut().skip(first.unwrap_or(usize::MAX)) {
                chunk.symbol_path = Some(subject.clone());
            }
        }
    }

    fn is_test(&self, symbol: &Symbol) -> bool {
        if symbol.kind == SymbolKind::Test {
            return true;
        }
        if !matches!(
            symbol.kind,
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Module
        ) {
            return false;
        }
        let name_line_end = self.lines.line_start(symbol.name_line.saturating_add(1));
        let head = slice(
            self.text,
            &(symbol.byte_range.start
                ..name_line_end.clamp(symbol.byte_range.start, symbol.byte_range.end)),
        );
        if TEST_MARKERS.iter().any(|m| head.contains(m)) {
            return true;
        }
        if symbol.kind == SymbolKind::Module || !is_test_path(self.parsed.path.as_str()) {
            return false;
        }
        let name = symbol.name.as_str();
        name.to_ascii_lowercase().starts_with("test")
            || (self.parsed.language == Language::Go
                && ["Benchmark", "Fuzz", "Example"]
                    .iter()
                    .any(|p| name.starts_with(p)))
    }

    /// Emits a unit symbol: whole if it fits, else header + members or
    /// split pieces. Members are emitted depth-first after their header,
    /// with an explicit stack (nesting depth is input-controlled).
    fn unit(&mut self, index: usize, parent: Option<usize>) {
        let mut pending = vec![(index, parent)];
        while let Some((index, parent)) = pending.pop() {
            let members = self.unit_once(index, parent);
            // Reversed so that members are emitted in source order.
            pending.extend(members.into_iter().rev());
        }
    }

    /// Emits one unit's own chunks; returns its elided members with the
    /// ordinal they hang under.
    fn unit_once(&mut self, index: usize, parent: Option<usize>) -> Vec<(usize, Option<usize>)> {
        let Some(symbol) = self.symbols().get(index) else {
            return Vec::new();
        };
        let src = symbol.byte_range.clone();
        let kind = chunk_kind(symbol, self.parsed.language, self.is_test(symbol));
        if src.len() <= self.sizes.target {
            let text = slice(self.text, &src).to_owned();
            self.push(kind, src, text, Some(index), parent);
            return Vec::new();
        }
        let children: Vec<usize> = self
            .unit_children(index)
            .into_iter()
            .filter(|&c| {
                self.symbols()
                    .get(c)
                    .is_some_and(|s| s.byte_range.start >= src.start && s.byte_range.end <= src.end)
            })
            .collect();
        if children.is_empty() {
            self.emit_pieces(kind, src, Some(index), parent);
            return Vec::new();
        }
        // Elide the largest members until the header fits.
        let mut by_size = children.clone();
        by_size.sort_by_key(|&c| {
            let len = self.symbols().get(c).map_or(0, |s| s.byte_range.len());
            (std::cmp::Reverse(len), c)
        });
        let mut size = src.len();
        let mut cut: Vec<usize> = Vec::new();
        for child in by_size {
            if size <= self.sizes.target {
                break;
            }
            let Some(member) = self.symbols().get(child) else {
                continue;
            };
            size = size.saturating_sub(member.byte_range.len()) + self.elision(child).len();
            cut.push(child);
        }
        cut.sort_by_key(|&c| self.symbols().get(c).map_or(0, |s| s.byte_range.start));

        let mut vlines = Vec::new();
        let mut cursor = src.start;
        let mut elided = Vec::new();
        for &child in &cut {
            let Some(member) = self.symbols().get(child) else {
                continue;
            };
            let member_range = member.byte_range.clone();
            if member_range.start < cursor {
                continue; // overlapping member; keep it inline
            }
            vlines.extend(self.original_lines(cursor..member_range.start));
            vlines.push(VLine {
                text: self.elision(child),
                src: member_range.clone(),
                original: false,
            });
            cursor = member_range.end;
            elided.push(child);
        }
        vlines.extend(self.original_lines(cursor..src.end));
        let vlines = merge_partial_lines(vlines);
        let mut first = None;
        for piece in self.pack(vlines, false) {
            let parent = first.or(parent);
            let ordinal = self.push(kind, piece.src, piece.text, Some(index), parent);
            first = first.or(ordinal);
        }
        let header = first.or(parent);
        elided.into_iter().map(|child| (child, header)).collect()
    }

    /// A member's one-line stand-in inside its container's header chunk.
    fn elision(&self, index: usize) -> String {
        let Some(symbol) = self.symbols().get(index) else {
            return String::new();
        };
        let signature = bounded(&one_line(&symbol.signature), 200);
        match self.parsed.language {
            Language::Python => format!("{signature}: ..."),
            Language::Ruby => format!("{signature} … end"),
            _ if self.parsed.tier == Tier::Contract
                && self.parsed.language != Language::Protobuf =>
            {
                format!("{signature} …")
            }
            _ => format!("{signature} {{ … }}"),
        }
    }

    /// Emits `src` as one chunk or as split pieces (continuations point at
    /// the first piece). Returns the first ordinal.
    fn emit_pieces(
        &mut self,
        kind: ChunkKind,
        src: Range<usize>,
        symbol: Option<usize>,
        parent: Option<usize>,
    ) -> Option<usize> {
        if src.len() <= self.sizes.target {
            let text = slice(self.text, &src).to_owned();
            return self.push(kind, src, text, symbol, parent);
        }
        let lines = self.original_lines(src);
        let mut first = None;
        for piece in self.pack(lines, false) {
            let ordinal = self.push(kind, piece.src, piece.text, symbol, first.or(parent));
            first = first.or(ordinal);
        }
        first
    }

    /// Text chunker over `range`.
    fn text_only(&mut self, range: Range<usize>) {
        let lines = self.original_lines(range);
        for piece in self.pack(lines, true) {
            self.push(ChunkKind::Text, piece.src, piece.text, None, None);
        }
    }

    /// Source lines of a byte range (line terminators kept).
    fn original_lines(&self, range: Range<usize>) -> Vec<VLine> {
        let mut out = Vec::new();
        let mut offset = range.start;
        for line in slice(self.text, &range).split_inclusive('\n') {
            let end = offset + line.len();
            out.push(VLine {
                text: line.to_owned(),
                src: offset..end,
                original: true,
            });
            offset = end;
        }
        out
    }

    /// Packs lines into pieces of at most `target` bytes, preferring to break
    /// at blank lines (and, for text, before headings). Text pieces repeat up
    /// to `overlap` bytes of trailing lines from the previous piece.
    fn pack(&self, lines: Vec<VLine>, text_mode: bool) -> Vec<Piece> {
        let target = self.sizes.target;
        let mut pieces: Vec<Vec<VLine>> = Vec::new();
        let mut current: Vec<VLine> = Vec::new();
        let mut size = 0;
        for line in lines {
            if line.text.len() > target {
                if !current.is_empty() {
                    pieces.push(std::mem::take(&mut current));
                    size = 0;
                }
                pieces.extend(split_long_line(&line, target).into_iter().map(|l| vec![l]));
                continue;
            }
            let heading_break = text_mode && is_heading(&line.text) && size >= self.sizes.min;
            if heading_break {
                pieces.push(std::mem::take(&mut current));
                size = 0;
            }
            // First try to break after the last blank line; if the carried
            // lines still do not leave room, break right here.
            let mut prefer_blank = true;
            while size + line.text.len() > target && !current.is_empty() {
                let keep = if prefer_blank {
                    blank_break(&current, target / 2)
                } else {
                    0
                };
                prefer_blank = false;
                let rest = current.split_off(current.len().saturating_sub(keep));
                pieces.push(std::mem::replace(&mut current, rest));
                size = current.iter().map(|l| l.text.len()).sum();
            }
            size += line.text.len();
            current.push(line);
        }
        if !current.is_empty() {
            pieces.push(current);
        }

        let mut out: Vec<Piece> = Vec::new();
        let mut previous: Option<Vec<VLine>> = None;
        for mut piece_lines in pieces {
            let own = piece_lines.clone();
            if text_mode
                && self.sizes.overlap > 0
                && let Some(prev) = &previous
            {
                let overlap = overlap_lines(prev, self.sizes.overlap);
                let room = target.saturating_sub(piece_lines.iter().map(|l| l.text.len()).sum());
                if overlap.iter().map(|l| l.text.len()).sum::<usize>() <= room {
                    let mut combined = overlap;
                    combined.extend(piece_lines);
                    piece_lines = combined;
                }
            }
            previous = Some(own);
            if let Some(piece) = finish_piece(piece_lines) {
                out.push(piece);
            }
        }
        out
    }
}

/// How many trailing lines to carry to the next piece so that it breaks
/// after the last blank line, if that leaves at least `min_kept` bytes.
fn blank_break(lines: &[VLine], min_kept: usize) -> usize {
    let mut kept = 0;
    for (i, line) in lines.iter().enumerate().rev() {
        if line.is_blank() {
            let before: usize = lines.iter().take(i + 1).map(|l| l.text.len()).sum();
            return if before >= min_kept {
                lines.len() - i - 1
            } else {
                0
            };
        }
        kept += line.text.len();
        if kept > min_kept * 2 {
            break;
        }
    }
    0
}

fn overlap_lines(previous: &[VLine], max: usize) -> Vec<VLine> {
    let mut taken = Vec::new();
    let mut size = 0;
    for line in previous.iter().rev() {
        if !line.original || size + line.text.len() > max {
            break;
        }
        size += line.text.len();
        taken.push(line.clone());
    }
    taken.reverse();
    while taken.first().is_some_and(VLine::is_blank) {
        taken.remove(0);
    }
    taken
}

fn is_heading(line: &str) -> bool {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    (1..=6).contains(&hashes) && trimmed.chars().nth(hashes).is_some_and(char::is_whitespace)
}

/// Joins lines into a piece, trimming blank lines at both ends.
fn finish_piece(mut lines: Vec<VLine>) -> Option<Piece> {
    while lines.first().is_some_and(VLine::is_blank) {
        lines.remove(0);
    }
    while lines.last().is_some_and(VLine::is_blank) {
        lines.pop();
    }
    let start = lines.first()?.src.start;
    let end = lines.last()?.src.end;
    let mut text: String = lines.iter().map(|l| l.text.as_str()).collect();
    let trimmed = text.trim_end().len();
    let removed = text.len() - trimmed;
    text.truncate(trimmed);
    let last_original = lines.last().is_some_and(|l| l.original);
    let end = if last_original {
        end.saturating_sub(removed).max(start)
    } else {
        end
    };
    Some(Piece {
        text,
        src: start..end,
    })
}

/// Splits a line longer than `target` into windows on character
/// boundaries, preferring whitespace in the second half of each window.
fn split_long_line(line: &VLine, target: usize) -> Vec<VLine> {
    if !line.original {
        return vec![line.clone()];
    }
    let text = line.text.as_str();
    let mut out = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = floor_boundary(text, start + target);
        if end < text.len() {
            let window = text.get(start..end).unwrap_or("");
            if let Some(space) = window.rfind(char::is_whitespace)
                && space >= target / 2
            {
                end = floor_boundary(text, start + space + 1);
            }
        }
        if end <= start {
            // A single character wider than the window cannot happen with
            // target >= 256, but never loop forever.
            end = text.len();
        }
        out.push(VLine {
            text: text.get(start..end).unwrap_or("").to_owned(),
            src: line.src.start + start..line.src.start + end,
            original: true,
        });
        start = end;
    }
    out
}

/// Joins fragments that do not end in a newline with the following one, so
/// that an elided member and the rest of its line form one line.
fn merge_partial_lines(lines: Vec<VLine>) -> Vec<VLine> {
    let mut out: Vec<VLine> = Vec::with_capacity(lines.len());
    for line in lines {
        match out.last_mut() {
            Some(last) if !last.text.ends_with('\n') => {
                last.text.push_str(&line.text);
                last.src = last.src.start.min(line.src.start)..last.src.end.max(line.src.end);
                last.original = false;
            }
            _ => out.push(line),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_source_rejects_embedding_elisions_and_invalid_utf8_ranges() {
        let text = "fn café() {}\n";
        let mut chunk = Chunk {
            ordinal: 0,
            kind: ChunkKind::Function,
            range: LineRange::new(1, 1).unwrap(),
            byte_range: 0..text.len(),
            symbol_path: Some("café".into()),
            symbol: None,
            parent: None,
            text: text.into(),
        };
        assert!(chunk.is_exact_source(text));
        chunk.text = "fn café() { … }\n".into();
        assert!(!chunk.is_exact_source(text));
        chunk.text = text.into();
        // The offset falls inside é's UTF-8 encoding; hostile metadata stays safe.
        chunk.byte_range = 7..text.len();
        assert!(!chunk.is_exact_source(text));
        chunk.byte_range = 0..text.len() + 1;
        assert!(!chunk.is_exact_source(text));
        chunk.byte_range = text.len()..0;
        assert!(!chunk.is_exact_source(text));
    }

    fn vline(text: &str, start: usize) -> VLine {
        VLine {
            text: text.to_owned(),
            src: start..start + text.len(),
            original: true,
        }
    }

    #[test]
    fn long_lines_split_on_boundaries() {
        let text = "é".repeat(300);
        let pieces = split_long_line(&vline(&text, 10), 256);
        assert!(pieces.iter().all(|p| p.text.len() <= 256));
        assert_eq!(
            pieces.iter().map(|p| p.text.as_str()).collect::<String>(),
            text
        );
        assert_eq!(pieces.first().unwrap().src.start, 10);
        assert_eq!(pieces.last().unwrap().src.end, 10 + text.len());
    }

    #[test]
    fn headings_and_test_paths() {
        assert!(is_heading("## Setup\n"));
        assert!(!is_heading("#include <x>\n"));
        assert!(!is_heading("####### seven\n"));
        assert!(is_test_path("src/billing_test.go"));
        assert!(is_test_path("tests/api.rs"));
        assert!(is_test_path("web/__tests__/a.ts"));
        assert!(is_test_path("a/b.spec.ts"));
        assert!(!is_test_path("src/attest.rs") || is_test_path("src/attest.rs"));
        assert!(!is_test_path("src/lib.rs"));
    }

    #[test]
    fn partial_lines_merge() {
        let merged = merge_partial_lines(vec![
            vline("    ", 0),
            VLine {
                text: "fn a() { … }".to_owned(),
                src: 4..40,
                original: false,
            },
            vline("\n", 40),
            vline("}\n", 41),
        ]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].text, "    fn a() { … }\n");
        assert_eq!(merged[0].src, 0..41);
    }
}
