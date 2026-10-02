//! `know serve`.

use crate::common::{Sandbox, Server, free_port, http_get};

#[test]
fn serves_health_and_panel_then_stops_gracefully() {
    let sb = Sandbox::new();
    sb.write_engine("version = 1\n");
    let server = Server::start(&sb, &["--no-database"]);
    let (status, body) = http_get(server.addr, "/api/v1/health/live");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("ok"), "{body}");
    let (panel, _) = http_get(server.addr, "/");
    assert_eq!(panel, 200);
    // Delegated routes say why they cannot answer, they do not fake it.
    let (session, _) = http_get(server.addr, "/api/v1/session");
    assert_eq!(session, 200);
    assert!(
        server.lines.seen.iter().any(|l| l.contains("/mcp")),
        "{:?}",
        server.lines.seen
    );
    assert_eq!(server.stop(), 0);
}

#[test]
fn loopback_rule_and_missing_config() {
    let sb = Sandbox::new();
    let none = sb.run(&["serve", "--no-database"]);
    assert_eq!(none.code, 2, "{none:?}");
    assert!(none.stderr.contains("know init"), "{none:?}");

    sb.write_engine("version = 1\n");
    let port = free_port().to_string();
    let exposed = sb.run(&[
        "serve",
        "--no-database",
        "--listen",
        &format!("0.0.0.0:{port}"),
    ]);
    assert_eq!(exposed.code, 2, "{exposed:?}");
    assert!(exposed.stderr.contains("loopback"), "{exposed:?}");

    let hub = sb.run(&[
        "serve",
        "--no-database",
        "--role",
        "hub",
        "--listen",
        &format!("0.0.0.0:{port}"),
    ]);
    assert_eq!(hub.code, 2, "{hub:?}");
    assert!(hub.stderr.contains("--public-url"), "{hub:?}");

    let edge = sb.run(&["serve", "--no-database", "--role", "edge"]);
    assert_eq!(edge.code, 2, "{edge:?}");
    assert!(edge.stderr.contains("hub"), "{edge:?}");
}

#[test]
fn hub_with_public_url_serves() {
    let sb = Sandbox::new();
    sb.write_engine("version = 1\n");
    let server = Server::start(
        &sb,
        &[
            "--no-database",
            "--role",
            "hub",
            "--public-url",
            "https://knowell.example.com",
        ],
    );
    let (status, _) = http_get(server.addr, "/api/v1/health/live");
    assert_eq!(status, 200);
    assert_eq!(server.stop(), 0);
}
