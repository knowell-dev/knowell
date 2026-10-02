//! Lexical search: a code-aware tokenizer and a Tantivy BM25 index.
//!
//! # Design
//!
//! * **Own tokenizer, pre-tokenized documents.** Instead of registering a
//!   Tantivy `Tokenizer`, [`tokenize`] runs in this crate and its output is fed
//!   to Tantivy as `PreTokenizedString` values. The query side uses the very
//!   same function, so index-time and query-time tokenization cannot drift
//!   apart, and splitting rules (camelCase, snake_case, Turkish folding) live
//!   in one plain, unit-tested function.
//! * **No query parser.** User queries are free text and routinely contain
//!   `:`, quotes and parentheses (code, stack traces, questions). They are
//!   tokenized and turned into `TermQuery` / `BooleanQuery` objects by hand, so
//!   no input can produce a query-syntax error or change query semantics.
//! * **Determinism.** One indexing thread keeps the segment layout stable,
//!   readers reload manually on [`LexicalWriter::commit`], and
//!   [`LexicalIndex::search`] sorts by score descending then id ascending,
//!   widening its fetch so the cut at `limit` never depends on document order.
//! * **Small index.** Only the document key and path are stored; bodies are
//!   indexed (term frequencies, no positions) but never stored.
//!
//! # Example
//!
//! ```
//! use knowell_lexical::{LexicalDoc, LexicalIndex};
//!
//! # fn main() -> Result<(), knowell_lexical::LexicalError> {
//! let index = LexicalIndex::create_in_ram()?;
//! let mut writer = index.writer()?;
//! writer.add(LexicalDoc {
//!     id: "billing/subscription.ts",
//!     path: "billing/subscription.ts",
//!     text: "async cancelSubscription(id) {}",
//! })?;
//! writer.commit()?;
//! let hits = index.search("cancel subscription", 5)?;
//! assert_eq!(hits.first().map(|h| h.id.as_str()), Some("billing/subscription.ts"));
//! # Ok(())
//! # }
//! ```

mod index;
pub mod tokenize;

pub use index::{
    DEFAULT_COORDINATION, LexicalDoc, LexicalError, LexicalHit, LexicalIndex, LexicalWriter,
    PATH_BOOST, SearchOptions,
};
pub use tokenize::{Token, tokenize, tokenize_query};
