//! Issue, use and revoke tokens through the real CLI and HTTP hub.

use crate::common::{Sandbox, ScratchDb, Server, admin_url};

const PEPPER: &str = "KNOWELL_CANARY_cli_pepper_0123456789";

fn configured(url: &str) -> Sandbox {
    let mut sb = Sandbox::new();
    sb.set_env("KNOWELL_CLI_DB_URL", url);
    sb.set_env("KNOWELL_CLI_PEPPER", PEPPER);
    sb.write_engine("version = 1\n[server]\nrole = 'hub'\ntoken_pepper = 'env:KNOWELL_CLI_PEPPER'\n[database]\nmode = 'external'\nurl = 'env:KNOWELL_CLI_DB_URL'\n");
    sb
}

#[test]
fn token_lifecycle_and_login_against_a_real_hub() {
    let Some(admin) = admin_url("token_lifecycle_and_login_against_a_real_hub") else {
        return;
    };
    let db = ScratchDb::create(&admin);
    let hub = configured(&db.url);
    let file = hub.work().join("test-token.secret");
    let created = hub.run(&[
        "token",
        "create",
        "--principal",
        "alice",
        "--create-user",
        "--role",
        "admin",
        "--output",
        file.to_str().unwrap(),
    ]);
    assert_eq!(created.code, 0, "{created:?}");
    assert!(!created.all().contains(PEPPER));
    let secret = std::fs::read_to_string(&file).unwrap();
    assert!(!created.all().contains(&secret));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let listed = hub.run(&["token", "list", "--principal", "alice"]);
    assert_eq!(listed.code, 0, "{listed:?}");
    assert_eq!(listed.stdout.lines().count(), 1);
    assert!(listed.stdout.contains("state=active"));
    assert!(!listed.all().contains(&secret));
    assert!(!listed.all().contains(PEPPER));
    let id = listed.stdout.split_whitespace().next().unwrap();
    uuid::Uuid::parse_str(id).unwrap();
    let duplicate = hub.run(&[
        "token",
        "create",
        "--principal",
        "alice",
        "--output",
        file.to_str().unwrap(),
    ]);
    assert_eq!(duplicate.code, 2, "{duplicate:?}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), secret);
    assert_eq!(
        hub.run(&["token", "list"]).stdout.lines().count(),
        1,
        "failed output must roll back issuance"
    );
    let other = hub.work().join("other-token.secret");
    let other_org = hub.run(&[
        "token",
        "create",
        "--organization",
        "other",
        "--principal",
        "alice",
        "--create-user",
        "--role",
        "admin",
        "--output",
        other.to_str().unwrap(),
    ]);
    assert_eq!(other_org.code, 0, "{other_org:?}");
    let cross_org = hub.run(&["token", "revoke", "--organization", "other", id]);
    assert_eq!(cross_org.code, 2, "{cross_org:?}");
    assert!(hub.run(&["token", "list"]).stdout.contains("state=active"));

    let server = Server::start(&hub, &[]);
    let url = format!("http://{}", server.addr);
    let foreign_edge = Sandbox::new();
    let foreign_ref = format!("file:{}", other.display());
    let foreign = foreign_edge.run(&["login", &url, "--token-ref", &foreign_ref]);
    assert_eq!(foreign.code, 1, "{foreign:?}");
    assert!(!foreign_edge.engine_config().exists());
    let edge = Sandbox::new();
    let reference = format!("file:{}", file.display());
    let login = edge.run(&["login", &url, "--token-ref", &reference]);
    assert_eq!(login.code, 0, "{login:?}");
    assert!(login.stdout.contains("token accepted"));
    assert!(!login.all().contains(&secret));
    let saved = std::fs::read_to_string(edge.engine_config()).unwrap();
    assert!(!saved.contains(&secret));
    assert!(!saved.contains(PEPPER));
    assert_eq!(
        knowell_config::load_engine(&edge.engine_config())
            .unwrap()
            .server
            .role,
        knowell_config::ServerRole::Edge
    );
    let revoked = hub.run(&["token", "revoke", id]);
    assert_eq!(revoked.code, 0, "{revoked:?}");
    assert_eq!(hub.run(&["token", "revoke", id]).code, 0);
    assert!(hub.run(&["token", "list"]).stdout.contains("state=revoked"));
    let refused_edge = Sandbox::new();
    let refused = refused_edge.run(&["login", &url, "--token-ref", &reference]);
    assert_eq!(refused.code, 1, "{refused:?}");
    assert!(!refused.all().contains(&secret));
    assert!(!refused_edge.engine_config().exists());
    assert_eq!(server.stop(), 0);
    crate::common::block_on(async {
        use sqlx::Connection;
        let mut conn = knowell_store::PgConnection::connect(&db.url).await.unwrap();
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT action, count(*) FROM audit_log WHERE resource = $1 GROUP BY action ORDER BY action",
        ).bind(format!("token:{id}")).fetch_all(&mut conn).await.unwrap();
        assert_eq!(
            rows,
            vec![("token_create".into(), 1), ("token_revoke".into(), 2)]
        );
        conn.close().await.unwrap();
    });
}

#[test]
fn failed_bootstrap_rolls_back_and_pepper_is_required() {
    let Some(admin) = admin_url("failed_bootstrap_rolls_back_and_pepper_is_required") else {
        return;
    };
    let db = ScratchDb::create(&admin);
    let hub = configured(&db.url);
    let missing_dir = hub.work().join("missing").join("token.secret");
    let failed = hub.run(&[
        "token",
        "create",
        "--principal",
        "alice",
        "--create-user",
        "--role",
        "viewer",
        "--output",
        missing_dir.to_str().unwrap(),
    ]);
    assert_eq!(failed.code, 2, "{failed:?}");
    let output = hub.work().join("token.secret");
    let retry = hub.run(&[
        "token",
        "create",
        "--principal",
        "alice",
        "--create-user",
        "--role",
        "viewer",
        "--output",
        output.to_str().unwrap(),
    ]);
    assert_eq!(retry.code, 0, "{retry:?}");
    hub.write_engine(
        "version = 1\n[database]\nmode = 'external'\nurl = 'env:KNOWELL_CLI_DB_URL'\n",
    );
    let refused = hub.run(&[
        "token",
        "create",
        "--principal",
        "alice",
        "--output",
        hub.work().join("unissued.secret").to_str().unwrap(),
    ]);
    assert_eq!(refused.code, 2, "{refused:?}");
    assert!(refused.stderr.contains("server.token_pepper"));
    assert!(!hub.work().join("unissued.secret").exists());
    assert_eq!(hub.run(&["token", "list"]).stdout.lines().count(), 1);
}
