//! Raw-wire lifecycle regressions without the rmcp client's metadata injection.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::sync::Arc;
use std::time::Duration;

use knowell_mcp::{FixtureTools, KnowellServer, serve_stdio_with_io};
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf,
};
use tokio::task::JoinHandle;
use tokio::time::timeout;

const LEGACY: &str = "2025-11-25";
const MODERN: &str = "2026-07-28";
const PROTOCOL_META: &str = "io.modelcontextprotocol/protocolVersion";
const CAPABILITIES_META: &str = "io.modelcontextprotocol/clientCapabilities";
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REPLY_BYTES: usize = 1 << 20;

struct WireClient {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
    server: JoinHandle<()>,
}

impl WireClient {
    fn new() -> Self {
        let (server_io, client_io) = tokio::io::duplex(MAX_REPLY_BYTES);
        let server = tokio::spawn(async move {
            let server = KnowellServer::new(Arc::new(FixtureTools::new()));
            let (read, write) = tokio::io::split(server_io);
            serve_stdio_with_io(server, read, write)
                .await
                .expect("synthetic server starts");
        });
        let (reader, writer) = tokio::io::split(client_io);
        Self {
            reader: BufReader::new(reader),
            writer,
            server,
        }
    }

    async fn send(&mut self, value: Value) {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        timeout(IO_TIMEOUT, self.writer.write_all(&bytes))
            .await
            .expect("raw request write timed out")
            .expect("raw request write failed");
        timeout(IO_TIMEOUT, self.writer.flush())
            .await
            .expect("raw request flush timed out")
            .expect("raw request flush failed");
    }

    async fn request(&mut self, id: u64, method: &str, params: Option<Value>) -> Value {
        let mut request = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(params) = params {
            request["params"] = params;
        }
        self.send(request).await;
        self.reply(id).await
    }

    async fn reply(&mut self, id: u64) -> Value {
        let wanted_id = json!(id);
        let reply = timeout(IO_TIMEOUT, async {
            // A bounded notification allowance keeps asynchronous messages from
            // being confused with the correlated response we are asserting.
            for _ in 0..32 {
                let mut line = String::new();
                let count = (&mut self.reader)
                    .take(u64::try_from(MAX_REPLY_BYTES).unwrap() + 1)
                    .read_line(&mut line)
                    .await
                    .unwrap();
                assert_ne!(count, 0, "server closed before the correlated response");
                assert!(
                    count <= MAX_REPLY_BYTES,
                    "synthetic reply exceeded its byte bound"
                );
                let message: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(message["jsonrpc"], "2.0");
                if message.get("id") == Some(&wanted_id) {
                    return message;
                }
                assert!(
                    message.get("id").is_none(),
                    "unexpected response id: {message}"
                );
                assert!(
                    message.get("method").is_some(),
                    "invalid notification: {message}"
                );
            }
            panic!("too many notifications before the correlated response")
        })
        .await
        .expect("raw response read timed out");
        assert_eq!(reply["id"], wanted_id);
        reply
    }

    async fn discover(&mut self, id: u64) {
        let reply = self
            .request(id, "server/discover", Some(json!({"_meta": modern_meta()})))
            .await;
        assert!(reply.get("error").is_none(), "discover failed: {reply}");
        let versions = reply["result"]["supportedVersions"].as_array().unwrap();
        assert!(
            versions.contains(&json!(MODERN)),
            "modern support was lost: {reply}"
        );
        assert!(
            versions.contains(&json!(LEGACY)),
            "legacy support was lost: {reply}"
        );
    }

    async fn initialize(&mut self, id: u64, requested: &str, negotiated: &str) {
        let reply = self
            .request(
                id,
                "initialize",
                Some(json!({
                    "protocolVersion": requested,
                    "capabilities": {},
                    "clientInfo": {"name": "synthetic-raw-client", "version": "1.0"}
                })),
            )
            .await;
        assert!(reply.get("error").is_none(), "initialize failed: {reply}");
        assert_eq!(reply["result"]["protocolVersion"], negotiated);
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
    }

    async fn assert_tools(&mut self, id: u64, params: Option<Value>) {
        let reply = self.request(id, "tools/list", params).await;
        assert!(reply.get("error").is_none(), "tools/list failed: {reply}");
        assert_eq!(reply["result"]["tools"].as_array().unwrap().len(), 14);
    }

    async fn assert_source(&mut self, id: u64, metadata: Option<Value>) {
        let mut params = json!({
            "name": "fetch",
            "arguments": {
                "workspace": "demo-shop",
                "paths": [{"project": "billing-api", "path": "src/payments/payment.service.ts"}]
            }
        });
        if let Some(metadata) = metadata {
            params["_meta"] = metadata;
        }
        let reply = self.request(id, "tools/call", Some(params)).await;
        assert!(reply.get("error").is_none(), "fetch failed: {reply}");
        assert_ne!(
            reply["result"]["isError"], true,
            "fetch tool error: {reply}"
        );
        assert!(reply["result"].get("structuredContent").is_none());
        let content = reply["result"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.contains("export class PaymentService"));
        assert!(text.contains("src/payments/payment.service.ts"));
    }
}

