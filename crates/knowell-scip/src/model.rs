//! Knowell-side data model for an ingested SCIP index.

use knowell_core::{ContentHash, LineRange, RepoPath};

use crate::symbol::ScipSymbol;

/// Index of a symbol inside one [`ScipIndex`](crate::ScipIndex).
///
/// Ids are only meaningful for the index that produced them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SymbolId(pub(crate) usize);

impl SymbolId {
    /// The position of the symbol in [`ScipIndex::symbols`](crate::ScipIndex::symbols).
    pub fn index(self) -> usize {
        self.0
    }
}

/// A source range exactly as SCIP states it: 0-based lines and 0-based
/// character offsets, with an exclusive end character.
///
/// Characters are counted in the document's [`PositionEncoding`] (UTF-8, UTF-16
/// or UTF-32 code units); the engine never converts them. Field order gives
/// the natural ordering: by start, then by end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Span {
    /// First line, 0-based.
    pub start_line: u32,
    /// First character on `start_line`, 0-based.
    pub start_character: u32,
    /// Last line, 0-based (inclusive).
    pub end_line: u32,
    /// End character on `end_line`, 0-based, exclusive.
    pub end_character: u32,
}

/// Why a SCIP range array was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RangeError {
    /// SCIP ranges have exactly 3 (single line) or 4 (multi-line) elements.
    #[error("range has {0} elements, expected 3 or 4")]
    BadLength(usize),
    /// A coordinate is negative.
    #[error("range contains a negative coordinate")]
    Negative,
    /// The end is before the start.
    #[error("range end is before its start")]
    Reversed,
}

impl Span {
    /// Converts a SCIP `range` array: `[line, start_char, end_char]` or
    /// `[start_line, start_char, end_line, end_char]`.
    pub fn from_scip(raw: &[i32]) -> Result<Self, RangeError> {
        let (sl, sc, el, ec) = match *raw {
            [line, start, end] => (line, start, line, end),
            [sl, sc, el, ec] => (sl, sc, el, ec),
            _ => return Err(RangeError::BadLength(raw.len())),
        };
        let conv = |v: i32| u32::try_from(v).map_err(|_| RangeError::Negative);
        let span = Self {
            start_line: conv(sl)?,
            start_character: conv(sc)?,
            end_line: conv(el)?,
            end_character: conv(ec)?,
        };
        if (span.end_line, span.end_character) < (span.start_line, span.start_character) {
            return Err(RangeError::Reversed);
        }
        Ok(span)
    }

    /// The covered lines as a 1-based inclusive [`LineRange`].
    ///
    /// Returns `None` only if the arithmetic would overflow `u32`, which
    /// cannot happen for spans built by [`Span::from_scip`] (coordinates fit `i32`).
    pub fn line_range(&self) -> Option<LineRange> {
        let start = self.start_line.checked_add(1)?;
        let end = self.end_line.checked_add(1)?;
        LineRange::new(start, end).ok()
    }

    /// Whether the 0-based position `(line, character)` lies inside the span
    /// (end exclusive; an empty span contains only its own position).
    pub fn contains(&self, line: u32, character: u32) -> bool {
        let pos = (line, character);
        let start = (self.start_line, self.start_character);
        let end = (self.end_line, self.end_character);
        if start == end {
            return pos == start;
        }
        start <= pos && pos < end
    }

    /// Size used to pick the innermost of several nested spans:
    /// `(lines, characters)` where characters is only comparable for spans on one line.
    pub(crate) fn extent(&self) -> (u32, u32) {
        let lines = self.end_line.saturating_sub(self.start_line);
        let chars = if lines == 0 {
            self.end_character.saturating_sub(self.start_character)
        } else {
            self.end_character
        };
        (lines, chars)
    }
}

/// How a position encodes characters in a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PositionEncoding {
    /// The indexer did not say; treat as unknown.
    Unspecified,
    /// UTF-8 code units (bytes).
    Utf8,
    /// UTF-16 code units.
    Utf16,
    /// UTF-32 code units (Unicode scalar values).
    Utf32,
}

/// The primary role of an occurrence, chosen from the SCIP role bitset with the
/// priority `Definition > ForwardDefinition > Import > WriteAccess > ReadAccess > Reference`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OccurrenceRole {
    /// The symbol is defined here.
    Definition,
    /// A forward declaration (for example a C/C++ header declaration).
    ForwardDefinition,
    /// The symbol is imported here.
    Import,
    /// The symbol is written to.
    WriteAccess,
    /// The symbol is read.
    ReadAccess,
    /// A plain reference: no role bit is set.
    Reference,
}

/// The raw SCIP `symbol_roles` bitset of an occurrence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SymbolRoles(pub i32);

impl SymbolRoles {
    const DEFINITION: i32 = 1;
    const IMPORT: i32 = 2;
    const WRITE: i32 = 4;
    const READ: i32 = 8;
    const GENERATED: i32 = 16;
    const TEST: i32 = 32;
    const FORWARD: i32 = 64;

