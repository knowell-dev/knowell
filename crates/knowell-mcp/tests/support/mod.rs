//! Shared helpers for the integration tests: an in-process MCP client over a
//! duplex pipe, call helpers, and a small JSON Schema checker.

#![allow(dead_code)]

pub(crate) mod schema;

use std::sync::Arc;

use knowell_mcp::{FixtureTools, KnowellServer, OutputMode};
use rmcp::model::{CallToolRequestParams, CallToolResult, Tool};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;

/// Full-mode client for the typed contract compatibility suite.
pub(crate) async fn connect(tools: Arc<FixtureTools>) -> RunningService<RoleClient, ()> {
    connect_server(KnowellServer::new(tools).with_output_mode(OutputMode::Full)).await
}

/// A client connected to a server with an explicit presentation mode.
pub(crate) async fn connect_with_mode(
    tools: Arc<FixtureTools>,
    mode: OutputMode,
) -> RunningService<RoleClient, ()> {
    connect_server(KnowellServer::new(tools).with_output_mode(mode)).await
}

async fn connect_server(server: KnowellServer<FixtureTools>) -> RunningService<RoleClient, ()> {
    let (server_io, client_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });
    ().serve(client_io).await.expect("client handshake")
}

/// Calls a tool with JSON arguments.
pub(crate) async fn call(
    client: &RunningService<RoleClient, ()>,
    tool: &str,
    args: Value,
) -> CallToolResult {
    let arguments = args
        .as_object()
        .cloned()
        .expect("arguments must be an object");
    client
        .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments))
        .await
        .expect("tools/call transport")
}

/// The text content of a result (all text blocks joined).
pub(crate) fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The structured content of a successful result.
pub(crate) fn structured(result: &CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "tool failed: {}", text(result));
    result
        .structured_content
        .clone()
        .expect("structured content")
}

/// Asserts a tool error of `kind` and returns its text.
pub(crate) fn expect_error(result: &CallToolResult, kind: &str) -> String {
    assert_eq!(
        result.is_error,
        Some(true),
        "expected an error: {}",
        text(result)
    );
    assert!(result.structured_content.is_none());
    let text = text(result);
    assert!(text.starts_with(&format!("error[{kind}]")), "{text}");
    text
}

/// Opens the fixture workspace and returns the context id.
pub(crate) async fn open(client: &RunningService<RoleClient, ()>, args: Value) -> String {
    let result = call(client, "open_workspace", args).await;
    structured(&result)["context_id"]
        .as_str()
        .expect("context_id")
        .to_owned()
}

/// Validates a successful result's structured content against the tool's
/// advertised output schema.
pub(crate) fn assert_matches_output_schema(tool: &Tool, result: &CallToolResult) {
    let value = structured(result);
    let schema = Value::Object(
        tool.output_schema
            .as_ref()
            .expect("output schema")
            .as_ref()
            .clone(),
    );
    let errors = schema::validate(&schema, &value);
    assert!(
        errors.is_empty(),
        "{} output does not match its schema:\n{}\n{value:#}",
        tool.name,
        errors.join("\n")
    );
}
