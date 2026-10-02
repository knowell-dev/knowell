//! The engine's error type and its mappings to the MCP and REST error
//! channels.

use knowell_index::IndexError;
use knowell_mcp::ToolError;
use knowell_query::QueryError;
use knowell_store::StoreError;

/// Failures of engine construction and of operations that are not tool
/// calls. Messages are lowercase and never contain secret values (store and
/// index errors are already scrubbed by their crates).
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// A database operation failed.
    #[error("store: {0}")]
    Store(#[from] StoreError),
    /// The indexer failed.
    #[error("index: {0}")]
    Index(#[from] IndexError),
    /// The query pipeline rejected its input or configuration.
    #[error("query: {0}")]
    Query(#[from] QueryError),
    /// The engine configuration is unusable.
    #[error("configuration: {0}")]
    Config(String),
    /// The request is malformed or contradictory.
    #[error("invalid request: {0}")]
    Invalid(String),
    /// The addressed item does not exist (or is not visible).
    #[error("not found: {0}")]
    NotFound(String),
    /// The caller may not do this.
    #[error("permission denied: {0}")]
    Forbidden(String),
    /// Something the engine cannot answer right now.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// Anything else; the message is for logs.
    #[error("internal: {0}")]
    Internal(String),
}

impl EngineError {
    /// Shorthand for [`EngineError::Internal`].
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }
}

impl From<EngineError> for ToolError {
    fn from(error: EngineError) -> Self {
        match error {
            EngineError::Invalid(message) => ToolError::invalid_input(message),
            EngineError::NotFound(message) => ToolError::not_found(message),
            EngineError::Forbidden(message) => ToolError::permission_denied(message),
            EngineError::Unavailable(message) => ToolError::not_ready(message, Some(5_000)),
            EngineError::Query(QueryError::DomainMismatch { .. }) => {
                ToolError::invalid_input(error.to_string())
            }
            other => ToolError::internal(other.to_string()),
        }
    }
}

impl From<EngineError> for knowell_server::EngineError {
    fn from(error: EngineError) -> Self {
        match error {
            EngineError::Invalid(message) => Self::Invalid { message },
            EngineError::NotFound(what) => Self::NotFound { what },
            EngineError::Forbidden(message) => Self::Forbidden { message },
            EngineError::Unavailable(reason) => Self::Unavailable { reason },
            other => Self::Internal {
                message: other.to_string(),
            },
        }
    }
}

/// Converts a tool error into the REST error channel, keeping the class.
pub(crate) fn tool_to_rest(error: ToolError) -> knowell_server::EngineError {
    match error {
        ToolError::InvalidInput(message) => knowell_server::EngineError::Invalid { message },
        ToolError::NotFound(what) => knowell_server::EngineError::NotFound { what },
        ToolError::PermissionDenied(message) => knowell_server::EngineError::Forbidden { message },
        ToolError::NotReady { message, .. } | ToolError::Stale(message) => {
            knowell_server::EngineError::Unavailable { reason: message }
        }
        ToolError::Internal(message) => knowell_server::EngineError::Internal { message },
    }
}

/// Maps a store error inside a tool to an internal tool error (logged, never
/// sent to the agent).
pub(crate) fn store_tool(error: StoreError) -> ToolError {
    ToolError::internal(format!("store: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_survive_both_mappings() {
        let tool: ToolError = EngineError::Invalid("bad".into()).into();
        assert_eq!(tool.kind(), "invalid_input");
        let tool: ToolError = EngineError::NotFound("x".into()).into();
        assert_eq!(tool.kind(), "not_found");
        let tool: ToolError = EngineError::Internal("db down".into()).into();
        assert_eq!(tool.kind(), "internal");
        assert!(!tool.client_message("r1").contains("db down"));
        let rest: knowell_server::EngineError = EngineError::Forbidden("no".into()).into();
        assert!(matches!(
            rest,
            knowell_server::EngineError::Forbidden { .. }
        ));
        let rest = tool_to_rest(ToolError::stale("expired"));
        assert!(matches!(
            rest,
            knowell_server::EngineError::Unavailable { .. }
        ));
    }
}
