//! `know init`, `workspace add|list`, `serve`, `backup|restore` against real
//! databases.

use crate::common::{
    Sandbox, ScratchDb, Server, admin_url, generate_fixture, git_available, http_get, http_request,
    password_of, session_cookie,
};

#[test]
fn init_refuses_a_pasted_url_without_echoing_it() {
    let sb = Sandbox::new();
    let canary = "KNOWELL_CANARY_pw_9d2e";
    let url = format!("postgres://knowell:{canary}@127.0.0.1:5432/knowell");
    let out = sb.run(&["init", "--database", "external", "--database-url-ref", &url]);
    assert_eq!(out.code, 2, "{out:?}");
    assert!(!out.all().contains(canary), "{out:?}");
    assert!(out.stderr.contains("reference"), "{out:?}");
    assert!(!sb.engine_config().exists());

    let missing = sb.run(&["init", "--database", "external"]);
    assert_eq!(missing.code, 2, "{missing:?}");
    assert!(missing.stderr.contains("--database-url-ref"), "{missing:?}");
}

#[test]
fn init_external_reports_an_unset_reference() {
    let sb = Sandbox::new();
    let out = sb.run(&[
        "init",
        "--database",
        "external",
        "--database-url-ref",
        "env:KNOWELL_CLI_NOT_SET",
    ]);
    assert_eq!(out.code, 1, "{out:?}");
    assert!(out.stdout.contains("env:KNOWELL_CLI_NOT_SET"), "{out:?}");
    // The configuration (a reference only) is kept for the next attempt.
    let config = std::fs::read_to_string(sb.engine_config()).unwrap();
    assert!(config.contains("env:KNOWELL_CLI_NOT_SET"), "{config}");
    knowell_config::load_engine(&sb.engine_config()).unwrap();

    // Without flags, init continues with the existing settings.
    let retry = sb.run(&["init"]);
    assert_eq!(retry.code, 1, "{retry:?}");
    assert!(
        retry.stdout.contains("env:KNOWELL_CLI_NOT_SET"),
        "{retry:?}"
    );

    // A different mode or reference is refused, not applied.
    let other = sb.run(&["init", "--database", "managed"]);
    assert_eq!(other.code, 2, "{other:?}");
    assert!(
        other.stderr.contains("already uses database mode"),
        "{other:?}"
    );
    let other_ref = sb.run(&["init", "--database-url-ref", "env:KNOWELL_CLI_OTHER"]);
    assert_eq!(other_ref.code, 2, "{other_ref:?}");
    let config_after = std::fs::read_to_string(sb.engine_config()).unwrap();
    assert_eq!(config_after, config);
}

#[test]
fn compose_prints_instructions_only() {
    let sb = Sandbox::new();
    let out = sb.run(&["init", "--database", "compose"]);
    assert_eq!(out.code, 0, "{out:?}");
    assert!(out.stdout.contains("docker compose up"), "{out:?}");
    assert!(!sb.engine_config().exists());
}

/// init → workspace import → workspace add → workspace list → serve, all
/// against a scratch database.
#[test]
fn external_database_end_to_end() {
    let Some(admin) = admin_url("database::external_database_end_to_end") else {
        return;
    };
    if !git_available() {
        eprintln!("skipping database::external_database_end_to_end: git is not available");
        return;
    }
    let db = ScratchDb::create(&admin);
    let password = password_of(&db.url);
    let mut sb = Sandbox::new();
    sb.set_env("KNOWELL_CLI_DB_URL", &db.url);
    let no_leak = |text: &str| {
        assert!(!text.contains(&db.url), "url leaked");
        if let Some(p) = &password {
            assert!(!text.contains(p.as_str()), "password leaked");
        }
    };

    let init = sb.run(&[
        "init",
        "--database",
        "external",
        "--database-url-ref",
        "env:KNOWELL_CLI_DB_URL",
    ]);
    no_leak(&init.all());
    assert_eq!(init.code, 0, "{init:?}");
    assert!(init.stdout.contains("schema up to date"), "{init:?}");
    let config = std::fs::read_to_string(sb.engine_config()).unwrap();
    no_leak(&config);
    assert!(config.contains("env:KNOWELL_CLI_DB_URL"));

    let again = sb.run(&[
        "init",
        "--database",
        "external",
        "--database-url-ref",
        "env:KNOWELL_CLI_DB_URL",
    ]);
    assert_eq!(again.code, 0, "{again:?}");
    assert!(again.stdout.contains("exists"), "{again:?}");

    // A workspace from the synthetic fixture.
    let ws_dir = sb.work().join("fixture");
    let repos = generate_fixture(&sb, &ws_dir);
    let import = sb.run(&[
        "workspace",
        "import",
        ws_dir.to_str().unwrap(),
        "--name",
        "fixture",
        "--track-current",
    ]);
    assert_eq!(import.code, 0, "{import:?}");

    let add = sb.run_in(&ws_dir, &["workspace", "add", "--json"]);
    no_leak(&add.all());
    assert_eq!(add.code, 0, "{add:?}");
    let added: serde_json::Value = serde_json::from_str(&add.stdout).unwrap();
    assert_eq!(added["workspace"], "fixture");
    assert_eq!(added["projects"], repos.len());
    assert_eq!(added["projects_created"], repos.len());
    assert_eq!(added["views_created"], repos.len());

    // Registering again is idempotent.
    let readd = sb.run_in(&ws_dir, &["workspace", "add", "--json"]);
    assert_eq!(readd.code, 0, "{readd:?}");
    let readded: serde_json::Value = serde_json::from_str(&readd.stdout).unwrap();
    assert_eq!(readded["projects_created"], 0);
    assert_eq!(readded["views_created"], 0);

    let list = sb.run(&["workspace", "list", "--json"]);
    no_leak(&list.all());
    assert_eq!(list.code, 0, "{list:?}");
    let listed: serde_json::Value = serde_json::from_str(&list.stdout).unwrap();
    let workspaces = listed["workspaces"].as_array().unwrap();
    assert_eq!(workspaces.len(), 1, "{listed}");
    let projects = workspaces[0]["projects"].as_array().unwrap();
    assert_eq!(projects.len(), repos.len());
    assert!(
        projects.iter().all(|p| p["views"][0] == "branch:main"),
        "{listed}"
    );
    assert!(
        workspaces[0]["file"]
            .as_str()
            .unwrap()
            .ends_with("knowell.toml")
    );

    // The server opens the store and answers.
    let server = Server::start(&sb, &[]);
    let (status, body) = http_get(server.addr, "/api/v1/health/live");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("ok"), "{body}");
    // The loopback panel acts as the local user, who administers the
    // organization through grants stored in the database.
    let (status, head, body) = http_request(server.addr, "/api/v1/session", None);
    assert_eq!(status, 200, "{body}");
    let cookie = session_cookie(&head).unwrap();
    let (status, _, body) = http_request(server.addr, "/api/v1/workspaces", Some(&cookie));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("fixture"), "{body}");
    let seen = server.lines.seen.join("\n");
    assert_eq!(server.stop(), 0);
    no_leak(&seen);
}

