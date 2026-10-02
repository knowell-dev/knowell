use std::io;
use std::path::PathBuf;

/// Errors returned by workspace import, client connection and CI generation.
///
/// Messages never quote file contents: configuration files may hold secrets
/// and parser errors can echo values.
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    /// A file or directory could not be read.
    #[error("cannot read `{}`: {source}", path.display())]
    Read {
        /// The path that failed.
        path: PathBuf,
        /// Underlying error.
        source: io::Error,
    },
    /// A file or directory could not be written, backed up or removed.
    #[error("cannot write `{}`: {source}", path.display())]
    Write {
        /// The path that failed.
        path: PathBuf,
        /// Underlying error.
        source: io::Error,
    },
    /// An existing JSON file is not valid JSON (or not a JSON object).
    #[error(
        "`{}` is not a valid JSON object (line {line}, column {column}); fix it first, nothing was changed",
        path.display()
    )]
    InvalidJson {
        /// The offending file.
        path: PathBuf,
        /// 1-based line of the problem (0 when unknown).
        line: usize,
        /// 1-based column of the problem (0 when unknown).
        column: usize,
    },
    /// An existing TOML file is not valid TOML.
    #[error("`{}` is not valid TOML; fix it first, nothing was changed", path.display())]
    InvalidToml {
        /// The offending file.
        path: PathBuf,
    },
    /// The file already contains something Knowell must not overwrite.
    #[error("`{}`: {what}; resolve it by hand, nothing was changed", path.display())]
    Conflict {
        /// The file in conflict.
        path: PathBuf,
        /// What is in the way.
        what: String,
    },
    /// The caller supplied an unusable option.
    #[error("invalid input: {0}")]
    InvalidInput(String),
}
