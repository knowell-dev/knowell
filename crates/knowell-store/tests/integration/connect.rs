use std::time::Duration;

use knowell_store::{Store, StoreError, StoreOptions};
use secrecy::SecretString;

use crate::common::{admin_options, require_db};

const CANARY: &str = "KNOWELL_CANARY_pw_51d0";

fn quick() -> StoreOptions {
    StoreOptions {
        acquire_timeout: Duration::from_secs(5),
        ..StoreOptions::default()
    }
}

fn assert_clean(err: &StoreError, secrets: &[&str]) {
    for text in [err.to_string(), format!("{err:?}")] {
        for secret in secrets {
            assert!(!text.contains(secret), "error leaks a secret: {text}");
        }
    }
}

/// Needs no database: nothing listens on port 1.
#[tokio::test]
async fn connect_errors_never_contain_the_url_or_password() {
    let encoded = "KNOWELL_CANARY_p%40ss_51d0";
    let decoded = "KNOWELL_CANARY_p@ss_51d0";
    let urls = [
        format!("postgres://knowell:{CANARY}@127.0.0.1:1/knowell"),
        format!("postgres://knowell:{encoded}@127.0.0.1:1/knowell"),
        format!("postgresql://127.0.0.1:1/knowell?user=knowell&password={CANARY}"),
    ];
    for url in urls {
        let err = Store::connect(&SecretString::from(url.clone()), &quick())
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Connect(_)), "{err}");
        assert_clean(&err, &[url.as_str(), CANARY, encoded, decoded]);
    }
}

#[tokio::test]
async fn invalid_urls_are_rejected_without_echo() {
    for url in [
        format!("mysql://u:{CANARY}@localhost/db"),
        format!("postgres://u:{CANARY}@localhost:99999/db"),
        format!("postgres://u@localhost/db?passwd={CANARY}"),
    ] {
        let err = Store::connect(&SecretString::from(url.clone()), &quick())
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::InvalidUrl(_)), "{err}");
        assert_clean(&err, &[url.as_str(), CANARY]);
    }
}

/// A real server rejecting a wrong password: the server's message names the
/// user, never the password.
#[tokio::test]
async fn authentication_failure_does_not_leak_the_password() {
    let Some(admin) = admin_options(module_path!()) else {
        return;
    };
    let host = admin.get_host().to_owned();
    let url = format!(
        "postgres://{}:{CANARY}@{}:{}/postgres",
        admin.get_username(),
        if host.contains(':') {
            format!("[{host}]")
        } else {
            host
        },
        admin.get_port()
    );
    let err = Store::connect(&SecretString::from(url.clone()), &quick())
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Connect(_)), "{err}");
    assert_clean(&err, &[url.as_str(), CANARY]);
}

#[tokio::test]
async fn check_server_reports_postgres_and_pgvector() {
    let db = require_db!();
    let info = db.store.check_server().await.unwrap();
    assert!(
        info.server_version_num >= 170_000,
        "{}",
        info.server_version
    );
    let vector = info.vector.as_ref().expect("pgvector available");
    assert!(!vector.default_version.is_empty());
    // Migrations installed the extension in this database.
    assert!(vector.installed_version.is_some());
    assert!(info.is_supported(), "{:?}", info.issues());
}

#[tokio::test]
async fn migrations_apply_from_empty_and_rerun_cleanly() {
    let db = require_db!();
    // Already migrated by the harness; a second run is a no-op.
    db.store.migrate().await.unwrap();
    let mut conn = db.conn().await;
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(applied, 10);
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.tables
         WHERE table_schema = current_schema() AND table_name <> '_sqlx_migrations'
         ORDER BY table_name",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    let expected = [
        "access_grant",
        "api_token",
        "audit_log",
        "chunk",
        "chunk_input",
        "content",
        "contract",
        "edge",
        "embedding",
        "embedding_profile",
        "file_version",
        "index_generation",
        "job",
        "knowledge_evidence",
        "knowledge_history",
        "knowledge_record",
        "knowledge_record_version",
        "occurrence",
        "organization",
        "principal",
        "project",
        "source",
        "symbol",
        "task",
        "task_checkpoint",
        "view",
        "view_generation",
        "view_manifest",
        "view_manifest_entry",
        "workspace",
    ];
    assert_eq!(tables, expected);
    // Ids are UUIDv7.
    let version: i16 = sqlx::query_scalar("SELECT uuid_extract_version(knowell_uuidv7())")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(version, 7);
}
