//! [`KnowellServer`]: the rmcp [`ServerHandler`] that exposes a
//! [`KnowellTools`] engine as MCP tools, prompts and a resource template.

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use axum::http::request::Parts;
use rmcp::handler::server::tool::{schema_for_input, schema_for_output};
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    GetPromptRequestParams, GetPromptResponse, Implementation, JsonObject, ListPromptsResult,
    ListResourceTemplatesResult, ListToolsResult, MetaObject, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, ResourceContents,
    ResourceTemplate, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::caller::{
    Caller, CallerResolver, ClientIdentity, LocalOnly, RequestHead, TransportKind,
};
use crate::engine::KnowellTools;
use crate::error::ToolError;
use crate::model::{GapReason, Target, ViewPin};
use crate::prompts;
use crate::render::ToolOutput;
use crate::resource::{FILE_URI_TEMPLATE, FileUri};
use crate::schema;
use crate::tools::{
    AnalyzeImpactInput, AnalyzeImpactOutput, BuildContextInput, BuildContextOutput, ContractsInput,
    ContractsOutput, FetchInput, FetchOutput, HistoryInput, HistoryOutput, IndexStatusInput,
    IndexStatusOutput, InspectSymbolInput, InspectSymbolOutput, OpenWorkspaceInput,
    OpenWorkspaceOutput, ReadMemoryInput, ReadMemoryOutput, ResumeTaskInput, ResumeTaskOutput,
    SaveCheckpointInput, SaveCheckpointOutput, SearchInput, SearchOutput, ToolName, TraceFlowInput,
    TraceFlowOutput, Validate, WriteMemoryInput, WriteMemoryOutput,
};

/// How long clients may cache the static tool, prompt and template lists,
/// in milliseconds (protocol 2026-07-28 cache hints).
const LIST_TTL_MS: u64 = 60 * 60 * 1000;

/// `_meta` key carrying the trust label of resource contents.
pub const META_TRUST: &str = "knowell/trust";
/// `_meta` key carrying the evidence of resource contents.
pub const META_EVIDENCE: &str = "knowell/evidence";
/// `_meta` key carrying the instruction-like flags of resource contents.
pub const META_INSTRUCTION_LIKE: &str = "knowell/instructionLike";

/// Presentation of successful MCP tool results for one server instance.
///
/// The mode is static for the server and its clones. Clients must refresh
/// the tool catalogue after changing it because output schemas differ.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputMode {
    /// Source-centered text for read tools, without duplicate structured content.
    /// Write receipts retain their full typed outputs.
    #[default]
    Source,
    /// Full typed outputs, with the existing short text rendering.
    Full,
    /// Read tools return only the rendering in a structured `text` field.
    /// Write receipts retain their full typed outputs.
    Compact,
}

impl OutputMode {
    fn compacts(self, tool: ToolName) -> bool {
        self == Self::Compact && tool.is_read_only()
    }

    fn sources(self, tool: ToolName) -> bool {
        self == Self::Source && tool.is_read_only()
    }
}

/// MCP server for a [`KnowellTools`] engine.
///
/// The server holds no per-session selection: every call carries its own
/// target, so one instance (or clones of it) can serve many clients.
pub struct KnowellServer<T> {
    tools: Arc<T>,
    resolver: Arc<dyn CallerResolver>,
    transport: TransportKind,
    output_mode: OutputMode,
}

impl<T> Clone for KnowellServer<T> {
    fn clone(&self) -> Self {
        Self {
            tools: Arc::clone(&self.tools),
            resolver: Arc::clone(&self.resolver),
            transport: self.transport,
            output_mode: self.output_mode,
        }
    }
}

impl<T: KnowellTools> KnowellServer<T> {
    /// A server for a byte-stream transport (stdio, or an in-process duplex)
    /// that accepts the local user ([`LocalOnly`]).
    pub fn new(tools: Arc<T>) -> Self {
        Self {
            tools,
            resolver: Arc::new(LocalOnly),
            transport: TransportKind::Stdio,
            output_mode: OutputMode::default(),
        }
    }

