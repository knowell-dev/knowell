//! `know context --session-start` (the Claude Code SessionStart hook).

use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::common::Sandbox;

const WORKSPACE: &str = r#"version = 1
[workspace]
name = "demo-shop"
track = "branch:main"

[[project]]
name = "billing-api"
path = "billing-api"

[[project]]
name = "storefront-web"
path = "storefront-web"
"#;

#[test]
fn prints_the_workspace_quickly() {
    let sb = Sandbox::new();
    std::fs::write(sb.work().join("knowell.toml"), WORKSPACE).unwrap();
    let nested = sb.work().join("billing-api").join("src");
    std::fs::create_dir_all(&nested).unwrap();

    // The best of three runs: the first launch of a fresh binary can be
    // slowed down by the OS (e.g. a virus scan), which is not our latency.
    let mut best = Duration::MAX;
    let mut last = None;
    for _ in 0..3 {
        let started = Instant::now();
        let output = sb
            .command_in(&nested, &["context", "--session-start"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        best = best.min(started.elapsed());
        last = Some(output);
    }
    let output = last.unwrap();
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "demo-shop",
        "billing-api",
        "storefront-web",
        "open_workspace",
    ] {
        assert!(stdout.contains(expected), "missing {expected}: {stdout}");
    }
    // The engine is wired: the start-up text must not tell agents to skip it.
    assert!(!stdout.contains("not_ready"), "{stdout}");
    assert!(
        best < Duration::from_millis(200),
        "context --session-start took {best:?}"
    );
}

#[test]
fn never_fails_the_hook() {
    let sb = Sandbox::new();
    // No workspace at all.
    let none = sb.run(&["context", "--session-start"]);
    assert_eq!(none.code, 0, "{none:?}");
    assert!(none.stdout.contains("open_workspace"), "{none:?}");

    // An invalid workspace file.
    std::fs::write(
        sb.work().join("knowell.toml"),
        "version = 1\n[workspace]\nname = \"Not A Slug\"\n",
    )
    .unwrap();
    let invalid = sb.run(&["context", "--session-start"]);
    assert_eq!(invalid.code, 0, "{invalid:?}");
    assert!(invalid.stdout.contains("invalid"), "{invalid:?}");

    // A --workspace that does not exist.
    let missing = sb.run(&["--workspace", "nope.toml", "context", "--session-start"]);
    assert_eq!(missing.code, 0, "{missing:?}");
}
