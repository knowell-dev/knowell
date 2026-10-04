use std::fs;
use std::path::{Path, PathBuf};

use pretty_assertions::assert_eq;
use serde_json::{Value, json};

use super::*;
use crate::edit::backup_path;

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let project = tmp.path().join("project");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&project).unwrap();
    Env {
        _tmp: tmp,
        home,
        project,
    }
}

fn opts(e: &Env, scope: Scope) -> ConnectOptions {
    let mut o = ConnectOptions::new(&e.home, &e.project);
    o.scope = scope;
    o
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

fn json_of(path: &Path) -> Value {
    serde_json::from_str(&read(path)).unwrap()
}

/// Files a connect is expected to create for (client, scope).
fn expected_files(e: &Env, client: Client, scope: Scope) -> Vec<PathBuf> {
    let (h, p) = (&e.home, &e.project);
    match (client, scope) {
        (Client::Codex, Scope::User) => {
            vec![h.join(".codex/config.toml"), h.join(".codex/AGENTS.md")]
        }
        (Client::Codex, Scope::Project) => vec![p.join(".codex/config.toml"), p.join("AGENTS.md")],
        (Client::Claude, Scope::User) => vec![
            h.join(".claude.json"),
            h.join(".claude/settings.json"),
            h.join(".claude/CLAUDE.md"),
        ],
        (Client::Claude, Scope::Project) => vec![
            p.join(".mcp.json"),
            p.join(".claude/settings.json"),
            p.join("CLAUDE.md"),
        ],
        (Client::Cursor, Scope::User) => {
            vec![
                h.join(".cursor/mcp.json"),
                p.join(".cursor/rules/knowell.mdc"),
            ]
        }
        (Client::Cursor, Scope::Project) => {
            vec![
                p.join(".cursor/mcp.json"),
                p.join(".cursor/rules/knowell.mdc"),
            ]
        }
    }
}

const CLIENTS: [Client; 3] = [Client::Codex, Client::Claude, Client::Cursor];
const SCOPES: [Scope; 2] = [Scope::User, Scope::Project];

fn snapshot(dir: &Path) -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push((p.clone(), fs::read_to_string(&p).unwrap_or_default()));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out
}

#[test]
fn connect_is_idempotent_and_disconnect_removes_everything() {
    for client in CLIENTS {
        for scope in SCOPES {
            let e = env();
            let o = opts(&e, scope);
            let report = connect(client, &o).unwrap();
            let mut changed = report.changed_files.clone();
            changed.sort();
            let mut expected = expected_files(&e, client, scope);
            expected.sort();
            assert_eq!(changed, expected, "{client:?} {scope:?}");
            assert_eq!(report.diffs.len(), expected.len());
            assert!(report.backups.is_empty(), "nothing pre-existed");
            for f in &expected {
                assert!(f.exists(), "{}", f.display());
            }

            let before = snapshot(&e.home)
                .into_iter()
                .chain(snapshot(&e.project))
                .collect::<Vec<_>>();
            let again = connect(client, &o).unwrap();
            assert!(
                again.changed_files.is_empty(),
                "{client:?} {scope:?}: {again:?}"
            );
            assert!(again.notes.iter().any(|n| n.contains("already connected")));
            let after = snapshot(&e.home)
                .into_iter()
                .chain(snapshot(&e.project))
                .collect::<Vec<_>>();
            assert_eq!(before, after);

            let gone = disconnect(client, &o).unwrap();
            assert_eq!(
                gone.changed_files.len(),
                expected.len(),
                "{client:?} {scope:?}"
            );
            for f in &expected {
                assert!(!f.exists(), "left behind: {}", f.display());
            }
            assert!(disconnect(client, &o).unwrap().changed_files.is_empty());
        }
    }
}

#[test]
fn dry_run_writes_nothing_but_reports_everything() {
    for client in CLIENTS {
        let e = env();
        let mut o = opts(&e, Scope::Project);
        o.dry_run = true;
        let report = connect(client, &o).unwrap();
        assert!(snapshot(&e.project).is_empty() && snapshot(&e.home).is_empty());
        assert_eq!(
            report.changed_files.len(),
            expected_files(&e, client, Scope::Project).len()
        );
        for d in &report.diffs {
            assert!(
                d.diff.contains("+++ ") && d.diff.contains("\n+"),
                "{}",
                d.diff
            );
        }
    }
}

