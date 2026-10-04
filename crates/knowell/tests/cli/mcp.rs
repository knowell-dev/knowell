//! `know mcp`: MCP over stdio.

use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::common::{Lines, Sandbox};

fn response(lines: &mut Lines, id: u64) -> serde_json::Value {
    let line = lines
        .wait_for(Duration::from_secs(30), |l| {
            serde_json::from_str::<serde_json::Value>(l)
                .is_ok_and(|v| v["id"] == serde_json::json!(id))
        })
        .unwrap_or_else(|| panic!("no response {id}; stdout so far: {:?}", lines.seen));
    serde_json::from_str(&line).unwrap()
}

#[test]
fn initialize_list_and_call_over_stdio() {
    stdio_session(&["mcp"], knowell_mcp::OutputMode::Source);
}

#[test]
fn source_stdio_flag_omits_read_schemas_and_preserves_errors() {
    stdio_session(
        &["mcp", "--output-mode", "source"],
        knowell_mcp::OutputMode::Source,
    );
}

#[test]
fn full_stdio_flag_preserves_typed_read_schemas() {
    stdio_session(
        &["mcp", "--output-mode", "full"],
        knowell_mcp::OutputMode::Full,
    );
}

#[test]
fn compact_stdio_flag_changes_read_schemas_and_preserves_errors() {
    stdio_session(
        &["mcp", "--output-mode", "compact"],
        knowell_mcp::OutputMode::Compact,
    );
}

#[test]
fn mcp_output_mode_rejects_unknown_values_before_starting() {
    let sb = Sandbox::new();
    let result = sb.run(&["mcp", "--output-mode", "unknown"]);
    assert_eq!(result.code, 2, "{result:?}");
    assert!(result.stderr.contains("--output-mode"), "{result:?}");
    assert!(result.stderr.contains("full"), "{result:?}");
    assert!(result.stderr.contains("compact"), "{result:?}");
    assert!(result.stderr.contains("source"), "{result:?}");
    assert!(result.stdout.is_empty(), "{result:?}");
}

fn stdio_session(args: &[&str], mode: knowell_mcp::OutputMode) {
    let sb = Sandbox::new();
    let mut child = sb
        .command(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = Lines::spawn(child.stdout.take().unwrap());
    let mut send = |value: serde_json::Value| {
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    };

    send(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "know-cli-test", "version": "0.0.0"}
        }
    }));
    let init = response(&mut lines, 1);
    assert!(init["result"]["serverInfo"].is_object(), "{init}");
    assert!(init["result"]["instructions"].is_string(), "{init}");
    send(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    send(serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let list = response(&mut lines, 2);
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 14, "{list}");
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let read_only = knowell_mcp::ToolName::parse(name).unwrap().is_read_only();
        if mode == knowell_mcp::OutputMode::Source && read_only {
            assert!(tool.get("outputSchema").is_none(), "{name}: {tool}");
            continue;
        }
        let properties = tool["outputSchema"]["properties"].as_object().unwrap();
        if mode == knowell_mcp::OutputMode::Compact && read_only {
            assert_eq!(properties.len(), 1, "{name}");
            assert_eq!(properties["text"]["type"], "string", "{name}");
            assert_eq!(
                tool["outputSchema"]["required"],
                serde_json::json!(["text"])
            );
        } else {
            assert!(!properties.contains_key("text"), "{name}");
        }
    }
    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort_unstable();
    for expected in [
        "open_workspace",
        "search",
        "index_status",
        "save_checkpoint",
    ] {
        assert!(names.contains(&expected), "{names:?}");
    }

    send(serde_json::json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "open_workspace", "arguments": {}}
    }));
    let call = response(&mut lines, 3);
    assert_eq!(call["result"]["isError"], true, "{call}");
    assert!(call["result"].get("structuredContent").is_none(), "{call}");
    let text = call["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("not_ready"), "{text}");
    // No database in this isolated home: the tools say so instead of guessing.
    assert!(text.contains("no database"), "{text}");

    // Every stdout line is a protocol message.
    for line in &lines.seen {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|_| panic!("non-protocol output on stdout: {line}"));
    }

    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("know mcp did not exit after stdin closed");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(0));
}