    /// The symbol is defined here.
    pub fn is_definition(self) -> bool {
        self.0 & Self::DEFINITION != 0
    }
    /// The symbol is imported here.
    pub fn is_import(self) -> bool {
        self.0 & Self::IMPORT != 0
    }
    /// The symbol is written to.
    pub fn is_write(self) -> bool {
        self.0 & Self::WRITE != 0
    }
    /// The symbol is read.
    pub fn is_read(self) -> bool {
        self.0 & Self::READ != 0
    }
    /// The occurrence is in generated code.
    pub fn is_generated(self) -> bool {
        self.0 & Self::GENERATED != 0
    }
    /// The occurrence is in test code.
    pub fn is_test(self) -> bool {
        self.0 & Self::TEST != 0
    }
    /// The occurrence is a forward declaration.
    pub fn is_forward_definition(self) -> bool {
        self.0 & Self::FORWARD != 0
    }

    /// The single primary role (see [`OccurrenceRole`]).
    pub fn primary(self) -> OccurrenceRole {
        if self.is_definition() {
            OccurrenceRole::Definition
        } else if self.is_forward_definition() {
            OccurrenceRole::ForwardDefinition
        } else if self.is_import() {
            OccurrenceRole::Import
        } else if self.is_write() {
            OccurrenceRole::WriteAccess
        } else if self.is_read() {
            OccurrenceRole::ReadAccess
        } else {
            OccurrenceRole::Reference
        }
    }
}

/// One use of a symbol inside a document. The document path is held by the
/// owning [`Document`]; queries return it alongside via [`Located`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    /// The symbol that occurs.
    pub symbol: SymbolId,
    /// The exact range, 0-based, as the indexer reported it.
    pub span: Span,
    /// The covered lines, 1-based and inclusive (Knowell convention).
    pub line_range: LineRange,
    /// Raw role bits.
    pub roles: SymbolRoles,
    /// Primary role derived from `roles`.
    pub role: OccurrenceRole,
    /// For definitions: the range of the whole enclosing construct (for
    /// example a function body), if the indexer provided a valid one.
    pub enclosing_span: Option<Span>,
    /// `enclosing_span` as 1-based inclusive lines.
    pub enclosing_range: Option<LineRange>,
}

/// An occurrence together with the document it belongs to.
#[derive(Debug, Clone, Copy)]
pub struct Located<'a> {
    /// Path relative to the project root.
    pub path: &'a RepoPath,
    /// The occurrence.
    pub occurrence: &'a Occurrence,
}

/// An indexed source file.
#[derive(Debug, Clone)]
pub struct Document {
    /// Path relative to the project root (validated, never escaping it).
    pub path: RepoPath,
    /// Language name as reported by the indexer (may be empty).
    pub language: String,
    /// Character encoding of this document's positions.
    pub position_encoding: PositionEncoding,
    /// BLAKE3 hash of the document text the indexer embedded, or of a
    /// caller-supplied manifest entry; `None` when neither is known.
    pub content_hash: Option<ContentHash>,
    /// Occurrences sorted by span.
    pub occurrences: Vec<Occurrence>,
}

/// Broad symbol kinds; SCIP's ~80 kinds are folded into what agents can use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SymbolKind {
    /// The indexer did not state a kind.
    Unspecified,
    /// Class, singleton class, object.
    Class,
    /// Struct, union.
    Struct,
    /// Interface, protocol, trait, type class, concept.
    Interface,
    /// Enum.
    Enum,
    /// Enum member / constant of an enum.
    EnumMember,
    /// Free function.
    Function,
    /// Method of any flavour (static, abstract, getter, setter, accessor, …).
    Method,
    /// Constructor.
    Constructor,
    /// Field, property, attribute, data member.
    Field,
    /// Variable, static variable.
    Variable,
    /// Constant.
    Constant,
    /// Function or method parameter, `this`/`self` parameter.
    Parameter,
    /// Type alias, type, associated type.
    Type,
    /// Type parameter / generic.
    TypeParameter,
    /// Module, namespace, package, file.
    Module,
    /// Macro.
    Macro,
    /// Anything else SCIP defines.
    Other,
}

impl SymbolKind {
    /// Short lowercase label for output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::Class => "class",
            Self::Struct => "struct",
            Self::Interface => "interface",
            Self::Enum => "enum",
            Self::EnumMember => "enum_member",
            Self::Function => "function",
            Self::Method => "method",
            Self::Constructor => "constructor",
            Self::Field => "field",
            Self::Variable => "variable",
            Self::Constant => "constant",
            Self::Parameter => "parameter",
            Self::Type => "type",
            Self::TypeParameter => "type_parameter",
            Self::Module => "module",
            Self::Macro => "macro",
            Self::Other => "other",
        }
    }
}

