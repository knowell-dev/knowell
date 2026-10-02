//! Public data produced by [`crate::parse`].

use std::ops::Range;
use std::time::Duration;

use knowell_core::{ContentHash, LineRange, RepoPath};
use serde::{Deserialize, Serialize};

use crate::language::{Dialect, Language, Tier};

/// What a [`Symbol`] declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    /// Free function (also arrow functions bound to module-level names).
    Function,
    /// Function declared in a class-like container, a Go method, or a C++
    /// out-of-line member definition.
    Method,
    /// Constructor / initialiser.
    Constructor,
    /// Class, record, data class, singleton object.
    Class,
    /// Struct or union.
    Struct,
    /// Enumeration (also protobuf `enum`).
    Enum,
    /// Interface or protocol.
    Interface,
    /// Trait or mixin.
    Trait,
    /// Rust `impl` block; Swift / Dart extension. Named after the extended type.
    Impl,
    /// Type alias / typedef.
    TypeAlias,
    /// Module-level constant (and `UPPER_CASE` module-level variables).
    Constant,
    /// Module-level variable.
    Variable,
    /// Module or namespace.
    Module,
    /// Field or property of a type (also protobuf fields).
    Field,
    /// Macro (`macro_rules!`, `#define`).
    Macro,
    /// Test block (`describe` / `it` / `test` calls in JavaScript test files).
    Test,
    /// HTTP or messaging operation (`GET /subscriptions/{id}`).
    Endpoint,
    /// API schema (OpenAPI `components.schemas` entry).
    Schema,
    /// AsyncAPI channel.
    Channel,
    /// SQL table.
    Table,
    /// SQL column.
    Column,
    /// SQL view.
    View,
    /// SQL index.
    Index,
    /// Protobuf service, Compose service.
    Service,
    /// Protobuf RPC.
    Rpc,
    /// Protobuf message.
    Message,
    /// Markdown heading; its range is the whole section.
    Heading,
    /// Dockerfile build stage (`FROM … AS name`).
    Stage,
    /// TOML table (`[section]`).
    Section,
    /// YAML / JSON / TOML key.
    Key,
    /// Kubernetes resource (`Deployment/web`).
    Resource,
    /// CSS rule set or at-rule.
    Rule,
}

impl SymbolKind {
    /// Stable lowercase identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Constructor => "constructor",
            SymbolKind::Class => "class",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Interface => "interface",
            SymbolKind::Trait => "trait",
            SymbolKind::Impl => "impl",
            SymbolKind::TypeAlias => "type_alias",
            SymbolKind::Constant => "constant",
            SymbolKind::Variable => "variable",
            SymbolKind::Module => "module",
            SymbolKind::Field => "field",
            SymbolKind::Macro => "macro",
            SymbolKind::Test => "test",
            SymbolKind::Endpoint => "endpoint",
            SymbolKind::Schema => "schema",
            SymbolKind::Channel => "channel",
            SymbolKind::Table => "table",
            SymbolKind::Column => "column",
            SymbolKind::View => "view",
            SymbolKind::Index => "index",
            SymbolKind::Service => "service",
            SymbolKind::Rpc => "rpc",
            SymbolKind::Message => "message",
            SymbolKind::Heading => "heading",
            SymbolKind::Stage => "stage",
            SymbolKind::Section => "section",
            SymbolKind::Key => "key",
            SymbolKind::Resource => "resource",
            SymbolKind::Rule => "rule",
        }
    }

    /// Parses a query capture suffix (`@definition.<kind>`).
    pub(crate) fn from_capture(name: &str) -> Option<SymbolKind> {
        Some(match name {
            "function" => SymbolKind::Function,
            "method" => SymbolKind::Method,
            "constructor" => SymbolKind::Constructor,
            "class" => SymbolKind::Class,
            "struct" => SymbolKind::Struct,
            "enum" => SymbolKind::Enum,
            "interface" => SymbolKind::Interface,
            "trait" => SymbolKind::Trait,
            "impl" => SymbolKind::Impl,
            "type_alias" => SymbolKind::TypeAlias,
            "constant" => SymbolKind::Constant,
            "variable" => SymbolKind::Variable,
            "module" => SymbolKind::Module,
            "field" => SymbolKind::Field,
            "macro" => SymbolKind::Macro,
            "test" => SymbolKind::Test,
            "table" => SymbolKind::Table,
            "column" => SymbolKind::Column,
            "view" => SymbolKind::View,
            "index" => SymbolKind::Index,
            "service" => SymbolKind::Service,
            "rpc" => SymbolKind::Rpc,
            "message" => SymbolKind::Message,
            "rule" => SymbolKind::Rule,
            _ => return None,
        })
    }

    /// Whether symbols of this kind can contain other symbols (and so
    /// contribute to qualified names).
    pub(crate) fn is_container(self) -> bool {
        !matches!(
            self,
            SymbolKind::TypeAlias
                | SymbolKind::Constant
                | SymbolKind::Variable
                | SymbolKind::Field
                | SymbolKind::Macro
                | SymbolKind::Column
                | SymbolKind::Index
                | SymbolKind::View
                | SymbolKind::Rpc
                // AsyncAPI operation names already carry their channel.
                | SymbolKind::Channel
        )
    }

    /// Whether a function nested in this kind is a method.
    pub(crate) fn is_class_like(self) -> bool {
        matches!(
            self,
            SymbolKind::Class
                | SymbolKind::Struct
                | SymbolKind::Enum
                | SymbolKind::Interface
                | SymbolKind::Trait
                | SymbolKind::Impl
        )
    }

    /// Separator placed before this kind's segment in a qualified name.
    pub(crate) fn separator(self) -> &'static str {
        match self {
            SymbolKind::Heading | SymbolKind::Test => " > ",
            SymbolKind::Rule => " ",
            _ => ".",
        }
    }
}