#[test]
fn dry_run_disconnect_keeps_files() {
    let e = env();
    let o = opts(&e, Scope::Project);
    connect(Client::Claude, &o).unwrap();
    let before = snapshot(&e.project);
    let mut dry = o.clone();
    dry.dry_run = true;
    let report = disconnect(Client::Claude, &dry).unwrap();
    assert_eq!(report.changed_files.len(), 3);
    assert_eq!(before, snapshot(&e.project));
}

#[test]
fn claude_project_merges_existing_files_and_restores_them() {
    let e = env();
    let mcp = e.project.join(".mcp.json");
    let settings = e.project.join(".claude/settings.json");
    let md = e.project.join("CLAUDE.md");
    let mcp_text = "{\n  \"mcpServers\": {\n    \"other\": { \"command\": \"x\" }\n  }\n}\n";
    let settings_text = r#"{
  "permissions": { "allow": ["Bash(ls)"] },
  "hooks": {
    "SessionStart": [
      { "matcher": "startup", "hooks": [ { "type": "command", "command": "echo hi" } ] }
    ],
    "Stop": [ { "hooks": [ { "type": "command", "command": "echo bye" } ] } ]
  }
}
"#;
    write(&mcp, mcp_text);
    write(&settings, settings_text);
    write(&md, "# Project rules\n\nBe nice.\n");

    let o = opts(&e, Scope::Project);
    let report = connect(Client::Claude, &o).unwrap();
    assert_eq!(report.backups.len(), 3);
    assert_eq!(read(&backup_path(&mcp)), mcp_text);

    let v = json_of(&mcp);
    assert_eq!(v["mcpServers"]["other"], json!({"command": "x"}));
    assert_eq!(
        v["mcpServers"]["knowell"],
        json!({"type": "stdio", "command": "know", "args": ["mcp", "--output-mode", "source"]})
    );
    let s = json_of(&settings);
    assert_eq!(s["permissions"]["allow"], json!(["Bash(ls)"]));
    assert_eq!(s["hooks"]["Stop"][0]["hooks"][0]["command"], "echo bye");
    let groups = s["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["hooks"][0]["command"], "echo hi");
    assert_eq!(groups[1]["hooks"][0]["command"], "know");
    assert_eq!(
        groups[1]["hooks"][0]["args"],
        json!(["context", "--session-start"])
    );
    let text = read(&md);
    assert!(text.starts_with("# Project rules\n\nBe nice.\n\n<!-- knowell:begin connect -->"));
    assert!(text.contains("open_workspace") && text.contains("untrusted data"));

    disconnect(Client::Claude, &o).unwrap();
    assert_eq!(
        json_of(&mcp),
        serde_json::from_str::<Value>(mcp_text).unwrap()
    );
    assert_eq!(
        json_of(&settings),
        serde_json::from_str::<Value>(settings_text).unwrap()
    );
    assert_eq!(read(&md), "# Project rules\n\nBe nice.\n");
}