/// A relationship from one symbol to another, as the indexer reported it.
///
/// `symbol` is the SCIP moniker of the *target*. Several flags can be set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relationship {
    /// SCIP symbol string of the related symbol.
    pub symbol: String,
    /// The owner is a reference to the target (used for things like interface-to-implementation hover).
    pub is_reference: bool,
    /// The owner implements the target.
    pub is_implementation: bool,
    /// The target is the type definition of the owner.
    pub is_type_definition: bool,
    /// The target is the definition of the owner (for example an overridden method).
    pub is_definition: bool,
}

/// A symbol known to the index (defined in a document or external).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreciseSymbol {
    /// The SCIP moniker. For `local N` symbols this is only unique together with `scope`.
    pub scip_symbol: String,
    /// For `local` symbols: the document they belong to. `None` for global symbols.
    pub scope: Option<RepoPath>,
    /// Human-facing name: the indexer's display name, or the last descriptor of the moniker.
    pub display_name: String,
    /// Broad kind.
    pub kind: SymbolKind,
    /// Documentation paragraphs joined by a blank line and truncated to the
    /// configured limit; empty when the indexer gave none.
    pub documentation: String,
    /// Relationships to other symbols.
    pub relationships: Vec<Relationship>,
}

impl PreciseSymbol {
    /// Parses the moniker into its structured form.
    pub fn parse(&self) -> Result<ScipSymbol, crate::symbol::SymbolParseError> {
        ScipSymbol::parse(&self.scip_symbol)
    }

    /// Whether this symbol is document-local.
    pub fn is_local(&self) -> bool {
        self.scope.is_some()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn three_element_range_is_single_line() {
        let s = Span::from_scip(&[4, 2, 9]).unwrap();
        assert_eq!(
            s,
            Span {
                start_line: 4,
                start_character: 2,
                end_line: 4,
                end_character: 9
            }
        );
        let lr = s.line_range().unwrap();
        assert_eq!((lr.start(), lr.end()), (5, 5));
    }

    #[test]
    fn four_element_range_is_multi_line() {
        let s = Span::from_scip(&[0, 0, 10, 1]).unwrap();
        let lr = s.line_range().unwrap();
        assert_eq!((lr.start(), lr.end()), (1, 11));
    }

    #[test]
    fn invalid_ranges() {
        assert_eq!(Span::from_scip(&[]), Err(RangeError::BadLength(0)));
        assert_eq!(Span::from_scip(&[1, 2]), Err(RangeError::BadLength(2)));
        assert_eq!(
            Span::from_scip(&[1, 2, 3, 4, 5]),
            Err(RangeError::BadLength(5))
        );
        assert_eq!(Span::from_scip(&[-1, 0, 1]), Err(RangeError::Negative));
        assert_eq!(Span::from_scip(&[0, -1, 1]), Err(RangeError::Negative));
        assert_eq!(Span::from_scip(&[0, 0, 1, -5]), Err(RangeError::Negative));
        assert_eq!(Span::from_scip(&[0, 5, 3]), Err(RangeError::Reversed));
        assert_eq!(Span::from_scip(&[3, 0, 2, 9]), Err(RangeError::Reversed));
        assert_eq!(Span::from_scip(&[3, 5, 3, 4]), Err(RangeError::Reversed));
    }

    #[test]
    fn extreme_values_do_not_overflow() {
        let s = Span::from_scip(&[i32::MAX, 0, i32::MAX, i32::MAX]).unwrap();
        let lr = s.line_range().unwrap();
        assert_eq!(lr.start(), 2_147_483_648);
        assert!(Span::from_scip(&[i32::MIN, 0, 1]).is_err());
    }

    #[test]
    fn contains_is_end_exclusive() {
        let s = Span::from_scip(&[2, 4, 8]).unwrap();
        assert!(s.contains(2, 4));
        assert!(s.contains(2, 7));
        assert!(!s.contains(2, 8));
        assert!(!s.contains(2, 3));
        assert!(!s.contains(3, 5));
        let multi = Span::from_scip(&[1, 5, 3, 2]).unwrap();
        assert!(multi.contains(2, 100));
        assert!(!multi.contains(3, 2));
    }

    #[test]
    fn primary_role_priority() {
        assert_eq!(SymbolRoles(0).primary(), OccurrenceRole::Reference);
        assert_eq!(SymbolRoles(1 | 8).primary(), OccurrenceRole::Definition);
        assert_eq!(SymbolRoles(64).primary(), OccurrenceRole::ForwardDefinition);
        assert_eq!(SymbolRoles(2 | 8).primary(), OccurrenceRole::Import);
        assert_eq!(SymbolRoles(4 | 8).primary(), OccurrenceRole::WriteAccess);
        assert_eq!(SymbolRoles(8).primary(), OccurrenceRole::ReadAccess);
        assert!(SymbolRoles(16 | 32).is_generated());
        assert!(SymbolRoles(32).is_test());
    }
}
