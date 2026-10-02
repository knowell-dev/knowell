//! `know connect` / `know disconnect`.

use crate::common::Sandbox;

#[test]
fn dry_run_shows_diffs_and_writes_nothing() {
    let sb = Sandbox::new();
    let project = sb.work().join("app");
    std::fs::create_dir_all(&project).unwrap();
    let dir = project.to_str().unwrap();

    let out = sb.run(&["connect", "claude", "--dry-run", "--dir", dir]);
    assert_eq!(out.code, 0, "{out:?}");
    for expected in [
        ".mcp.json",
        "CLAUDE.md",
        "context --session-start",
        "nothing was written",
    ] {
        assert!(out.stdout.contains(expected), "missing {expected}: {out:?}");
    }
    assert!(!project.join(".mcp.json").exists());
    assert!(!project.join("CLAUDE.md").exists());
    assert!(!sb.home().join(".claude.json").exists());

    let json = sb.run(&["connect", "cursor", "--dry-run", "--dir", dir, "--json"]);
    assert_eq!(json.code, 0, "{json:?}");
    let value: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    assert_eq!(value["client"], "cursor");
    assert_eq!(value["dry_run"], true);
    assert!(!value["diffs"].as_array().unwrap().is_empty());
    assert!(!project.join(".cursor").exists());
}

#[test]
fn connect_then_disconnect_round_trips() {
    let sb = Sandbox::new();
    let project = sb.work().join("app");
    std::fs::create_dir_all(&project).unwrap();
    let dir = project.to_str().unwrap();

    let first = sb.run(&["connect", "codex", "--dir", dir]);
    assert_eq!(first.code, 0, "{first:?}");
    let config = std::fs::read_to_string(project.join(".codex/config.toml")).unwrap();
    assert!(config.contains("[mcp_servers.knowell]"), "{config}");
    assert!(project.join("AGENTS.md").is_file());

    let again = sb.run(&["connect", "codex", "--dir", dir]);
    assert_eq!(again.code, 0, "{again:?}");
    assert!(again.stdout.contains("already connected"), "{again:?}");

    let removed = sb.run(&["disconnect", "codex", "--dir", dir]);
    assert_eq!(removed.code, 0, "{removed:?}");
    assert!(!project.join(".codex/config.toml").exists());
    assert!(!project.join("AGENTS.md").exists());
}

#[test]
fn user_scope_stays_inside_the_sandbox_home() {
    let sb = Sandbox::new();
    let out = sb.run(&["connect", "claude", "--scope", "user", "--dry-run"]);
    assert_eq!(out.code, 0, "{out:?}");
    let home = sb.home();
    let claude_json = home.join(".claude.json");
    assert!(
        out.stdout.contains(claude_json.to_str().unwrap()),
        "{out:?}"
    );
}

#[test]
fn env_values_are_refused() {
    let sb = Sandbox::new();
    let out = sb.run(&[
        "connect",
        "claude",
        "--dry-run",
        "--env-name",
        "KNOWELL_CANARY=secret-value",
    ]);
    assert_eq!(out.code, 2, "{out:?}");
    assert!(!out.all().contains("secret-value"), "{out:?}");
}
