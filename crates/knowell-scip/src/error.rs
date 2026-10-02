//! Errors for reading SCIP indexes.

/// Why a SCIP index could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ScipError {
    /// The index file could not be opened or read.
    #[error("cannot read scip index `{path}`: {source}")]
    Io {
        /// The file that failed.
        path: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The encoded index is larger than the configured limit.
    #[error("scip index is {size} bytes, over the limit of {limit} bytes")]
    TooLarge {
        /// Observed size in bytes.
        size: u64,
        /// Configured limit in bytes.
        limit: u64,
    },
    /// The bytes are not a valid SCIP protobuf message.
    #[error("scip index is not a valid protobuf message: {0}")]
    Decode(String),
    /// A count in the index exceeds the configured limit.
    #[error("scip index has more than {limit} {what}")]
    LimitExceeded {
        /// What was counted.
        what: &'static str,
        /// The configured limit.
        limit: usize,
    },
}
