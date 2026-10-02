//! Error type of the crate.

use knowell_core::Name;
use knowell_graph::GraphError;

/// Everything that can go wrong while loading rule packs, linking and
/// checking. Messages name packs, rules, files and identifiers only; they
/// never contain file contents or values read from configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    /// A pack file could not be read.
    #[error("pack `{pack}`: cannot read `{path}`: {message}")]
    PackIo {
        /// Pack directory or name.
        pack: String,
        /// File that failed.
        path: String,
        /// Operating-system error text.
        message: String,
    },
    /// `pack.toml` is not valid TOML or does not match the pack schema.
    #[error("pack `{pack}`: invalid pack.toml: {message}")]
    PackManifest {
        /// Pack directory or name.
        pack: String,
        /// Parser diagnostic (line/column and the offending key).
        message: String,
    },
    /// The pack is well-formed TOML but semantically invalid.
    #[error("pack `{pack}`: {message}")]
    InvalidPack {
        /// Pack name.
        pack: String,
        /// What is wrong and how to fix it.
        message: String,
    },
    /// A rule, binding or extractor entry of a pack is invalid.
    #[error("pack `{pack}` entry `{entry}`: {message}")]
    InvalidRule {
        /// Pack name.
        pack: String,
        /// Rule, binding or extractor id.
        entry: String,
        /// What is wrong and how to fix it.
        message: String,
    },
    /// A tree-sitter query does not compile against a grammar.
    #[error(
        "pack `{pack}` entry `{entry}`: query `{query}` does not compile for {language}: {message}"
    )]
    QueryCompile {
        /// Pack name.
        pack: String,
        /// Rule or binding id.
        entry: String,
        /// Query file, relative to the pack directory.
        query: String,
        /// Language the query was compiled for.
        language: String,
        /// tree-sitter diagnostic (row, column, kind).
        message: String,
    },
    /// Two packs share a name.
    #[error("pack `{0}` is loaded twice; pack names must be unique")]
    DuplicatePack(String),
    /// A project appears twice in the input of the linker or the checks.
    #[error("project `{0}` appears more than once; pass each project once")]
    DuplicateProject(Name),
    /// A generation is missing for a project that has link output.
    #[error("no view generation given for project `{0}`")]
    MissingGeneration(Name),
    /// Building or updating the graph failed.
    #[error("graph: {0}")]
    Graph(#[from] GraphError),
}
