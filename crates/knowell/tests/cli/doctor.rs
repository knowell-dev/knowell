//! `know doctor`.

use crate::common::{Sandbox, admin_url, password_of};

const CANARY_PASSWORD: &str = "KNOWELL_CANARY_pw_51f0c2";

fn checks(stdout: &str) -> (String, Vec<serde_json::Value>) {
    let value: serde_json::Value = serde_json::from_str(stdout).unwrap();
    let status = value["status"].as_str().unwrap().to_owned();
    let checks = value["checks"].as_array().unwrap().clone();
    for check in &checks {
        assert!(check["name"].is_string(), "{check}");
        assert!(
            ["ok", "warn", "fail"].contains(&check["status"].as_str().unwrap()),
            "{check}"
        );
        assert!(check["summary"].is_string(), "{check}");
        if check["status"] != "ok" {
            assert!(check["remediation"].is_string(), "{check}");
        }
    }
    (status, checks)
}

fn find<'a>(checks: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
    checks
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no check {name}: {checks:?}"))
}

#[test]
fn reports_a_missing_engine_config_as_failure() {
    let sb = Sandbox::new();
    let out = sb.run(&["doctor", "--json"]);
    assert_eq!(out.code, 1, "{out:?}");
    let (status, checks) = checks(&out.stdout);
    assert_eq!(status, "fail");
    assert_eq!(find(&checks, "engine_config")["status"], "fail");
    for name in ["workspace_config", "git", "panel", "agent_clients"] {
        find(&checks, name);
    }

    let text = sb.run(&["doctor"]);
    assert_eq!(text.code, 1, "{text:?}");
    assert!(text.stdout.contains("[FAIL] engine_config"), "{text:?}");
    assert!(text.stdout.contains("know init"), "{text:?}");
}

#[test]
fn never_prints_the_database_url() {
    let mut sb = Sandbox::new();
    sb.write_engine(
        "version = 1\n[database]\nmode = \"external\"\nurl = \"env:KNOWELL_CLI_CANARY_URL\"\n",
    );
    // Port 1 refuses connections, so the check fails with a driver error.
    sb.set_env(
        "KNOWELL_CLI_CANARY_URL",
        &format!("postgres://knowell:{CANARY_PASSWORD}@127.0.0.1:1/knowell"),
    );
    for args in [
        &["doctor", "--json"][..],
        &["doctor"][..],
        &["-v", "doctor"][..],
    ] {
        let out = sb.run(args);
        assert_eq!(out.code, 1, "{out:?}");
        assert!(
            !out.all().contains(CANARY_PASSWORD),
            "password leaked: {out:?}"
        );
        assert!(
            !out.all().contains("127.0.0.1:1/knowell"),
            "url leaked: {out:?}"
        );
    }
    let out = sb.run(&["doctor", "--json"]);
    let (_, checks) = checks(&out.stdout);
    assert_eq!(find(&checks, "database")["status"], "fail");
}

#[test]
fn unset_references_are_named_not_resolved() {
    let sb = Sandbox::new();
    sb.write_engine(
        "version = 1\n[database]\nmode = \"external\"\nurl = \"env:KNOWELL_CLI_UNSET_URL\"\n\
[providers.gemini]\nkind = \"gemini\"\napi_key = \"env:KNOWELL_CLI_UNSET_KEY\"\n",
    );
    let out = sb.run(&["doctor", "--json"]);
    assert_eq!(out.code, 1, "{out:?}");
    let (_, checks) = checks(&out.stdout);
    let db = find(&checks, "database");
    assert_eq!(db["status"], "fail");
    assert!(
        db["summary"]
            .as_str()
            .unwrap()
            .contains("env:KNOWELL_CLI_UNSET_URL")
    );
    let providers = find(&checks, "providers");
    assert_eq!(providers["status"], "fail");
    assert!(
        providers["summary"]
            .as_str()
            .unwrap()
            .contains("env:KNOWELL_CLI_UNSET_KEY")
    );
}

#[test]
fn healthy_external_database() {
    let Some(admin) = admin_url("doctor::healthy_external_database") else {
        return;
    };
    let db = crate::common::ScratchDb::create(&admin);
    let mut sb = Sandbox::new();
    sb.set_env("KNOWELL_CLI_DB_URL", &db.url);
    let init = sb.run(&[
        "init",
        "--database",
        "external",
        "--database-url-ref",
        "env:KNOWELL_CLI_DB_URL",
    ]);
    assert_eq!(init.code, 0, "{init:?}");
    let out = sb.run(&["doctor", "--json"]);
    let password = password_of(&db.url);
    if let Some(password) = &password {
        assert!(!out.all().contains(password.as_str()), "password leaked");
    }
    assert!(!out.all().contains(&db.url), "url leaked");
    let (_, checks) = checks(&out.stdout);
    assert_eq!(find(&checks, "database")["status"], "ok", "{out:?}");
    assert_eq!(find(&checks, "pgvector")["status"], "ok", "{out:?}");
    // Only warnings remain (no workspace, no agent connected).
    assert_eq!(out.code, 0, "{out:?}");
}