/// Declared visibility, normalised across languages.
///
/// `None` on a symbol means the language has no such notion for it (Ruby,
/// SQL, Markdown, …) or it cannot be determined syntactically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Visible to every consumer (`pub`, `public`, `export`, Go capitalised).
    Public,
    /// Visible to subclasses.
    Protected,
    /// Visible within the crate / package / assembly / module
    /// (`pub(crate)`, Java package-private, C# and Kotlin `internal`).
    Internal,
    /// Visible only to the declaring scope (`private`, `_name`, `static` C
    /// functions, non-exported TypeScript module members).
    Private,
}

/// A declaration found in a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    /// Declared name (`cancelSubscription`, `GET /subscriptions/{id}`,
    /// `Setup`).
    pub name: String,
    /// Container path within the file: enclosing symbols' names joined with
    /// `.` (`SubscriptionService.cancelSubscription`); Markdown headings and
    /// JavaScript test blocks join with ` > `, CSS rules with a space. Go
    /// receivers and C++ / SQL qualifiers are included
    /// (`Service.Cancel`, `subscriptions.cancelled_at`). Package / file-level
    /// namespaces are included only when they enclose the code (block
    /// namespaces, C# / PHP file-scoped namespaces).
    pub qualified_name: String,
    /// What the symbol declares.
    pub kind: SymbolKind,
    /// Lines of the whole declaration, including leading doc comments and
    /// attributes / decorators (like LSP's `DocumentSymbol.range`).
    pub range: LineRange,
    /// Byte range matching [`Symbol::range`], trailing whitespace excluded.
    pub byte_range: Range<usize>,
    /// 1-based line of the symbol's name.
    pub name_line: u32,
    /// Declaration without body and without doc comments, whitespace
    /// normalised (`pub fn cancel(&mut self, reason: &str) -> Result<(), Error>`),
    /// bounded to 12 lines / 600 bytes.
    pub signature: String,
    /// Doc comment / docstring text with comment markers removed, bounded to
    /// 2000 bytes.
    pub doc: Option<String>,
    /// Declared visibility, when the language has the notion.
    pub visibility: Option<Visibility>,
    /// Index (into [`ParsedFile::symbols`]) of the enclosing symbol.
    pub parent: Option<usize>,
    /// Whether the declaration has a body that skeletons elide (`false` for
    /// fields, constants, prototypes and interface method signatures).
    pub has_body: bool,
}