impl Drop for WireClient {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn modern_meta() -> Value {
    json!({
        PROTOCOL_META: MODERN,
        CAPABILITIES_META: {}
    })
}

fn legacy_list_params() -> [Option<Value>; 4] {
    [
        None,
        Some(json!({})),
        Some(json!({"_meta": {}})),
        Some(
            json!({"_meta": {"progressToken": "synthetic-progress", "example.trace/id": "synthetic"}}),
        ),
    ]
}

#[tokio::test]
async fn discover_then_legacy_initialize_clears_inline_metadata_requirement() {
    let mut client = WireClient::new();
    client.discover(1).await;
    // Real clients can probe discovery, then choose the initialize lifecycle.
    // The negotiated session must not retain the probe's per-request contract.
    client.initialize(2, LEGACY, LEGACY).await;
    for (id, params) in (3..).zip(legacy_list_params()) {
        client.assert_tools(id, params).await;
    }
    let ping = client.request(7, "ping", None).await;
    assert!(ping.get("error").is_none(), "legacy ping failed: {ping}");
    client.assert_source(8, None).await;
}

#[tokio::test]
async fn pipelined_discovery_and_initialize_use_the_legacy_session() {
    let mut client = WireClient::new();
    client
        .send(json!({
            "jsonrpc": "2.0", "id": 1, "method": "server/discover",
            "params": {"_meta": modern_meta()}
        }))
        .await;
    client
        .send(json!({
            "jsonrpc": "2.0", "id": 2, "method": "initialize",
            "params": {
                "protocolVersion": LEGACY, "capabilities": {},
                "clientInfo": {"name": "synthetic-pipelined-client", "version": "1.0"}
            }
        }))
        .await;
    let discovery = client.reply(1).await;
    assert!(discovery.get("error").is_none());
    let initialized = client.reply(2).await;
    assert_eq!(initialized["result"]["protocolVersion"], LEGACY);
    client
        .send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    client.assert_tools(3, None).await;
    let ping = client.request(4, "ping", None).await;
    assert!(ping.get("error").is_none(), "legacy ping failed: {ping}");
    client.assert_source(5, None).await;
}

#[tokio::test]
async fn modern_client_can_reuse_the_completed_probe_id() {
    let mut client = WireClient::new();
    client.discover(1).await;
    client
        .assert_tools(1, Some(json!({"_meta": modern_meta()})))
        .await;
    client.assert_source(2, Some(modern_meta())).await;
}

#[tokio::test]
async fn ordinary_legacy_initialize_does_not_require_protocol_metadata() {
    for version in ["2024-11-05", "2025-03-26", "2025-06-18", LEGACY] {
        let mut client = WireClient::new();
        client.initialize(1, version, version).await;
        for (id, params) in (2..).zip(legacy_list_params()) {
            client.assert_tools(id, params).await;
        }
        client.assert_source(6, None).await;
    }
}

#[tokio::test]
async fn modern_initialize_request_negotiates_a_supported_handshake_version() {
    let mut client = WireClient::new();
    client.initialize(1, MODERN, LEGACY).await;
    client.assert_tools(2, None).await;
}

#[tokio::test]
async fn genuine_modern_lifecycle_validates_metadata_without_poisoning_the_connection() {
    let mut client = WireClient::new();
    client.discover(1).await;
    client
        .assert_tools(2, Some(json!({"_meta": modern_meta()})))
        .await;
    client.assert_source(100, Some(modern_meta())).await;
    let ping = client
        .request(101, "ping", Some(json!({"_meta": modern_meta()})))
        .await;
    assert_eq!(ping["error"]["code"], -32601);

    let invalid = [
        (None, vec![PROTOCOL_META, CAPABILITIES_META]),
        (Some(json!({})), vec![PROTOCOL_META, CAPABILITIES_META]),
        (
            Some(json!({"_meta": {}})),
            vec![PROTOCOL_META, CAPABILITIES_META],
        ),
        (
            Some(json!({"_meta": {PROTOCOL_META: MODERN}})),
            vec![CAPABILITIES_META],
        ),
        (
            Some(json!({"_meta": {CAPABILITIES_META: {}}})),
            vec![PROTOCOL_META],
        ),
        (
            Some(json!({"_meta": {PROTOCOL_META: 123, CAPABILITIES_META: {}}})),
            vec![PROTOCOL_META],
        ),
        (
            Some(json!({"_meta": {PROTOCOL_META: MODERN, CAPABILITIES_META: null}})),
            vec![CAPABILITIES_META],
        ),
        (
            Some(
                json!({"_meta": {PROTOCOL_META: MODERN, CAPABILITIES_META: "KNOWELL_CANARY_INVALID_CAPABILITIES"}}),
            ),
            vec![CAPABILITIES_META],
        ),
        (
            Some(json!({"_meta": {PROTOCOL_META: MODERN, CAPABILITIES_META: []}})),
            vec![CAPABILITIES_META],
        ),
    ];
    let mut id = 3;
    for (params, missing) in invalid {
        let reply = client.request(id, "tools/list", params).await;
        assert!(
            reply.get("result").is_none(),
            "invalid metadata was accepted: {reply}"
        );
        assert_eq!(reply["error"]["code"], -32602);
        let message = reply["error"]["message"].as_str().unwrap();
        for field in missing {
            assert!(
                message.contains(field),
                "missing field was not reported: {reply}"
            );
        }
        assert!(!message.contains("KNOWELL_CANARY_INVALID_CAPABILITIES"));
        // Each malformed request is followed by a valid raw request on the
        // same pipe, proving an error does not silently destroy the catalogue.
        id += 1;
        client
            .assert_tools(id, Some(json!({"_meta": modern_meta()})))
            .await;
        id += 1;
    }
    let unsupported = client
        .request(
            id,
            "tools/list",
            Some(json!({"_meta": {
                PROTOCOL_META: "2099-01-01", CAPABILITIES_META: {}
            }})),
        )
        .await;
    assert_eq!(unsupported["error"]["code"], -32022);
    client
        .assert_tools(id + 1, Some(json!({"_meta": modern_meta()})))
        .await;
}
