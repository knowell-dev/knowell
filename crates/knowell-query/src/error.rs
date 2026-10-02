use knowell_core::Name;

/// Errors for invalid query input, scope or configuration.
///
/// A failing or missing candidate source is *not* an error: the engine answers
/// from the sources that work and reports the gap as a
/// [`Degradation`](crate::Degradation) in the response.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    /// A view identifier is empty, too long or contains control characters.
    #[error("view id must be 1-200 characters without control characters")]
    InvalidViewId,
    /// A commit id is not a full lowercase hex id.
    #[error("commit id must be a full 40- or 64-digit lowercase hex id")]
    InvalidCommit,
    /// A language name is empty, too long or uses characters outside the allowed set.
    #[error("language must be 1-64 characters from a-z, 0-9, `+`, `#`, `.`, `-` or `_`")]
    InvalidLanguage,
    /// A path glob cannot be parsed.
    #[error("path glob `{pattern}` is invalid: {reason}")]
    InvalidGlob {
        /// The rejected pattern, truncated to 120 characters.
        pattern: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// A glossary entry is empty, too long or contains control characters.
    #[error("glossary entry `{term}` is invalid: {reason}")]
    InvalidGlossaryEntry {
        /// The entry's term, truncated to 120 characters.
        term: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// The scope names a different workspace than its view manifest.
    #[error("scope workspace `{scope}` does not match the view manifest workspace `{manifest}`")]
    WorkspaceMismatch {
        /// Workspace named by the scope.
        scope: Name,
        /// Workspace the manifest was pinned for.
        manifest: Name,
    },
    /// The plan was made (and its glossary applied) for a different business
    /// domain than the scope searches.
    #[error("the plan was made for domain `{plan}` but the scope searches domain `{scope}`")]
    DomainMismatch {
        /// Domain of the plan.
        plan: Name,
        /// Domain of the scope.
        scope: Name,
    },
    /// A view manifest entry is inconsistent.
    #[error("view manifest entry for project `{project}` is invalid: {reason}")]
    InvalidManifest {
        /// The project whose pin is inconsistent.
        project: Name,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// A configuration value is out of range.
    #[error("configuration `{field}` is invalid: {reason}")]
    InvalidConfig {
        /// Dotted name of the offending field, e.g. `fusion.result_limit`.
        field: &'static str,
        /// The accepted range or rule.
        reason: &'static str,
    },
}

/// Truncates user-supplied text echoed in an error message.
pub(crate) fn echo(text: &str) -> String {
    const MAX: usize = 120;
    if text.chars().count() <= MAX {
        text.to_owned()
    } else {
        let mut out: String = text.chars().take(MAX).collect();
        out.push('…');
        out
    }
}
