//! Model Context Protocol server: Knowell's agent-facing tools, their
//! input/output schemas, and the stdio and Streamable HTTP transports.
//!
//! # Layers
//!
//! - [`tools`] — input/output types of the 14 tools (serde + JSON Schema),
//!   their catalog entry ([`ToolName`]) and input validation ([`Validate`]).
//! - [`KnowellTools`] — the engine trait, one async method per tool. The
//!   query engine and storage implement it; [`FixtureTools`] is an in-memory
//!   implementation with canned data for tests and client checks.
//! - [`KnowellServer`] — the rmcp server handler: tool listing with
//!   annotations, source-centered text or opt-in structured results, error
//!   mapping, prompts (`onboard`, `impact-review`) and the versioned-file
//!   resource template.
//! - [`serve_stdio`] and [`streamable_http_router`] — transports.
//! - [`CallerResolver`] — the authentication seam; [`LocalOnly`] by default.
//!
//! # Contract
//!
//! Every call carries the target it acts on (a `context_id` from
//! `open_workspace`, or a workspace with view pins), so concurrent agents
//! never change each other's selection. Every source-derived result item
//! carries [`Evidence`]; empty or partial results carry [`Gap`]s; repository
//! and memory text is [`UntrustedText`] with instruction-like lines flagged;
//! long operations return a [`JobRef`].
//!
//! ```no_run
//! use std::sync::Arc;
//! use knowell_mcp::{FixtureTools, serve_stdio};
//!
//! # async fn run() -> Result<(), knowell_mcp::ServeError> {
//! serve_stdio(Arc::new(FixtureTools::new())).await
//! # }
//! ```

mod caller;
mod engine;
mod error;
mod fixture;
mod ids;
mod model;
mod prompts;
mod render;
mod resource;
mod schema;
mod server;
mod source_render;
mod text;
pub mod tools;
mod transport;

pub use caller::{
    Caller, CallerResolver, ClientIdentity, LocalOnly, Principal, RequestHead, TransportKind,
};
pub use engine::KnowellTools;
pub use error::{
    MAX_ERROR_MESSAGE_CHARS, NOT_READY_CODE, PERMISSION_DENIED_CODE, STALE_CODE, ServeError,
    ToolError,
};
pub use fixture::{FIXTURE_WORKSPACE, FixtureTools};
pub use ids::{
    CheckpointId, CommitId, ContextId, IdError, JobId, MemoryId, ResultId, TaskId, Timestamp,
};
pub use model::{
    AnalysisLevel, Evidence, EvidenceType, FileLocator, FreshnessTier, Gap, GapReason, GraphHop,
    IndexState, JobRef, JobState, MatchReason, ProjectView, RelationKind, Resolution, SymbolRef,
    Target, ViewLayer, ViewPin,
};
pub use prompts::{IMPACT_REVIEW, INSTRUCTIONS, ONBOARD};
pub use render::reasons as match_reasons;
pub use resource::{FILE_URI_TEMPLATE, FileUri, MAX_URI_BYTES, UriError};
pub use server::{
    KnowellServer, META_EVIDENCE, META_INSTRUCTION_LIKE, META_TRUST, OutputMode,
    file_resource_template, tool_definition, tool_definitions,
};
pub use text::{
    InstructionFlag, InstructionPattern, TextOrigin, Trust, UntrustedText, detect_instruction_like,
};
pub use tools::{ToolName, Validate};
pub use transport::{
    DEFAULT_MAX_REQUEST_BODY_BYTES, HttpServerOptions, MCP_HTTP_PATH, serve_stdio,
    serve_stdio_with, streamable_http_router, streamable_http_service,
};
