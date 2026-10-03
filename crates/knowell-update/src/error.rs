//! Sanitized updater errors: untrusted metadata and transport details are never echoed.

/// Errors from release selection, verification, and installation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An operating-system operation failed, without including its path or input.
    #[error("update filesystem operation failed; check permissions and free space")]
    Io(#[from] std::io::Error),
    /// Persisted installation or metadata state is invalid.
    #[error("invalid update state: {0}")]
    State(&'static str),
    /// Another operation or a running engine holds an admission lock.
    #[error("update is busy; finish the active operation and retry")]
    Busy,
    /// A transaction was interrupted and needs explicit recovery.
    #[error("update recovery is required; inspect the pending transaction before retrying")]
    RecoveryRequired,
    /// This installation is not demonstrably owned by the direct installer.
    #[error("installation ownership is unknown; use the original installation method")]
    Ownership,
    /// Downloaded or persisted artifact identity differs from the trusted identity.
    #[error("update integrity verification failed; nothing may be activated")]
    Integrity,
    /// A candidate cannot safely consume the installation's current data formats.
    #[error("update compatibility check failed: {0}")]
    Compatibility(&'static str),
    /// No real production or explicitly operator-provided trust root exists.
    #[error("update trust is not configured; provision a trusted root before checking releases")]
    TrustUnconfigured,
    /// A TUF operation failed. Its inner error can contain rejected URLs or values.
    #[error(
        "trusted update metadata could not be verified; check connectivity, expiry, and trust configuration"
    )]
    Metadata,
    /// A malformed or unsupported signed release manifest was encountered.
    #[error("invalid signed release manifest; the publisher must correct its metadata")]
    Manifest,
    /// A repository URL or transport scheme violates policy.
    #[error(
        "invalid update source; use credential-free https urls or an explicit local offline repository"
    )]
    Source,
    /// An explicit requested version or platform has no eligible signed target.
    #[error("requested release is unavailable for this channel and platform")]
    ReleaseUnavailable,
    /// The selected release has been withdrawn by the publisher.
    #[error("requested release has been revoked; select a supported release")]
    Revoked,
    /// An older binary was requested without explicit downgrade authorization.
    #[error(
        "requested release is older; an explicit pinned version and downgrade authorization are required"
    )]
    Downgrade,
    /// Development builds are not eligible for implicit release replacement.
    #[error("development builds require an explicit release version; no release is inferred")]
    Development,
    /// The public release policy prohibits prereleases until stable 1.0 exists.
    #[error("preview releases are unavailable until stable 1.0 is published for this platform")]
    PreviewUnavailable,
    /// A bounded repository or artifact transfer exceeded its resource limit.
    #[error("update download exceeded its byte or time limit")]
    Limit,
}

/// Result type for updater operations.
pub type Result<T> = std::result::Result<T, Error>;
