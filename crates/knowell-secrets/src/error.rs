//! Error type of the crate. No variant ever carries a secret value or file
//! contents; references (`env:NAME`, `file:/path`) are configuration, not
//! secrets, and are safe to show.

use thiserror::Error;

/// Errors from exclusion configuration and secret resolution.
#[derive(Debug, Error)]
pub enum SecretsError {
    /// A user exclusion glob could not be compiled.
    #[error("invalid exclusion pattern `{pattern}`: {reason}")]
    InvalidPattern {
        /// The offending pattern as configured.
        pattern: String,
        /// Why it was rejected (glob syntax problem).
        reason: String,
    },
    /// The environment variable is not set.
    #[error("secret reference `{reference}` is not set in the environment")]
    EnvNotSet {
        /// The reference, e.g. `env:NAME`.
        reference: String,
    },
    /// The environment variable is set but is not valid unicode.
    #[error("secret reference `{reference}` is not valid unicode")]
    EnvNotUnicode {
        /// The reference, e.g. `env:NAME`.
        reference: String,
    },
    /// The secret resolved to an empty value.
    #[error("secret reference `{reference}` resolved to an empty value")]
    Empty {
        /// The reference.
        reference: String,
    },
    /// The secret file could not be read.
    #[error("secret reference `{reference}` could not be read ({kind})")]
    FileUnreadable {
        /// The reference, e.g. `file:/run/secrets/key`.
        reference: String,
        /// The `std::io::ErrorKind` name, e.g. `NotFound`.
        kind: String,
    },
    /// The secret file is not valid UTF-8.
    #[error("secret reference `{reference}` is not valid UTF-8")]
    FileNotUtf8 {
        /// The reference.
        reference: String,
    },
}