/// Stops the managed PostgreSQL of a sandbox when the test ends, also on
/// failure, so the temporary directory can be removed.
struct StopManaged(std::path::PathBuf);

impl Drop for StopManaged {
    fn drop(&mut self) {
        let config = knowell_pg_managed::ManagedConfig::new(&self.0);
        if let Ok(pg) = knowell_pg_managed::ManagedPostgres::new(config) {
            let _ = crate::common::block_on(pg.stop());
        }
    }
}

/// Downloads and runs PostgreSQL: run with `--ignored`.
#[test]
#[ignore = "downloads PostgreSQL binaries (network, about a minute)"]
fn managed_init_backup_restore() {
    let sb = Sandbox::new();
    let _stop = StopManaged(sb.knowell_home());
    let init = sb.run_to_files(&["init"]);
    // Without the pgvector bundle init completes everything but the schema
    // and says so; with it, it completes.
    match init.code {
        0 => assert!(init.stdout.contains("schema up to date"), "{init:?}"),
        1 => assert!(init.stdout.contains("pgvector: NOT AVAILABLE"), "{init:?}"),
        _ => panic!("{init:?}"),
    }
    let again = sb.run_to_files(&["init"]);
    assert_eq!(again.code, init.code, "init is not idempotent: {again:?}");
    assert!(again.stdout.contains("already initialised"), "{again:?}");

    let doctor = sb.run_to_files(&["doctor", "--json"]);
    let value: serde_json::Value = serde_json::from_str(&doctor.stdout).unwrap();
    let managed = value["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "managed_postgres")
        .unwrap()
        .clone();
    assert_eq!(managed["status"], "ok", "{doctor:?}");

    let dump = sb.work().join("knowell.dump");
    let backup = sb.run_to_files(&["backup", dump.to_str().unwrap()]);
    assert_eq!(backup.code, 0, "{backup:?}");
    assert!(dump.is_file());
    let exists = sb.run_to_files(&["restore", dump.to_str().unwrap()]);
    assert_eq!(exists.code, 1, "{exists:?}");
    assert!(exists.stderr.contains("--into"), "{exists:?}");
    let restore = sb.run_to_files(&["restore", dump.to_str().unwrap(), "--into", "knowell_copy"]);
    assert_eq!(restore.code, 0, "{restore:?}");
}

#[test]
fn backup_on_external_prints_guidance() {
    let sb = Sandbox::new();
    sb.write_engine(
        "version = 1\n[database]\nmode = \"external\"\nurl = \"env:KNOWELL_DATABASE_URL\"\n",
    );
    let out = sb.run(&["backup", "x.dump"]);
    assert_eq!(out.code, 1, "{out:?}");
    assert!(out.stdout.contains("pg_dump"), "{out:?}");
    assert!(out.stdout.contains("$KNOWELL_DATABASE_URL"), "{out:?}");
    let restore = sb.run(&["restore", "x.dump"]);
    assert_eq!(restore.code, 1, "{restore:?}");
    assert!(restore.stdout.contains("pg_restore"), "{restore:?}");
}
