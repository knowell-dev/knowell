//! Code analysis for Knowell: language detection, tree-sitter parsing,
//! symbol and import extraction, semantic chunking, embedding inputs and
//! signature skeletons.
//!
//! The pipeline for one file version:
//!
//! 1. [`parse`] detects the [`Language`] and its [`Tier`], parses with the
//!    bundled tree-sitter grammar (bounded by [`ParseLimits`]) and returns a
//!    [`ParsedFile`]: [`Symbol`]s, [`Import`]s, SQL [`Block`]s,
//!    generated-file and syntax-error flags, and a [`Degradation`] when
//!    analysis was skipped or is partial.
//! 2. [`chunks`] cuts the text into meaningful units ([`Chunk`]).
//! 3. [`prepared_input`] builds the exact embedding input of a chunk and its
//!    cache hash, which covers [`PARSER_VERSION`] and
//!    [`PREPARED_FORMAT_VERSION`].
//! 4. [`skeleton`] renders declarations with bodies elided for context
//!    packing.
//!
//! Everything here processes untrusted repository content: no function
//! panics on any input, input size and parse time are bounded, deeply
//! nested input is rejected before it reaches the parser, and syntax errors
//! are tolerated. Output is deterministic.
//!
//! ```
//! use knowell_core::RepoPath;
//! use knowell_parse::{ChunkContext, ChunkOptions, chunks, parse, prepared_input};
//!
//! let path = RepoPath::new("src/billing.py").unwrap();
//! let text = "def cancel(subscription_id: str) -> None:\n    \"\"\"Cancels it.\"\"\"\n";
//! let parsed = parse(&path, text);
//! assert_eq!(parsed.symbols[0].qualified_name, "cancel");
//! let chunks = chunks(&parsed, text, &ChunkOptions::default()).unwrap();
//! let input = prepared_input(&chunks[0], &ChunkContext::for_chunk("billing", &parsed, &chunks[0]));
//! assert!(input.text.starts_with("path: src/billing.py\n"));
//! ```

mod chunk;
mod dockerfile;
mod extract;
mod generated;
mod grammar;
mod language;
mod markdown;
mod model;
mod parse;
mod prepare;
mod skeleton;
mod sql;
mod structured;
mod text;

use knowell_core::RepoPath;

pub use chunk::{Chunk, ChunkKind, ChunkOptions, chunks};
pub use language::{Dialect, Language, Tier, UnknownLanguage};
pub use model::{
    Block, BlockKind, Degradation, Import, ParseLimits, ParsedFile, Symbol, SymbolKind, Visibility,
};
pub use parse::{parse, parse_tree, parse_with, ts_language};
pub use prepare::{ChunkContext, PREPARED_FORMAT_VERSION, PreparedInput, prepared_input};
pub use skeleton::skeleton;
/// The tree-sitter version Knowell's grammars are built for; dependents that
/// run their own queries ([`ts_language`], [`parse_tree`]) use this re-export.
pub use tree_sitter;

/// Version of the analysis output (symbols, imports, blocks, chunks,
/// skeletons). Bump it whenever any of them can change for the same input —
/// a grammar or query update, a chunking rule, a normalisation — so that
/// stored analysis and embedding-cache entries keyed on it are invalidated.
pub const PARSER_VERSION: u32 = 1;

/// Errors returned by the functions that take a [`ParsedFile`] together with
/// its text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// The text is not the text the [`ParsedFile`] was produced from.
    #[error("text does not match the parsed file `{path}`; parse this exact text first")]
    TextMismatch {
        /// The parsed file's path.
        path: RepoPath,
    },
}