/// An import / include / use / require.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Import {
    /// The imported module or path as written, quotes removed
    /// (`@nestjs/common`, `std::collections::HashMap`, `stdio.h`).
    pub specifier: String,
    /// Lines of the import statement.
    pub range: LineRange,
}

/// Kind of a [`Block`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    /// One SQL statement (DDL, DML or migration step).
    Statement,
}

/// A statement-level unit that bounds chunks without being a declaration
/// (SQL statements). Files whose blocks are non-empty are chunked by block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    /// What the block is.
    pub kind: BlockKind,
    /// Lines of the block, including directly preceding comments.
    pub range: LineRange,
    /// Byte range matching [`Block::range`] (includes the terminating `;`).
    pub byte_range: Range<usize>,
    /// First line of the statement, whitespace normalised, bounded to 200 bytes.
    pub label: String,
    /// The object the statement is about (table, view, …), if any.
    pub subject: Option<String>,
}

/// Why structural analysis of a file was skipped or is incomplete.
///
/// A degraded file is never presented as fully analysed: the reason travels
/// with the [`ParsedFile`] (ARCHITECTURE principle 2, "no silent fallback").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Degradation {
    /// The file exceeds [`ParseLimits::max_bytes`]: not parsed and not chunked.
    TooLarge {
        /// File size in bytes.
        bytes: usize,
        /// The limit in bytes.
        limit: usize,
    },
    /// Minified / single-line bundle: not parsed, chunked as text.
    Minified,
    /// Bracket or indentation nesting exceeds [`ParseLimits::max_nesting`]:
    /// not parsed, chunked as text.
    TooDeep {
        /// Observed nesting depth (approximate, saturates just above the limit).
        depth: usize,
        /// The limit.
        limit: usize,
    },
    /// Parsing or extraction exceeded [`ParseLimits::timeout`]. Symbols found
    /// before the deadline are kept.
    Timeout,
    /// Parsing was cancelled by the caller.
    Cancelled,
    /// Symbols beyond a limit were dropped: [`ParseLimits::max_symbols`], or
    /// the query engine's bound on simultaneously open matches (4096) on a
    /// pathological tree.
    Truncated {
        /// The limit that was hit.
        limit: usize,
    },
    /// The grammar or one of its queries failed to load (an internal bug).
    GrammarError {
        /// Diagnostic message.
        message: String,
    },
}

/// Resource bounds for parsing untrusted repository content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParseLimits {
    /// Files larger than this are neither parsed nor chunked. Default 2 MiB.
    pub max_bytes: usize,
    /// Wall-clock budget for parsing plus extraction. Default 5 s.
    pub timeout: Duration,
    /// Maximum bracket nesting depth (and indentation width / 8) accepted
    /// before tree-sitter is invoked. Default 1024.
    pub max_nesting: usize,
    /// Maximum symbols kept per file. Default 20 000.
    pub max_symbols: usize,
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self {
            max_bytes: 2 * 1024 * 1024,
            timeout: Duration::from_secs(5),
            max_nesting: 1024,
            max_symbols: 20_000,
        }
    }
}

/// The result of analysing one file version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedFile {
    /// Repository-relative path.
    pub path: RepoPath,
    /// Detected language.
    pub language: Language,
    /// The language's analysis tier.
    pub tier: Tier,
    /// Schema flavour of a structure file (OpenAPI, Compose, …).
    pub dialect: Option<Dialect>,
    /// Declarations, ordered by start offset (containers before their
    /// members).
    pub symbols: Vec<Symbol>,
    /// Imports in source order.
    pub imports: Vec<Import>,
    /// Statement-level units (SQL); empty for other languages.
    pub blocks: Vec<Block>,
    /// Generator banner, lockfile or generated-file name detected.
    pub is_generated: bool,
    /// The syntax tree contains `ERROR` or `MISSING` nodes.
    pub has_errors: bool,
    /// Why analysis was skipped or is incomplete, if it was.
    pub degraded: Option<Degradation>,
    /// Hash of the analysed text; [`crate::chunks`] and [`crate::skeleton`]
    /// verify that they are given the same text.
    pub content_hash: ContentHash,
    /// Text length in bytes.
    pub byte_len: usize,
    /// Number of lines (0 for an empty file).
    pub line_count: u32,
}
