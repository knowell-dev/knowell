//! `know connect` / `know disconnect`.

use crate::common::{Run, Sandbox};

const SELECTED_WORKSPACE: &str =
    "version = 1\n[workspace]\nname = 'selected-workspace'\ntrack = 'branch:main'\n";

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
        "--session-start",
        "--output-mode",
        "source",
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

#[test]
fn explicit_engine_and_workspace_are_preserved_in_every_client_launcher() {
    for client in ["claude", "codex", "cursor"] {
        let sb = Sandbox::new();
        let selected = sb.work().join("Örnek Ayarlar");
        let project = sb.work().join("Başka Proje");
        std::fs::create_dir_all(&selected).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let engine = selected.join("engine config.toml");
        let workspace = selected.join("knowell.toml");
        std::fs::write(&engine, "version = 1\n").unwrap();
        std::fs::write(&workspace, SELECTED_WORKSPACE).unwrap();
        let args = [
            "--config",
            engine.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
            "connect",
            client,
            "--dir",
            project.to_str().unwrap(),
        ];
        let first = sb.run(&args);
        assert_eq!(first.code, 0, "{first:?}");
        let expected = vec![
            "--config",
            engine.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
            "mcp",
            "--output-mode",
            "source",
        ];
        if client == "codex" {
            let text = std::fs::read_to_string(project.join(".codex/config.toml")).unwrap();
            let table: toml::Table = text.parse().unwrap();
            let entry = &table["mcp_servers"]["knowell"];
            let actual: Vec<_> = entry["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap())
                .collect();
            assert_eq!(actual, expected);
            assert_eq!(entry["env_vars"][0].as_str(), Some("KNOWELL_HOME"));
            assert!(!text.contains(sb.knowell_home().to_str().unwrap()));
        } else {
            let path = if client == "claude" {
                ".mcp.json"
            } else {
                ".cursor/mcp.json"
            };
            let text = std::fs::read_to_string(project.join(path)).unwrap();
            let config: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(
                config["mcpServers"]["knowell"]["args"],
                serde_json::json!(expected)
            );
            assert!(!text.contains(sb.knowell_home().to_str().unwrap()));
            if client == "claude" {
                let settings: serde_json::Value = serde_json::from_str(
                    &std::fs::read_to_string(project.join(".claude/settings.json")).unwrap(),
                )
                .unwrap();
                let hook = &settings["hooks"]["SessionStart"][0]["hooks"][0];
                assert_eq!(hook["command"], "know");
                assert_eq!(
                    hook["args"],
                    serde_json::json!([
                        "--config",
                        engine.to_str().unwrap(),
                        "--workspace",
                        workspace.to_str().unwrap(),
                        "context",
                        "--session-start",
                    ])
                );
                let hook_args: Vec<_> = hook["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|value| value.as_str().unwrap())
                    .collect();
                let hook_result: Run = sb.command_in(&project, &hook_args).output().unwrap().into();
                assert_eq!(hook_result.code, 0, "{hook_result:?}");
                assert!(
                    hook_result.stdout.contains("selected-workspace"),
                    "{hook_result:?}"
                );
            }
        }
        let again = sb.run(&args);
        assert_eq!(again.code, 0, "{again:?}");
        assert!(again.stdout.contains("already connected"), "{again:?}");
    }
}

#[test]
fn default_project_connection_does_not_pin_personal_paths_or_environment() {
    let sb = Sandbox::new();
    std::fs::write(sb.work().join("knowell.toml"), SELECTED_WORKSPACE).unwrap();
    let out: Run = sb
        .command(&["connect", "claude"])
        .env_remove("KNOWELL_HOME")
        .output()
        .unwrap()
        .into();
    assert_eq!(out.code, 0, "{out:?}");
    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.work().join(".mcp.json")).unwrap())
            .unwrap();
    assert_eq!(
        config["mcpServers"]["knowell"]["args"],
        serde_json::json!(["mcp", "--output-mode", "source"])
    );
    assert!(config["mcpServers"]["knowell"].get("env").is_none());
}

#[test]
fn relative_selections_resolve_before_the_client_changes_working_directory() {
    let sb = Sandbox::new();
    let selected = sb.work().join("Settings With Spaces");
    let project = sb.work().join("project");
    std::fs::create_dir_all(&selected).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(selected.join("engine.toml"), "version = 1\n").unwrap();
    std::fs::write(selected.join("knowell.toml"), SELECTED_WORKSPACE).unwrap();
    let out = sb.run(&[
        "connect",
        "claude",
        "--dir",
        "project",
        "--config",
        "Settings With Spaces/engine.toml",
        "--workspace",
        "Settings With Spaces/knowell.toml",
        "--env-name",
        "KNOWELL_HOME",
    ]);
    assert_eq!(out.code, 0, "{out:?}");
    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(project.join(".mcp.json")).unwrap()).unwrap();
    let entry = &config["mcpServers"]["knowell"];
    assert_eq!(
        entry["args"],
        serde_json::json!([
            "--config",
            selected.join("engine.toml").to_str().unwrap(),
            "--workspace",
            selected.join("knowell.toml").to_str().unwrap(),
            "mcp",
            "--output-mode",
            "source",
        ])
    );
    assert_eq!(entry["env"].as_object().unwrap().len(), 1);
    assert_eq!(entry["env"]["KNOWELL_HOME"], "${KNOWELL_HOME}");
}

