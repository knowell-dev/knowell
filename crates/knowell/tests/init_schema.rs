//! Existing domain-schema upgrades require explicit maintenance authorization.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stderr
)]

#[path = "cli/common.rs"]
#[allow(dead_code)]
mod common;

use knowell_store::{Store, StoreOptions};
use secrecy::SecretString;
use sqlx::migrate::Migrator;

use common::{Sandbox, ScratchDb, admin_url};

static DOMAIN_MIGRATIONS: Migrator = sqlx::migrate!("../knowell-store/migrations");

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    tables: Vec<String>,
    history: Vec<(i64, bool, Vec<u8>)>,
    markers: Vec<String>,
    maintenance_owner: Option<uuid::Uuid>,
}

async fn snapshot(store: &Store) -> Snapshot {
    let mut conn = store.acquire().await.unwrap();
    let tables = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.tables
         WHERE table_schema = 'public' ORDER BY table_name",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    let history = sqlx::query_as(
        "SELECT version, success, checksum FROM public._sqlx_migrations ORDER BY version",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    let markers =
        sqlx::query_scalar("SELECT marker FROM public.synthetic_upgrade_guard ORDER BY marker")
            .fetch_all(&mut *conn)
            .await
            .unwrap();
    drop(conn);
    let maintenance_owner = store.maintenance_owner().await.unwrap();
    Snapshot {
        tables,
        history,
        markers,
        maintenance_owner,
    }
}

#[test]
fn init_refuses_an_existing_older_schema_without_ddl_or_maintenance_intent() {
    let Some(admin) = admin_url("init_schema::existing_upgrade_requires_maintenance") else {
        return;
    };
    let db = ScratchDb::create(&admin);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let store = runtime
        .block_on(Store::connect(
            &SecretString::from(db.url.clone()),
            &StoreOptions::default(),
        ))
        .unwrap();
    let latest = Store::latest_schema().version;
    let earlier = Migrator::with_migrations(
        DOMAIN_MIGRATIONS
            .iter()
            .filter(|migration| migration.version < latest)
            .cloned()
            .collect(),
    );
    runtime.block_on(async {
        earlier.run(store.pool()).await.unwrap();
        let mut conn = store.acquire().await.unwrap();
        sqlx::raw_sql(
            "CREATE TABLE public.synthetic_upgrade_guard (marker text PRIMARY KEY);
             INSERT INTO public.synthetic_upgrade_guard VALUES ('preserved synthetic marker')",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
    });
    let before = runtime.block_on(snapshot(&store));
    let previous = runtime.block_on(store.inspect_schema()).unwrap().version;
    assert!(previous > 0 && previous < latest);
    assert!(before.maintenance_owner.is_none());
    assert!(
        !before
            .tables
            .iter()
            .any(|name| name == "_knowell_maintenance")
    );

    let mut sandbox = Sandbox::new();
    sandbox.write_engine(
        "version = 1\n[database]\nmode = \"external\"\nurl = \"env:KNOWELL_CLI_SYNTHETIC_DB_URL\"\n",
    );
    sandbox.set_env("KNOWELL_CLI_SYNTHETIC_DB_URL", &db.url);
    let init = sandbox.run_with_timeout(&["init"], std::time::Duration::from_secs(30));
    assert!(!init.all().contains(&db.url), "database url leaked");
    assert_eq!(init.code, 2);
    assert!(
        init.stderr
            .contains("know maintain --operation UUID --migrate")
    );
    assert!(init.stderr.contains("--backup FILE"));
    assert!(
        init.stderr
            .contains("--external-backup-confirmed --session-gates-confirmed")
    );
    assert_eq!(runtime.block_on(snapshot(&store)), before);

    // Explicit maintenance checks backup authorization before persisting intent.
    let operation = uuid::Uuid::now_v7().to_string();
    let rejected = sandbox.run_with_timeout(
        &[
            "maintain",
            "--operation",
            &operation,
            "--migrate",
            "--session-gates-confirmed",
        ],
        std::time::Duration::from_secs(30),
    );
    assert!(!rejected.all().contains(&db.url), "database url leaked");
    assert_eq!(rejected.code, 2);
    assert!(rejected.stderr.contains("backup"));
    assert_eq!(runtime.block_on(snapshot(&store)), before);
    runtime.block_on(store.close());
}

#[test]
fn init_refuses_missing_earlier_history_even_when_latest_migration_is_present() {
    let Some(admin) = admin_url("init_schema::incomplete_current_schema") else {
        return;
    };
    let db = ScratchDb::create(&admin);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let store = runtime
        .block_on(Store::connect(
            &SecretString::from(db.url.clone()),
            &StoreOptions::default(),
        ))
        .unwrap();
    runtime.block_on(async {
        DOMAIN_MIGRATIONS.run(store.pool()).await.unwrap();
        let mut conn = store.acquire().await.unwrap();
        sqlx::raw_sql(
            "CREATE TABLE public.synthetic_upgrade_guard (marker text PRIMARY KEY);
             INSERT INTO public.synthetic_upgrade_guard VALUES ('preserved synthetic marker');
             DELETE FROM public._sqlx_migrations WHERE version = 7",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
    });
    let before = runtime.block_on(snapshot(&store));
    assert_eq!(
        runtime.block_on(store.inspect_schema()).unwrap().version,
        Store::latest_schema().version
    );
    assert!(matches!(
        runtime.block_on(store.validate_schema()),
        Err(knowell_store::StoreError::SchemaMigrationRequired { .. })
    ));
    assert!(before.maintenance_owner.is_none());
    assert!(
        !before
            .tables
            .iter()
            .any(|name| name == "_knowell_maintenance")
    );

    let mut sandbox = Sandbox::new();
    sandbox.write_engine(
        "version = 1\n[database]\nmode = \"external\"\nurl = \"env:KNOWELL_CLI_SYNTHETIC_DB_URL\"\n",
    );
    sandbox.set_env("KNOWELL_CLI_SYNTHETIC_DB_URL", &db.url);
    let init = sandbox.run_with_timeout(&["init"], std::time::Duration::from_secs(30));
    assert!(!init.all().contains(&db.url), "database url leaked");
    assert_eq!(init.code, 2);
    assert!(
        init.stderr
            .contains("cannot validate the existing database schema")
    );
    assert!(
        init.stderr
            .contains("know maintain --operation UUID --migrate")
    );
    assert!(init.stderr.contains("backup authorization"));
    assert_eq!(runtime.block_on(snapshot(&store)), before);
    runtime.block_on(store.close());
}