    /// Replaces the caller resolver (the authentication seam).
    pub fn with_caller_resolver(mut self, resolver: Arc<dyn CallerResolver>) -> Self {
        self.resolver = resolver;
        self
    }

    /// Selects the static output format for this server and its clones.
    ///
    /// Source is the default. Read-tool schemas and results change together;
    /// mutation receipts and resource contents are unchanged.
    pub fn with_output_mode(mut self, mode: OutputMode) -> Self {
        self.output_mode = mode;
        self
    }

    /// Sets the transport reported to the resolver.
    pub(crate) fn with_transport(mut self, transport: TransportKind) -> Self {
        self.transport = transport;
        self
    }

    /// The engine.
    pub fn tools(&self) -> &Arc<T> {
        &self.tools
    }

    fn resolve_caller(&self, context: &RequestContext<RoleServer>) -> Result<Caller, ToolError> {
        let client = context.client_info().map(|info| ClientIdentity {
            name: info.name,
            version: info.version,
        });
        let head = RequestHead {
            transport: self.transport,
            http: context.extensions.get::<Parts>(),
            client: client.as_ref(),
        };
        self.resolver.resolve(&head)
    }

    async fn dispatch(
        &self,
        tool: ToolName,
        arguments: JsonObject,
        caller: &Caller,
    ) -> Result<Rendered, ToolError> {
        let tools = &*self.tools;
        let mode = if tool.is_read_only() {
            self.output_mode
        } else {
            OutputMode::Full
        };
        match tool {
            ToolName::OpenWorkspace => {
                invoke::<OpenWorkspaceInput, _, _, _>(arguments, mode, |input| {
                    tools.open_workspace(caller, input)
                })
                .await
            }
            ToolName::Search => {
                invoke::<SearchInput, _, _, _>(arguments, mode, |input| tools.search(caller, input))
                    .await
            }
            ToolName::Fetch => {
                invoke::<FetchInput, _, _, _>(arguments, mode, |input| tools.fetch(caller, input))
                    .await
            }
            ToolName::InspectSymbol => {
                invoke::<InspectSymbolInput, _, _, _>(arguments, mode, |input| {
                    tools.inspect_symbol(caller, input)
                })
                .await
            }
            ToolName::TraceFlow => {
                invoke::<TraceFlowInput, _, _, _>(arguments, mode, |input| {
                    tools.trace_flow(caller, input)
                })
                .await
            }
            ToolName::AnalyzeImpact => {
                invoke::<AnalyzeImpactInput, _, _, _>(arguments, mode, |input| {
                    tools.analyze_impact(caller, input)
                })
                .await
            }
            ToolName::Contracts => {
                invoke::<ContractsInput, _, _, _>(arguments, mode, |input| {
                    tools.contracts(caller, input)
                })
                .await
            }
            ToolName::BuildContext => {
                invoke::<BuildContextInput, _, _, _>(arguments, mode, |input| {
                    tools.build_context(caller, input)
                })
                .await
            }
            ToolName::History => {
                invoke::<HistoryInput, _, _, _>(arguments, mode, |input| {
                    tools.history(caller, input)
                })
                .await
            }
            ToolName::ReadMemory => {
                invoke::<ReadMemoryInput, _, _, _>(arguments, mode, |input| {
                    tools.read_memory(caller, input)
                })
                .await
            }
            ToolName::WriteMemory => {
                invoke::<WriteMemoryInput, _, _, _>(arguments, mode, |input| {
                    tools.write_memory(caller, input)
                })
                .await
            }
            ToolName::ResumeTask => {
                invoke::<ResumeTaskInput, _, _, _>(arguments, mode, |input| {
                    tools.resume_task(caller, input)
                })
                .await
            }
            ToolName::SaveCheckpoint => {
                invoke::<SaveCheckpointInput, _, _, _>(arguments, mode, |input| {
                    tools.save_checkpoint(caller, input)
                })
                .await
            }
            ToolName::IndexStatus => {
                invoke::<IndexStatusInput, _, _, _>(arguments, mode, |input| {
                    tools.index_status(caller, input)
                })
                .await
            }
        }
    }

