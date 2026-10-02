//! Errors: [`ToolError`] for tool, prompt and resource calls, [`ServeError`]
//! for running a transport.

use rmcp::ErrorData;
use rmcp::model::ErrorCode;
use serde_json::json;

/// Longest message forwarded to a client, in characters. Longer messages are
/// cut; a client needs a short, actionable sentence, not a dump.
pub const MAX_ERROR_MESSAGE_CHARS: usize = 500;

/// JSON-RPC code for [`ToolError::PermissionDenied`] (Knowell-specific,
/// from the implementation-defined server-error range).
pub const PERMISSION_DENIED_CODE: i32 = -32040;
/// JSON-RPC code for [`ToolError::NotReady`].
pub const NOT_READY_CODE: i32 = -32041;
/// JSON-RPC code for [`ToolError::Stale`].
pub const STALE_CODE: i32 = -32042;

/// Failure of a tool call, as reported by a [`crate::KnowellTools`]
/// implementation or by input validation.
///
/// Messages of the user-facing variants are sent to the agent, so they must
/// be short, actionable and free of secrets and internal detail (hosts,
/// SQL, file-system paths outside repositories). The detail of
/// [`ToolError::Internal`] is logged on the server and never sent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolError {
    /// The arguments are malformed or contradictory; the agent can fix them.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// A workspace, context, project, id or record does not exist (or is not
    /// visible to the caller, which is reported the same way).
    #[error("not found: {0}")]
    NotFound(String),
    /// The caller may not perform this operation.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// The data needed is not ready yet (first index running); retry later.
    #[error("not ready: {message}")]
    NotReady {
        /// What is not ready.
        message: String,
        /// Suggested wait before retrying, in milliseconds.
        retry_after_ms: Option<u32>,
    },
    /// The context or view the call refers to was retired or expired; call
    /// `open_workspace` again.
    #[error("stale: {0}")]
    Stale(String),
    /// An unexpected failure inside the engine. The detail is for server logs
    /// only.
    #[error("internal error")]
    Internal(String),
}

impl ToolError {
    /// Invalid input with a message for the agent.
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }

    /// Not found with a message for the agent.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    /// Permission denied with a message for the agent.
    pub fn permission_denied(message: impl Into<String>) -> Self {
        Self::PermissionDenied(message.into())
    }

    /// Not ready with a message and an optional retry hint in milliseconds.
    pub fn not_ready(message: impl Into<String>, retry_after_ms: Option<u32>) -> Self {
        Self::NotReady {
            message: message.into(),
            retry_after_ms,
        }
    }

    /// Stale context or view with a message for the agent.
    pub fn stale(message: impl Into<String>) -> Self {
        Self::Stale(message.into())
    }

    /// Internal failure; `detail` is logged, never sent to the client.
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::Internal(detail.into())
    }

    /// Stable machine-readable kind: `invalid_input`, `not_found`,
    /// `permission_denied`, `not_ready`, `stale` or `internal`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidInput(_) => "invalid_input",
            Self::NotFound(_) => "not_found",
            Self::PermissionDenied(_) => "permission_denied",
            Self::NotReady { .. } => "not_ready",
            Self::Stale(_) => "stale",
            Self::Internal(_) => "internal",
        }
    }

    /// Whether retrying the same call later can succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::NotReady { .. } | Self::Internal(_))
    }

    /// The message that may be shown to the client: sanitised, bounded, and
    /// generic for internal errors. `request` identifies the call so that an
    /// operator can find the logged detail.
    pub fn client_message(&self, request: &str) -> String {
        match self {
            Self::InvalidInput(m)
            | Self::NotFound(m)
            | Self::PermissionDenied(m)
            | Self::Stale(m)
            | Self::NotReady { message: m, .. } => sanitize_message(m),
            Self::Internal(_) => format!(
                "internal error while handling request {}; details are in the server log",
                sanitize_message(request)
            ),
        }
    }

    /// One-line rendering for a tool result shown to the agent, with a hint
    /// on how to proceed.
    pub fn render(&self, request: &str) -> String {
        let message = self.client_message(request);
        let hint = match self {
            Self::InvalidInput(_) => " Fix the arguments and call again.",
            Self::NotFound(_) => "",
            Self::PermissionDenied(_) => " Do not retry; ask a person with access.",
            Self::NotReady {
                retry_after_ms: Some(ms),
                ..
            } => return format!("error[not_ready]: {message} Retry after {ms} ms."),
            Self::NotReady { .. } => " Retry later or check index_status.",
            Self::Stale(_) => " Call open_workspace again and use the new context_id.",
            Self::Internal(_) => " You may retry once.",
        };
        format!("error[{}]: {message}{hint}", self.kind())
    }

    /// Maps the error to a JSON-RPC error for requests that have no
    /// tool-level error channel (resources, prompts). `data.kind` carries
    /// [`ToolError::kind`].
    pub fn to_error_data(&self, request: &str) -> ErrorData {
        let message = self.client_message(request);
        let mut data = json!({ "kind": self.kind(), "retryable": self.is_retryable() });
        if let Self::NotReady {
            retry_after_ms: Some(ms),
            ..
        } = self
            && let Some(object) = data.as_object_mut()
        {
            object.insert("retry_after_ms".into(), json!(ms));
        }
        let code = match self {
            Self::InvalidInput(_) => ErrorCode::INVALID_PARAMS,
            Self::NotFound(_) => ErrorCode::RESOURCE_NOT_FOUND,
            Self::PermissionDenied(_) => ErrorCode(PERMISSION_DENIED_CODE),
            Self::NotReady { .. } => ErrorCode(NOT_READY_CODE),
            Self::Stale(_) => ErrorCode(STALE_CODE),
            Self::Internal(_) => ErrorCode::INTERNAL_ERROR,
        };
        ErrorData::new(code, message, Some(data))
    }
}