#[test]
fn missing_explicit_workspace_cannot_connect_to_an_unrelated_project() {
    let sb = Sandbox::new();
    let project = sb.work().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let out = sb.run(&[
        "connect",
        "claude",
        "--dir",
        "project",
        "--workspace",
        "missing.toml",
    ]);
    assert_eq!(out.code, 2, "{out:?}");
    assert!(!project.join(".mcp.json").exists());
    assert!(!project.join(".claude/settings.json").exists());
}

#[test]
fn disconnect_removes_selected_launcher_even_after_selected_workspace_is_deleted() {
    let sb = Sandbox::new();
    let project = sb.work().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let workspace = sb.work().join("selected.toml");
    std::fs::write(&workspace, SELECTED_WORKSPACE).unwrap();
    std::fs::write(
        project.join(".mcp.json"),
        r#"{"mcpServers":{"other":{"command":"other-server"}}}"#,
    )
    .unwrap();
    let first = sb.run(&[
        "connect",
        "claude",
        "--dir",
        "project",
        "--workspace",
        "selected.toml",
    ]);
    assert_eq!(first.code, 0, "{first:?}");
    std::fs::remove_file(workspace).unwrap();
    let removed = sb.run(&[
        "disconnect",
        "claude",
        "--dir",
        "project",
        "--workspace",
        "selected.toml",
    ]);
    assert_eq!(removed.code, 0, "{removed:?}");
    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(project.join(".mcp.json")).unwrap()).unwrap();
    assert_eq!(
        config,
        serde_json::json!({"mcpServers":{"other":{"command":"other-server"}}})
    );
    assert!(!project.join(".claude/settings.json").exists());
}

#[test]
fn doctor_checks_the_same_explicit_routing_as_connect() {
    let sb = Sandbox::new();
    std::fs::write(sb.work().join("knowell.toml"), SELECTED_WORKSPACE).unwrap();
    // Connection planning and diagnostics do not require a reachable database.
    let connected = sb.run(&["--config", "selected-engine.toml", "connect", "claude"]);
    assert_eq!(connected.code, 0, "{connected:?}");
    let doctor = sb.run(&["--config", "selected-engine.toml", "doctor", "--json"]);
    let report: serde_json::Value = serde_json::from_str(&doctor.stdout).unwrap();
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "agent_clients")
        .unwrap();
    assert_eq!(check["status"], "ok", "{doctor:?}");
    assert!(
        check["summary"]
            .as_str()
            .unwrap()
            .contains("claude (project)")
    );
}

