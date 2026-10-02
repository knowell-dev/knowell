//! Streamable HTTP transport: the axum router is driven in-process with
//! `tower::ServiceExt::oneshot`.

// Test code: panicking on unexpected values is the assertion mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

mod support;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use knowell_mcp::{
    FixtureTools, HttpServerOptions, KnowellServer, MCP_HTTP_PATH, streamable_http_router,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const PROTOCOL: &str = "2025-11-25";

fn router(options: &HttpServerOptions) -> axum::Router {
    streamable_http_router(KnowellServer::new(Arc::new(FixtureTools::new())), options)
}

/// Options for one-shot JSON requests without sessions.
fn stateless() -> HttpServerOptions {
    HttpServerOptions {
        stateful_sessions: false,
        json_response: true,
        ..HttpServerOptions::default()
    }
}

fn post() -> axum::http::request::Builder {
    post_to("127.0.0.1:7341")
}

fn post_to(host: &str) -> axum::http::request::Builder {
    Request::builder()
        .method("POST")
        .uri(MCP_HTTP_PATH)
        .header(header::HOST, host)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header("mcp-protocol-version", PROTOCOL)
}

async fn send(router: axum::Router, request: Request<Body>) -> (StatusCode, String) {
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// The JSON-RPC message in a JSON body or in the first SSE `data:` line.
fn message(body: &str) -> Value {
    if let Ok(value) = serde_json::from_str(body) {
        return value;
    }
    let data = body
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .find(|data| !data.is_empty())
        .unwrap_or_else(|| panic!("no JSON-RPC message in {body}"));
    serde_json::from_str(data).unwrap()
}

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL,
            "capabilities": {},
            "clientInfo": {"name": "http-test", "version": "1.0"}
        }
    })
}

#[tokio::test]
async fn initialize_over_http_on_loopback() {
    let body = initialize();
    let request = post().body(Body::from(body.to_string())).unwrap();
    let (status, text) = send(router(&HttpServerOptions::default()), request).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let reply = message(&text);
    assert_eq!(reply["result"]["serverInfo"]["name"], "knowell");
    assert_eq!(reply["result"]["protocolVersion"], PROTOCOL);
    assert!(
        reply["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("open_workspace")
    );
}

#[tokio::test]
async fn rejects_foreign_hosts_and_browser_origins() {
    let body = initialize();
    let foreign_host = post_to("attacker.example")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, _) = send(router(&HttpServerOptions::default()), foreign_host).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let browser = post()
        .header(header::ORIGIN, "https://attacker.example")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, _) = send(router(&HttpServerOptions::default()), browser).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "origins are rejected unless configured"
    );

    let allowed = HttpServerOptions {
        allowed_origins: vec!["http://localhost:*".into()],
        ..HttpServerOptions::default()
    };
    let inspector = post()
        .header(header::ORIGIN, "http://localhost:6274")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, text) = send(router(&allowed), inspector).await;
    assert_eq!(status, StatusCode::OK, "{text}");
}

#[tokio::test]
async fn stateless_tool_calls_return_structured_results() {
    let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
    let request = post().body(Body::from(list.to_string())).unwrap();
    let (status, text) = send(router(&stateless()), request).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let reply = message(&text);
    assert_eq!(reply["result"]["tools"].as_array().unwrap().len(), 14);

    let open = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {"name": "open_workspace", "arguments": {"workspace": "demo-shop"}}
    });
    let request = post().body(Body::from(open.to_string())).unwrap();
    let (status, text) = send(router(&stateless()), request).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let reply = message(&text);
    assert_eq!(reply["result"]["isError"], false);
    assert!(reply["result"]["structuredContent"]["context_id"].is_string());
}

#[tokio::test]
async fn local_only_resolver_rejects_remote_peers() {
    let open = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {"name": "open_workspace", "arguments": {}}
    });
    let remote: SocketAddr = "192.0.2.10:50000".parse().unwrap();
    let mut request = post().body(Body::from(open.to_string())).unwrap();
    request.extensions_mut().insert(ConnectInfo(remote));
    let (status, text) = send(router(&stateless()), request).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let reply = message(&text);
    assert_eq!(reply["result"]["isError"], true);
    let content = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(content.starts_with("error[permission_denied]"), "{content}");
    assert!(!content.contains("192.0.2.10"));

    let local: SocketAddr = "127.0.0.1:50000".parse().unwrap();
    let mut request = post().body(Body::from(open.to_string())).unwrap();
    request.extensions_mut().insert(ConnectInfo(local));
    let (_, text) = send(router(&stateless()), request).await;
    assert_eq!(message(&text)["result"]["isError"], false);
}