    async fn read_file(&self, uri: &str, caller: &Caller) -> Result<ReadResourceResult, ToolError> {
        let file = FileUri::parse(uri).map_err(|e| ToolError::invalid_input(e.to_string()))?;
        let input = FetchInput {
            target: Target::workspace(
                file.workspace.clone(),
                vec![ViewPin {
                    project: file.project.clone(),
                    view: file.view.clone(),
                }],
            ),
            ids: Vec::new(),
            paths: vec![crate::model::FileLocator {
                project: file.project.clone(),
                path: file.path.clone(),
                lines: file.lines,
            }],
            context_lines: None,
        };
        input.validate()?;
        let output = self.tools.fetch(caller, input).await?;
        let Some(item) = output.items.into_iter().next() else {
            let reason = output.gaps.first();
            let message = reason.map_or_else(
                || "the file does not exist in the selected view".to_owned(),
                |gap| gap.message.clone(),
            );
            return Err(match reason.map(|gap| gap.reason) {
                Some(GapReason::ProjectNotIndexed) => ToolError::not_ready(message, None),
                _ => ToolError::not_found(message),
            });
        };
        let mut meta = MetaObject::new();
        meta.0.insert(META_TRUST.into(), json!("untrusted"));
        meta.0.insert(
            META_EVIDENCE.into(),
            serde_json::to_value(&item.evidence).unwrap_or(Value::Null),
        );
        if !item.content.instruction_like().is_empty() {
            meta.0.insert(
                META_INSTRUCTION_LIKE.into(),
                serde_json::to_value(item.content.instruction_like()).unwrap_or(Value::Null),
            );
        }
        let contents = ResourceContents::text(item.content.text(), uri)
            .with_mime_type("text/plain")
            .with_meta(meta);
        Ok(ReadResourceResult::new(vec![contents])
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Private))
    }
}

/// The complete structured read result in compact mode.
#[derive(serde::Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CompactReadOutput {
    text: String,
}

/// A rendered tool result: structured content plus text.
struct Rendered {
    structured: Option<Value>,
    text: String,
}

/// Deserialises, validates, runs and renders one tool call.
async fn invoke<I, O, F, Fut>(
    arguments: JsonObject,
    mode: OutputMode,
    run: F,
) -> Result<Rendered, ToolError>
where
    I: DeserializeOwned + Validate,
    O: ToolOutput,
    F: FnOnce(I) -> Fut,
    Fut: Future<Output = Result<O, ToolError>>,
{
    let input: I = serde_json::from_value(Value::Object(arguments))
        .map_err(|error| ToolError::invalid_input(error.to_string()))?;
    input.validate()?;
    let mut output = run(input).await?;
    if output.ensure_explained() {
        tracing::warn!("engine returned an empty or pending result without a gap; added one");
    }
    let text = if mode == OutputMode::Source {
        output.render_source()
    } else {
        output.render()
    };
    // Both output channels use the same rendering in compact mode. Clients
    // may serialize structuredContent into the model's context instead of
    // exposing TextContent, so keeping the full DTO there defeats compaction.
    let structured = match mode {
        OutputMode::Source => None,
        OutputMode::Compact => Some(serde_json::to_value(CompactReadOutput {
            text: text.clone(),
        })),
        OutputMode::Full => Some(serde_json::to_value(&output)),
    }
    .transpose()
    .map_err(|error| ToolError::internal(format!("cannot serialize tool output: {error}")))?;
    Ok(Rendered { structured, text })
}

/// The MCP tool definition of one tool: schemas, description and annotations.
///
/// Returns an error only if a schema is not an object schema, which the
/// crate's tests rule out.
pub fn tool_definition(tool: ToolName) -> Result<Tool, String> {
    tool_definition_with_mode(tool, OutputMode::Full)
}

