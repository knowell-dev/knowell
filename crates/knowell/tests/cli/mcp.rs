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
    let sb = Sandbox::new();
    let mut child = sb
        .command(&["mcp"])
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