/// Removes control characters (keeping none: messages are one line) and
/// cuts the message to [`MAX_ERROR_MESSAGE_CHARS`].
pub(crate) fn sanitize_message(message: &str) -> String {
    let mut out = String::with_capacity(message.len().min(MAX_ERROR_MESSAGE_CHARS));
    for (count, c) in message.chars().enumerate() {
        if count >= MAX_ERROR_MESSAGE_CHARS {
            out.push('…');
            break;
        }
        out.push(if c.is_control() { ' ' } else { c });
    }
    out.trim().to_owned()
}

/// Failure to run an MCP transport.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// The MCP handshake with the client failed.
    #[error("mcp session could not start: {0}")]
    Initialize(String),
    /// The server task ended abnormally.
    #[error("mcp server task failed: {0}")]
    Task(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_codes() {
        let cases = [
            (ToolError::invalid_input("x"), "invalid_input", -32602),
            (ToolError::not_found("x"), "not_found", -32002),
            (
                ToolError::permission_denied("x"),
                "permission_denied",
                PERMISSION_DENIED_CODE,
            ),
            (
                ToolError::not_ready("x", Some(5)),
                "not_ready",
                NOT_READY_CODE,
            ),
            (ToolError::stale("x"), "stale", STALE_CODE),
            (ToolError::internal("x"), "internal", -32603),
        ];
        for (error, kind, code) in cases {
            assert_eq!(error.kind(), kind);
            let data = error.to_error_data("1");
            assert_eq!(data.code.0, code, "{kind}");
            assert_eq!(data.data.unwrap()["kind"], kind);
        }
    }

    #[test]
    fn internal_detail_never_leaks() {
        let error = ToolError::internal("connection to postgres://user:pw@10.0.0.5/db refused");
        for text in [
            error.client_message("7"),
            error.render("7"),
            error.to_error_data("7").message.into_owned(),
            error.to_string(),
        ] {
            assert!(!text.contains("10.0.0.5"), "{text}");
            assert!(!text.contains("postgres"), "{text}");
        }
        assert!(error.render("7").contains("request 7"));
    }

    #[test]
    fn messages_are_sanitised_and_bounded() {
        let error = ToolError::invalid_input(format!("bad\u{1b}[31m\nline\0{}", "x".repeat(2000)));
        let message = error.client_message("1");
        assert!(!message.chars().any(char::is_control), "{message:?}");
        assert!(message.chars().count() <= MAX_ERROR_MESSAGE_CHARS + 1);
        assert!(message.ends_with('…'));
    }

    #[test]
    fn renders_hints() {
        assert_eq!(
            ToolError::not_ready("index building", Some(5000)).render("1"),
            "error[not_ready]: index building Retry after 5000 ms."
        );
        assert!(
            ToolError::stale("context expired")
                .render("1")
                .contains("open_workspace")
        );
        let data = ToolError::not_ready("x", Some(250)).to_error_data("1");
        assert_eq!(data.data.unwrap()["retry_after_ms"], 250);
    }
}