#[test]
fn claude_hook_command_change_replaces_instead_of_duplicating() {
    let e = env();
    let mut o = opts(&e, Scope::Project);
    connect(Client::Claude, &o).unwrap();
    o.command = "npx".into();
    o.args = vec!["-y".into(), "knowell".into(), "mcp".into()];
    let report = connect(Client::Claude, &o).unwrap();
    assert!(!report.changed_files.is_empty());
    let s = json_of(&e.project.join(".claude/settings.json"));
    let groups = s["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["hooks"][0]["command"], "npx");
    assert_eq!(
        groups[0]["hooks"][0]["args"],
        json!(["-y", "knowell", "context", "--session-start"])
    );
    let m = json_of(&e.project.join(".mcp.json"));
    assert_eq!(m["mcpServers"]["knowell"]["command"], "npx");
    assert_eq!(
        m["mcpServers"]["knowell"]["args"],
        json!(["-y", "knowell", "mcp"])
    );
}

#[test]
fn claude_user_scope_uses_dot_claude_json() {
    let e = env();
    write(
        &e.home.join(".claude.json"),
        "{\"numStartups\": 3, \"mcpServers\": {\"a\": {}}}",
    );
    let o = opts(&e, Scope::User);
    connect(Client::Claude, &o).unwrap();
    let v = json_of(&e.home.join(".claude.json"));
    assert_eq!(v["numStartups"], 3);
    assert!(v["mcpServers"]["a"].is_object() && v["mcpServers"]["knowell"].is_object());
    assert!(!e.project.join(".mcp.json").exists());
    disconnect(Client::Claude, &o).unwrap();
    let v = json_of(&e.home.join(".claude.json"));
    assert_eq!(v, json!({"numStartups": 3, "mcpServers": {"a": {}}}));
}

#[test]
fn cursor_merges_and_removes_only_its_entry() {
    let e = env();
    let mcp = e.project.join(".cursor/mcp.json");
    write(
        &mcp,
        "{\"mcpServers\": {\"other\": {\"url\": \"http://localhost:1/mcp\"}}}",
    );
    let mut o = opts(&e, Scope::Project);
    o.env_names = vec!["KNOWELL_HUB_TOKEN".into()];
    connect(Client::Cursor, &o).unwrap();
    let v = json_of(&mcp);
    assert_eq!(v["mcpServers"]["other"]["url"], "http://localhost:1/mcp");
    assert_eq!(
        v["mcpServers"]["knowell"],
        json!({"command": "know", "args": ["mcp", "--output-mode", "source"], "env": {"KNOWELL_HUB_TOKEN": "${env:KNOWELL_HUB_TOKEN}"}})
    );
    let rule = read(&e.project.join(".cursor/rules/knowell.mdc"));
    assert!(rule.starts_with("---\n") && rule.contains("alwaysApply: true"));
    assert!(rule.contains("<!-- knowell:begin connect -->"));
    disconnect(Client::Cursor, &o).unwrap();
    assert_eq!(
        json_of(&mcp),
        json!({"mcpServers": {"other": {"url": "http://localhost:1/mcp"}}})
    );
    assert!(!e.project.join(".cursor/rules/knowell.mdc").exists());
}

#[test]
fn cursor_foreign_rule_file_is_a_conflict_and_survives() {
    let e = env();
    let rule = e.project.join(".cursor/rules/knowell.mdc");
    write(&rule, "my own rule\n");
    let err = connect(Client::Cursor, &opts(&e, Scope::Project)).unwrap_err();
    assert!(matches!(err, SetupError::Conflict { .. }));
    assert!(
        !e.project.join(".cursor/mcp.json").exists(),
        "nothing written on conflict"
    );
    disconnect(Client::Cursor, &opts(&e, Scope::Project)).unwrap();
    assert_eq!(read(&rule), "my own rule\n");
}

#[test]
fn codex_preserves_comments_and_other_tables() {
    let e = env();
    let cfg = e.home.join(".codex/config.toml");
    let original =
        "# my settings\nmodel = \"gpt-x\"\n\n[mcp_servers.other]\ncommand = \"other\" # keep me\n";
    write(&cfg, original);
    let mut o = opts(&e, Scope::User);
    o.command = "C:\\tools\\know.exe".into();
    o.env_names = vec!["KNOWELL_HUB_TOKEN".into()];
    connect(Client::Codex, &o).unwrap();
    let text = read(&cfg);
    assert!(text.starts_with(original), "{text}");
    let table: toml::Table = text.parse().unwrap();
    let k = &table["mcp_servers"]["knowell"];
    assert_eq!(k["command"].as_str(), Some("C:\\tools\\know.exe"));
    assert_eq!(k["args"].as_array().unwrap().len(), 3);
    assert_eq!(k["env_vars"][0].as_str(), Some("KNOWELL_HUB_TOKEN"));
    assert!(table["mcp_servers"]["other"].is_table());
    assert!(
        !text.contains("KNOWELL_HUB_TOKEN\" ="),
        "names only, no values"
    );
    assert_eq!(read(&backup_path(&cfg)), original);

    // Changing the command rewrites the block in place.
    o.command = "know".into();
    connect(Client::Codex, &o).unwrap();
    let text2 = read(&cfg);
    assert_eq!(text2.matches("[mcp_servers.knowell]").count(), 1);

    disconnect(Client::Codex, &o).unwrap();
    assert_eq!(read(&cfg), original);
}

#[test]
fn codex_agents_md_block_is_merged_and_removed() {
    let e = env();
    let agents = e.project.join("AGENTS.md");
    write(&agents, "# Agents\n\nRule one.\n");
    let o = opts(&e, Scope::Project);
    connect(Client::Codex, &o).unwrap();
    let text = read(&agents);
    assert_eq!(text.matches("knowell:begin connect").count(), 1);
    assert!(text.contains("`trace_flow`") && text.contains("`resume_task`"));
    assert!(e.project.join(".codex/config.toml").exists());
    let report = connect(Client::Codex, &o).unwrap();
    assert!(report.changed_files.is_empty());
    disconnect(Client::Codex, &o).unwrap();
    assert_eq!(read(&agents), "# Agents\n\nRule one.\n");
}

#[test]
fn codex_conflicts_and_invalid_files_change_nothing() {
    let e = env();
    let cfg = e.project.join(".codex/config.toml");
    write(&cfg, "[mcp_servers.knowell]\ncommand = \"mine\"\n");
    let err = connect(Client::Codex, &opts(&e, Scope::Project)).unwrap_err();
    assert!(matches!(err, SetupError::Conflict { .. }));
    assert!(!e.project.join("AGENTS.md").exists());
    // Disconnect leaves a hand-written table alone.
    disconnect(Client::Codex, &opts(&e, Scope::Project)).unwrap();
    assert!(read(&cfg).contains("mine"));

    write(&cfg, "mcp_servers = { other = { command = \"x\" } }\n");
    assert!(matches!(
        connect(Client::Codex, &opts(&e, Scope::Project)),
        Err(SetupError::Conflict { .. })
    ));

    write(&cfg, "this is = = not toml SECRETVALUE");
    let err = connect(Client::Codex, &opts(&e, Scope::Project)).unwrap_err();
    assert!(matches!(err, SetupError::InvalidToml { .. }));
    assert!(!err.to_string().contains("SECRETVALUE"));
}

#[test]
fn invalid_json_is_reported_and_left_untouched() {
    let e = env();
    let mcp = e.project.join(".mcp.json");
    write(&mcp, "{ \"mcpServers\": ");
    let err = connect(Client::Claude, &opts(&e, Scope::Project)).unwrap_err();
    assert!(matches!(err, SetupError::InvalidJson { .. }));
    assert_eq!(read(&mcp), "{ \"mcpServers\": ");
    assert!(!e.project.join("CLAUDE.md").exists());

    write(&mcp, "{\"mcpServers\": []}");
    assert!(matches!(
        connect(Client::Claude, &opts(&e, Scope::Project)),
        Err(SetupError::Conflict { .. })
    ));
}

#[test]
fn unbalanced_markers_are_a_conflict() {
    let e = env();
    write(
        &e.project.join("CLAUDE.md"),
        "<!-- knowell:begin connect -->\nhalf\n",
    );
    assert!(matches!(
        connect(Client::Claude, &opts(&e, Scope::Project)),
        Err(SetupError::Conflict { .. })
    ));
    assert!(!e.project.join(".mcp.json").exists());
}

#[test]
fn env_names_are_validated_and_never_echoed() {
    let e = env();
    let mut o = opts(&e, Scope::Project);
    o.env_names = vec!["TOKEN=KNOWELL_CANARY_value".into()];
    let err = connect(Client::Claude, &o).unwrap_err();
    assert!(matches!(err, SetupError::InvalidInput(_)));
    assert!(!err.to_string().contains("KNOWELL_CANARY"));
    o.env_names.clear();
    o.command = " ".into();
    assert!(connect(Client::Claude, &o).is_err());
}

#[test]
fn client_reports_mask_values_while_preserving_exact_configs_and_backups() {
    let arbitrary_env = "KNOWELL_CANARY_arbitrary_env_value";
    let arbitrary_header = "KNOWELL_CANARY_arbitrary_header_value";
    let previous_token = "KNOWELL_CANARY_previous_token";
    let provider_key = format!("ghp_{}", "A".repeat(36));
    for client in [Client::Claude, Client::Codex, Client::Cursor] {
        let e = env();
        let config = e.project.join(match client {
            Client::Claude => ".mcp.json",
            Client::Codex => ".codex/config.toml",
            Client::Cursor => ".cursor/mcp.json",
        });
        let before = if client == Client::Codex {
            format!(
                "[mcp_servers.other]\ncommand = 'keep-other'\nargs = ['--literal', 'keep-argv']\nenv_vars = ['KEEP_NAME']\nenv = {{ ARBITRARY = '{arbitrary_env}' }}\nhttp_headers = {{ Arbitrary = '{arbitrary_header}', Authorization = 'zzq' }}\napi_key = '{previous_token}'\ndescription = '{provider_key}'\n"
            )
        } else {
            serde_json::to_string(&json!({
                "mcpServers": {
                    "other": {
                        "command": "keep-other", "args": ["--literal", "keep-argv"],
                        "env": {"ARBITRARY": arbitrary_env},
                        "headers": {"Arbitrary": arbitrary_header, "Authorization": "zzq"},
                        "api_key": previous_token, "description": provider_key,
                    },
                    "knowell": {"command": "old-know", "env": {"TOKEN": previous_token}},
                }
            }))
            .unwrap()
        };
        write(&config, &before);
        let markdown = e.project.join(match client {
            Client::Claude => "CLAUDE.md",
            Client::Codex => "AGENTS.md",
            Client::Cursor => ".cursor/rules/knowell.mdc",
        });
        let original_markdown = if client == Client::Cursor {
            format!(
                "{}\nSynthetic provider fixture: {provider_key}\n",
                instruction_block()
            )
        } else {
            format!("# Keep instruction\n\nSynthetic provider fixture: {provider_key}\n")
        };
        write(&markdown, &original_markdown);
        let mut options = opts(&e, Scope::Project);
        options.env_names = vec!["KEEP_NAME".to_owned()];
        options.dry_run = true;
        let dry = connect(client, &options).unwrap();
        assert_eq!(read(&config), before);
        assert_eq!(read(&markdown), original_markdown);
        assert!(!backup_path(&config).exists());
        let check = |report: &ConnectReport| {
            for diff in &report.diffs {
                for value in [
                    arbitrary_env,
                    arbitrary_header,
                    previous_token,
                    "zzq",
                    provider_key.as_str(),
                ] {
                    assert!(
                        !diff.diff.contains(value),
                        "report leaked a synthetic classified value"
                    );
                }
            }
        };
        check(&dry);
        assert!(dry.diffs.iter().any(|diff| diff.diff.contains("mcp")));
        options.dry_run = false;
        let connected = connect(client, &options).unwrap();
        check(&connected);
        assert_eq!(read(&backup_path(&config)), before);
        assert_eq!(read(&backup_path(&markdown)), original_markdown);
        if client == Client::Codex {
            let original: toml::Table = before.parse().unwrap();
            let current: toml::Table = read(&config).parse().unwrap();
            assert_eq!(
                current["mcp_servers"]["other"],
                original["mcp_servers"]["other"]
            );
            assert_eq!(
                current["mcp_servers"]["knowell"]["env_vars"][0].as_str(),
                Some("KEEP_NAME")
            );
        } else {
            let original: Value = serde_json::from_str(&before).unwrap();
            let current = json_of(&config);
            assert_eq!(
                current["mcpServers"]["other"],
                original["mcpServers"]["other"]
            );
        }
        let disconnected = disconnect(client, &options).unwrap();
        check(&disconnected);
        assert_eq!(read(&backup_path(&config)), before);
        if client == Client::Cursor {
            assert!(!markdown.exists());
        } else {
            assert_eq!(read(&markdown), original_markdown);
        }
        assert!(read(&config).contains(arbitrary_env));
    }
}

#[test]
fn hook_arguments_preserve_launcher_paths_without_shell_interpolation() {
    let e = env();
    let mut o = opts(&e, Scope::Project);
    o.command = "C:\\Synthetic Tools\\knöw\\know.exe".into();
    let engine = "C:\\Synthetic Settings\\engine $(ignored) `literal`.toml";
    let workspace = "C:\\Örnek Proje\\knowell's.toml";
    o.args = vec![
        "--config".into(),
        engine.into(),
        "--workspace".into(),
        workspace.into(),
        "mcp".into(),
    ];
    connect(Client::Claude, &o).unwrap();
    let hook = json_of(&e.project.join(".claude/settings.json"));
    assert_eq!(
        hook["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        o.command
    );
    assert_eq!(
        hook["hooks"]["SessionStart"][0]["hooks"][0]["args"],
        json!([
            "--config",
            engine,
            "--workspace",
            workspace,
            "context",
            "--session-start"
        ])
    );
    o.hook_command = Some("know context --session-start --quiet".into());
    connect(Client::Claude, &o).unwrap();
    let hook = json_of(&e.project.join(".claude/settings.json"));
    assert_eq!(
        hook["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        "know context --session-start --quiet"
    );
    assert!(
        hook["hooks"]["SessionStart"][0]["hooks"][0]
            .get("args")
            .is_none()
    );
}

#[test]
fn selected_launch_arguments_and_environment_names_round_trip_for_every_client() {
    let e = env();
    let args = vec![
        "--config".to_owned(),
        "Synthetic Settings/engine.toml".to_owned(),
        "--workspace".to_owned(),
        "Örnek Proje/knowell.toml".to_owned(),
        "mcp".to_owned(),
    ];
    for client in CLIENTS {
        let mut o = opts(&e, Scope::Project);
        o.args = args.clone();
        o.env_names = vec!["KNOWELL_HOME".into(), "KNOWELL_CANARY_PROVIDER".into()];
        connect(client, &o).unwrap();
        match client {
            Client::Codex => {
                let table: toml::Table =
                    read(&e.project.join(".codex/config.toml")).parse().unwrap();
                let entry = &table["mcp_servers"]["knowell"];
                let actual: Vec<_> = entry["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap())
                    .collect();
                assert_eq!(actual, args);
                assert_eq!(entry["env_vars"][0].as_str(), Some("KNOWELL_HOME"));
            }
            Client::Claude | Client::Cursor => {
                let path = if client == Client::Claude {
                    ".mcp.json"
                } else {
                    ".cursor/mcp.json"
                };
                let config = json_of(&e.project.join(path));
                let entry = &config["mcpServers"]["knowell"];
                assert_eq!(entry["args"], json!(args));
                let home = if client == Client::Claude {
                    "${KNOWELL_HOME}"
                } else {
                    "${env:KNOWELL_HOME}"
                };
                assert_eq!(entry["env"]["KNOWELL_HOME"], home);
            }
        }
        assert!(connect(client, &o).unwrap().changed_files.is_empty());
        disconnect(client, &o).unwrap();
    }
}

#[test]
fn legacy_claude_shell_hook_is_replaced_once_without_losing_other_hooks() {
    let e = env();
    let path = e.project.join(".claude/settings.json");
    write(
        &path,
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"know context --session-start","statusMessage":"Knowell: loading workspace context"},{"type":"command","command":"other-start"}]}]}}"#,
    );
    let o = opts(&e, Scope::Project);
    connect(Client::Claude, &o).unwrap();
    let config = json_of(&path);
    let groups = config["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["hooks"][0]["command"], "other-start");
    assert_eq!(
        groups[1]["hooks"][0]["args"],
        json!(["context", "--session-start"])
    );
    assert!(
        connect(Client::Claude, &o)
            .unwrap()
            .changed_files
            .is_empty()
    );
    disconnect(Client::Claude, &o).unwrap();
    assert_eq!(
        json_of(&path),
        json!({"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"other-start"}]}]}})
    );
}

#[test]
fn explicit_windows_npx_launcher_is_preserved_for_mcp_and_hook() {
    let e = env();
    let mut o = opts(&e, Scope::Project);
    o.command = "cmd".into();
    o.args = vec![
        "/c".into(),
        "npx".into(),
        "-y".into(),
        "knowell".into(),
        "mcp".into(),
    ];
    connect(Client::Claude, &o).unwrap();
    let mcp = json_of(&e.project.join(".mcp.json"));
    assert_eq!(mcp["mcpServers"]["knowell"]["command"], "cmd");
    assert_eq!(
        mcp["mcpServers"]["knowell"]["args"],
        json!(["/c", "npx", "-y", "knowell", "mcp"])
    );
    let settings = json_of(&e.project.join(".claude/settings.json"));
    assert_eq!(
        settings["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        "cmd"
    );
    assert_eq!(
        settings["hooks"]["SessionStart"][0]["hooks"][0]["args"],
        json!(["/c", "npx", "-y", "knowell", "context", "--session-start"])
    );
}

#[test]
fn source_mode_hook_preserves_global_options_and_literal_mcp_paths() {
    let e = env();
    let mut o = opts(&e, Scope::Project);
    o.command = "cmd".into();
    o.args = [
        "/c",
        "npx",
        "-y",
        "knowell",
        "--config",
        "mcp",
        "--parse-cache",
        "mcp",
        "--output-mode",
        "source",
        "--workspace",
        "mcp",
        "--lexical-spans",
        "2",
        "--quiet",
    ]
    .map(str::to_owned)
    .to_vec();
    connect(Client::Claude, &o).unwrap();
    let settings = json_of(&e.project.join(".claude/settings.json"));
    assert_eq!(
        settings["hooks"]["SessionStart"][0]["hooks"][0]["args"],
        json!([
            "/c",
            "npx",
            "-y",
            "knowell",
            "--config",
            "mcp",
            "--parse-cache",
            "context",
            "--session-start",
            "--workspace",
            "mcp",
            "--lexical-spans",
            "2",
            "--quiet"
        ])
    );
    assert!(
        connect(Client::Claude, &o)
            .unwrap()
            .changed_files
            .is_empty()
    );
}

#[test]
fn joined_output_modes_are_removed_only_from_mcp_arguments() {
    let e = env();
    for mode in ["source", "compact", "full"] {
        let mut o = opts(&e, Scope::Project);
        o.args = vec![
            "--workspace".into(),
            "--output-mode=literal-path".into(),
            "mcp".into(),
            format!("--output-mode={mode}"),
        ];
        assert_eq!(
            o.hook_args().unwrap(),
            [
                "--workspace",
                "--output-mode=literal-path",
                "context",
                "--session-start"
            ]
            .map(str::to_owned)
        );
    }
}

#[test]
fn malformed_mcp_hook_arguments_are_refused_without_writing_or_echoing_values() {
    for args in [
        vec!["mcp", "--output-mode"],
        vec!["mcp", "--output-mode", "KNOWELL_CANARY_invalid_mode"],
        vec!["mcp", "--output-mode=KNOWELL_CANARY_invalid_mode"],
        vec!["--workspace", "mcp"],
        vec!["mcp", "--config"],
    ] {
        let e = env();
        let mut o = opts(&e, Scope::Project);
        o.args = args.into_iter().map(str::to_owned).collect();
        let before = snapshot(&e.project);
        let error = connect(Client::Claude, &o).unwrap_err();
        assert!(matches!(error, SetupError::InvalidInput(_)));
        assert!(!error.to_string().contains("KNOWELL_CANARY"));
        assert_eq!(snapshot(&e.project), before);
    }
}

#[test]
fn disconnect_does_not_parse_an_obsolete_launcher_and_preserves_foreign_entries() {
    let e = env();
    let mcp = e.project.join(".mcp.json");
    write(
        &mcp,
        r#"{"mcpServers":{"other":{"command":"other-server"}}}"#,
    );
    let mut o = opts(&e, Scope::Project);
    connect(Client::Claude, &o).unwrap();
    o.args = vec!["mcp".into(), "--output-mode".into()];
    disconnect(Client::Claude, &o).unwrap();
    assert_eq!(
        json_of(&mcp),
        json!({"mcpServers":{"other":{"command":"other-server"}}})
    );
    assert!(!e.project.join(".claude/settings.json").exists());
}

#[test]
fn client_names_parse() {
    assert_eq!("Claude-Code".parse::<Client>().unwrap(), Client::Claude);
    assert_eq!("codex".parse::<Client>().unwrap().as_str(), "codex");
    assert!("vim".parse::<Client>().is_err());
}

#[test]
fn instruction_block_is_short_and_marked() {
    let b = instruction_block();
    assert!(b.lines().count() <= 16, "{}", b.lines().count());
    assert!(b.starts_with("<!-- knowell:begin connect -->"));
    assert!(b.ends_with("<!-- knowell:end connect -->"));
    for tool in [
        "open_workspace",
        "search",
        "inspect_symbol",
        "trace_flow",
        "build_context",
        "fetch",
        "write_memory",
        "save_checkpoint",
        "resume_task",
    ] {
        assert!(b.contains(tool), "{tool}");
    }
    assert!(b.contains("complementary passages"));
    assert!(b.contains("same pinned source"));
    assert!(!b.contains("At the start of every session call"));
}