fn tool_definition_with_mode(tool: ToolName, mode: OutputMode) -> Result<Tool, String> {
    fn build<I, O>(tool: ToolName, mode: OutputMode) -> Result<Tool, String>
    where
        I: JsonSchema + 'static,
        O: JsonSchema + 'static,
    {
        let mut input = schema_for_input::<I>()?.as_ref().clone();
        schema::compact_input(&mut input);
        let definition = Tool::new(tool.as_str(), tool.description(), input)
            .with_title(tool.title())
            .with_annotations(tool.annotations());
        if mode.sources(tool) {
            // MCP output schemas describe structuredContent, which source mode
            // deliberately omits rather than presenting the text twice.
            return Ok(definition);
        }
        let mut output = if mode.compacts(tool) {
            schema_for_output::<CompactReadOutput>().as_ref().clone()
        } else {
            schema_for_output::<O>().as_ref().clone()
        };
        schema::compact_output(&mut output);
        Ok(definition.with_raw_output_schema(Arc::new(output)))
    }
    match tool {
        ToolName::OpenWorkspace => build::<OpenWorkspaceInput, OpenWorkspaceOutput>(tool, mode),
        ToolName::Search => build::<SearchInput, SearchOutput>(tool, mode),
        ToolName::Fetch => build::<FetchInput, FetchOutput>(tool, mode),
        ToolName::InspectSymbol => build::<InspectSymbolInput, InspectSymbolOutput>(tool, mode),
        ToolName::TraceFlow => build::<TraceFlowInput, TraceFlowOutput>(tool, mode),
        ToolName::AnalyzeImpact => build::<AnalyzeImpactInput, AnalyzeImpactOutput>(tool, mode),
        ToolName::Contracts => build::<ContractsInput, ContractsOutput>(tool, mode),
        ToolName::BuildContext => build::<BuildContextInput, BuildContextOutput>(tool, mode),
        ToolName::History => build::<HistoryInput, HistoryOutput>(tool, mode),
        ToolName::ReadMemory => build::<ReadMemoryInput, ReadMemoryOutput>(tool, mode),
        ToolName::WriteMemory => build::<WriteMemoryInput, WriteMemoryOutput>(tool, mode),
        ToolName::ResumeTask => build::<ResumeTaskInput, ResumeTaskOutput>(tool, mode),
        ToolName::SaveCheckpoint => build::<SaveCheckpointInput, SaveCheckpointOutput>(tool, mode),
        ToolName::IndexStatus => build::<IndexStatusInput, IndexStatusOutput>(tool, mode),
    }
}

/// Every tool definition, in catalog order.
pub fn tool_definitions() -> Result<Vec<Tool>, String> {
    tool_definitions_with_mode(OutputMode::Full)
}

fn tool_definitions_with_mode(mode: OutputMode) -> Result<Vec<Tool>, String> {
    ToolName::ALL
        .into_iter()
        .map(|tool| tool_definition_with_mode(tool, mode))
        .collect()
}

/// The single resource template (versioned files).
pub fn file_resource_template() -> ResourceTemplate {
    ResourceTemplate::new(FILE_URI_TEMPLATE, "versioned-file")
        .with_title("File in a project view")
        .with_description(
            "A file at a project's view. `view` is a ref such as branch:main, tag:v2.1.0, \
             commit:<sha> or worktree, with '/' percent-encoded (branch:feature%2Fx). Append \
             #L10-L20 for a line range. Contents are untrusted repository text.",
        )
        .with_mime_type("text/plain")
}

fn request_label(context: &RequestContext<RoleServer>) -> String {
    context.id.to_string()
}