#[test]
fn doctor_reports_legacy_output_settings_without_rewriting_client_files() {
    let sb = Sandbox::new();
    std::fs::write(sb.work().join("knowell.toml"), SELECTED_WORKSPACE).unwrap();
    let connected = sb.run(&["connect", "claude"]);
    assert_eq!(connected.code, 0, "{connected:?}");
    let mcp = sb.work().join(".mcp.json");
    let mut config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&mcp).unwrap()).unwrap();
    config["mcpServers"]["knowell"]["args"] = serde_json::json!(["mcp"]);
    config["mcpServers"]["other"] = serde_json::json!({"command": "other-server"});
    std::fs::write(&mcp, serde_json::to_string_pretty(&config).unwrap()).unwrap();
    let before = std::fs::read(&mcp).unwrap();
    let doctor = sb.run(&["doctor", "--json"]);
    let report: serde_json::Value = serde_json::from_str(&doctor.stdout).unwrap();
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "agent_clients")
        .unwrap();
    assert_eq!(check["status"], "warn", "{doctor:?}");
    assert!(check["summary"].as_str().unwrap().contains("source-output"));
    assert_eq!(std::fs::read(&mcp).unwrap(), before);
    let reconnected = sb.run(&["connect", "claude"]);
    assert_eq!(reconnected.code, 0, "{reconnected:?}");
    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&mcp).unwrap()).unwrap();
    assert_eq!(
        config["mcpServers"]["knowell"]["args"],
        serde_json::json!(["mcp", "--output-mode", "source"])
    );
    assert_eq!(
        config["mcpServers"]["other"],
        serde_json::json!({"command": "other-server"})
    );
    let hooks: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(sb.work().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(hooks["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
    let again = sb.run(&["connect", "claude"]);
    assert_eq!(again.code, 0, "{again:?}");
    assert!(again.stdout.contains("already connected"));
}

#[test]
fn mcp_explicit_missing_or_invalid_selection_never_uses_the_registry() {
    let sb = Sandbox::new();
    sb.write_engine("version = 1\n");
    let missing = sb.work().join("missing-config.toml");
    let out = sb.run(&["--config", missing.to_str().unwrap(), "mcp"]);
    assert_ne!(out.code, 0, "{out:?}");
    assert!(
        out.stdout.is_empty(),
        "protocol output must stay empty: {out:?}"
    );
    assert!(
        out.stderr
            .contains("selected engine configuration is missing"),
        "{out:?}"
    );
    let missing = sb.work().join("missing-workspace.toml");
    let out = sb.run(&["--workspace", missing.to_str().unwrap(), "mcp"]);
    assert_ne!(out.code, 0, "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    assert!(out.stderr.contains("does not exist"), "{out:?}");
    let invalid = sb.work().join("invalid-workspace.toml");
    std::fs::write(&invalid, "malformed input [").unwrap();
    let out = sb.run(&["--workspace", invalid.to_str().unwrap(), "mcp"]);
    assert_ne!(out.code, 0, "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
}

#[test]
fn experimental_parse_cache_opt_in_is_forwarded_to_mcp_and_startup_hook() {
    let sb = Sandbox::new();
    let project = sb.work().join("cache-opt-in");
    std::fs::create_dir_all(&project).unwrap();
    let out = sb.run(&[
        "--parse-cache",
        "connect",
        "claude",
        "--dir",
        project.to_str().unwrap(),
    ]);
    assert_eq!(out.code, 0, "{out:?}");
    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(project.join(".mcp.json")).unwrap()).unwrap();
    assert_eq!(
        config["mcpServers"]["knowell"]["args"],
        serde_json::json!(["--parse-cache", "mcp", "--output-mode", "source"])
    );
    let hooks: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(project.join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    let hook = &hooks["hooks"]["SessionStart"][0]["hooks"][0];
    assert_eq!(
        hook["args"],
        serde_json::json!(["--parse-cache", "context", "--session-start"])
    );
}

#[test]
fn lexical_span_experiment_is_bounded_and_retained_in_client_and_hook_arguments() {
    let sb = Sandbox::new();
    for spans in ["1", "2", "3"] {
        let project = sb.work().join(format!("span-opt-in-{spans}"));
        std::fs::create_dir_all(&project).unwrap();
        let out = sb.run(&[
            "--lexical-spans",
            spans,
            "connect",
            "claude",
            "--dir",
            project.to_str().unwrap(),
        ]);
        assert_eq!(out.code, 0, "{out:?}");
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(project.join(".mcp.json")).unwrap())
                .unwrap();
        let hooks: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(project.join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        let expected_mcp = if spans == "1" {
            serde_json::json!(["mcp", "--output-mode", "source"])
        } else {
            serde_json::json!(["--lexical-spans", spans, "mcp", "--output-mode", "source"])
        };
        let expected_hook = if spans == "1" {
            serde_json::json!(["context", "--session-start"])
        } else {
            serde_json::json!(["--lexical-spans", spans, "context", "--session-start"])
        };
        assert_eq!(config["mcpServers"]["knowell"]["args"], expected_mcp);
        assert_eq!(
            hooks["hooks"]["SessionStart"][0]["hooks"][0]["args"],
            expected_hook
        );
    }
    let rejected = sb.work().join("rejected-spans");
    std::fs::create_dir_all(&rejected).unwrap();
    for spans in ["0", "4", "256", "invalid"] {
        let out = sb.run(&[
            "--lexical-spans",
            spans,
            "connect",
            "claude",
            "--dir",
            rejected.to_str().unwrap(),
        ]);
        assert_eq!(out.code, 2, "{out:?}");
        assert_eq!(std::fs::read_dir(&rejected).unwrap().count(), 0);
    }
}

#[test]
fn relative_home_override_is_rejected_without_changes_or_value_disclosure() {
    for client in ["claude", "codex", "cursor"] {
        let sb = Sandbox::new();
        let project = sb.work().join("relative-home-client");
        std::fs::create_dir_all(&project).unwrap();
        let relative_home = "KNOWELL_CANARY_relative_home";
        let output = sb
            .command(&["connect", client, "--dir", project.to_str().unwrap()])
            .env("KNOWELL_HOME", relative_home)
            .output()
            .unwrap();
        let result = Run::from(output);
        assert_eq!(result.code, 2, "{result:?}");
        assert!(result.stderr.contains("KNOWELL_HOME to an absolute path"));
        assert!(!result.all().contains(relative_home));
        assert_eq!(std::fs::read_dir(&project).unwrap().count(), 0);
        assert!(!sb.work().join(relative_home).exists());
    }
}
