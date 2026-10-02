//! SCIP ingestion: runs or reads language indexers' SCIP output and maps
//! precise definitions, references and implementations to Knowell symbols.
//!
//! * [`read_index_file`] / [`read_index_bytes`] decode an untrusted SCIP
//!   protobuf under [`Limits`] and produce a [`ScipIndex`] with cross-file
//!   queries and a freshness contract ([`ScipIndex::covers`]).
//! * [`ScipSymbol`] parses SCIP symbol strings into structured form.
//! * [`IndexerRegistry`] knows which indexer fits a project and
//!   [`run_indexer`] runs it with a timeout; a missing tool is an explicit
//!   [`IndexerError::NotInstalled`], never a silent fallback.
//!
//! Everything here is *precise* evidence (compiler-grade resolution done by
//! the language's own indexer), as opposed to the syntactic evidence from
//! tree-sitter. See the crate README for how callers label the two.

mod error;
mod index;
mod model;
mod registry;
mod run;
pub mod symbol;

pub use error::ScipError;
pub use index::{
    Freshness, IndexMetadata, IngestReport, Limits, RejectedDocument, ScipIndex, read_index_bytes,
    read_index_file,
};
pub use model::{
    Document, Located, Occurrence, OccurrenceRole, PositionEncoding, PreciseSymbol, RangeError,
    Relationship, Span, SymbolId, SymbolKind, SymbolRoles,
};
pub use registry::{
    ArgsFn, BUILTIN, Detection, IndexerRegistry, IndexerSpec, Language, Marker, find_on_path,
    which_in,
};
pub use run::{IndexerError, IndexerRun, STDERR_TAIL_BYTES, run_indexer};
pub use symbol::{Descriptor, DescriptorKind, ScipSymbol, SymbolPackage, SymbolParseError};