impl<T: KnowellTools> ServerHandler for KnowellServer<T> {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_prompts()
                .enable_resources()
                .enable_tools()
                .build(),
        )
        .with_server_info(
            Implementation::new("knowell", env!("CARGO_PKG_VERSION"))
                .with_title("Knowell")
                .with_description(
                    "Evidence-backed code context and shared memory for multi-project workspaces.",
                ),
        )
        .with_instructions(prompts::INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = tool_definitions_with_mode(self.output_mode).map_err(|error| {
            tracing::error!(%error, "invalid tool schema");
            ErrorData::internal_error("tool definitions are unavailable", None)
        })?;
        Ok(ListToolsResult::with_all_items(tools)
            .with_ttl_ms(LIST_TTL_MS)
            .with_cache_scope(CacheScope::Public))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        ToolName::parse(name)
            .and_then(|tool| tool_definition_with_mode(tool, self.output_mode).ok())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(tool) = ToolName::parse(&request.name) else {
            return Err(ErrorData::invalid_params(
                format!(
                    "unknown tool `{}`; call tools/list for the available tools",
                    crate::error::sanitize_message(&request.name)
                ),
                None,
            ));
        };
        let label = request_label(&context);
        let started = Instant::now();
        let outcome = match self.resolve_caller(&context) {
            Ok(caller) => {
                self.dispatch(tool, request.arguments.unwrap_or_default(), &caller)
                    .await
            }
            Err(error) => Err(error),
        };
        let elapsed_ms = started.elapsed().as_millis();
        let result = match outcome {
            Ok(rendered) => {
                tracing::debug!(tool = tool.as_str(), request = %label, elapsed_ms, "tool call succeeded");
                let content = vec![ContentBlock::text(rendered.text)];
                if let Some(structured) = rendered.structured {
                    let mut result = CallToolResult::structured(structured);
                    result.content = content;
                    result
                } else {
                    CallToolResult::success(content)
                }
            }
            Err(error) => {
                match &error {
                    ToolError::Internal(detail) => tracing::error!(
                        tool = tool.as_str(),
                        request = %label,
                        elapsed_ms,
                        detail = %detail,
                        "tool call failed"
                    ),
                    other => tracing::debug!(
                        tool = tool.as_str(),
                        request = %label,
                        elapsed_ms,
                        kind = other.kind(),
                        "tool call rejected"
                    ),
                }
                CallToolResult::error(vec![ContentBlock::text(error.render(&label))])
            }
        };
        Ok(result.into())
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(prompts::list())
            .with_ttl_ms(LIST_TTL_MS)
            .with_cache_scope(CacheScope::Public))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        let label = request_label(&context);
        self.resolve_caller(&context)
            .and_then(|_| prompts::get(&request.name, request.arguments.as_ref()))
            .map(GetPromptResponse::from)
            .map_err(|error| error.to_error_data(&label))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(
            ListResourceTemplatesResult::with_all_items(vec![file_resource_template()])
                .with_ttl_ms(LIST_TTL_MS)
                .with_cache_scope(CacheScope::Public),
        )
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let label = request_label(&context);
        let caller = self
            .resolve_caller(&context)
            .map_err(|error| error.to_error_data(&label))?;
        match self.read_file(&request.uri, &caller).await {
            Ok(result) => Ok(result.into()),
            Err(error) => {
                if let ToolError::Internal(detail) = &error {
                    tracing::error!(request = %label, detail = %detail, "resource read failed");
                }
                Err(error.to_error_data(&label))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Name, description and input schema of every tool: what MCP clients
    /// put into the model's context on every turn.
    fn model_facing_bytes(tool: &Tool) -> usize {
        tool.name.len()
            + tool.description.as_deref().map_or(0, str::len)
            + serde_json::to_string(tool.input_schema.as_ref()).map_or(0, |s| s.len())
    }

    #[test]
    fn model_facing_size_stays_within_budget() {
        const BUDGET: usize = 14_000;
        let tools = tool_definitions().unwrap();
        let total: usize = tools.iter().map(model_facing_bytes).sum();
        assert!(
            total <= BUDGET,
            "tool names, descriptions and input schemas take {total} bytes (budget {BUDGET});              trim descriptions or add `#[schemars(description = \"\")]` to self-explanatory fields.              Run `cargo run -p knowell-mcp --example tool_size` for per-tool sizes."
        );
        for tool in &tools {
            let size = model_facing_bytes(tool);
            assert!(size <= 2_500, "{} takes {size} bytes", tool.name);
        }
    }

    #[test]
    fn input_schemas_are_inlined_and_without_examples() {
        for tool in tool_definitions().unwrap() {
            let text = serde_json::to_string(tool.input_schema.as_ref()).unwrap();
            for banned in ["$defs", "$ref", "\"examples\"", "\"$schema\"", "\"null\""] {
                assert!(
                    !text.contains(banned),
                    "{} input schema has {banned}",
                    tool.name
                );
            }
        }
    }
}
