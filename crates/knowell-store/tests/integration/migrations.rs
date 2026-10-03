//! Schema upgrades must preserve data and history with or without pgvector.

use std::time::Duration;

use knowell_core::ContentHash;
use knowell_store::content::{self, NewContent};
use knowell_store::embeddings::{self, NewEmbedding, NewEmbeddingProfile};
use knowell_store::{Store, StoreError};
use sqlx::migrate::{Migrate, MigrateError, Migration, Migrator};
use sqlx::{PgConnection, SqlSafeStr};

use crate::common::{ENV, PLAIN_ENV, TestDb, fixture, name};

static LEGACY: Migrator = sqlx::migrate!();

fn profile() -> NewEmbeddingProfile {
    NewEmbeddingProfile {
        name: name("upgrade-test"),
        provider: "fake".into(),
        model: "synthetic".into(),
        dimensions: 3,
        input_format_version: "test-v1".into(),
    }
}

async fn history(conn: &mut PgConnection) -> Vec<(i64, Vec<u8>)> {
    sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations ORDER BY version")
        .fetch_all(conn)
        .await
        .unwrap()
}

async fn migrate(store: &Store) -> Result<(), StoreError> {
    tokio::time::timeout(Duration::from_secs(30), store.migrate())
        .await
        .expect("migration must release its advisory lock, including on error")
}

async fn assert_no_migration_lock(conn: &mut PgConnection) {
    // Closing a connection sends Terminate, but another backend can answer
    // before PostgreSQL finishes processing it. Bound that teardown delay.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let held: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory'
                 AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
            )
            .fetch_one(&mut *conn)
            .await
            .unwrap();
            if held == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a failed migration must not return a locked connection to the pool");
}

#[tokio::test]
async fn cancelled_migration_does_not_leave_a_lock_waiter_in_the_pool() {
    let Some(db) = TestDb::create(module_path!()).await else {
        return;
    };
    let mut blocker = db.conn().await;
    blocker.lock().await.unwrap();
    let mut waiter = Box::pin(db.store.migrate());
    let waiting = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_locks
                 WHERE locktype = 'advisory' AND NOT granted
                 AND database = (SELECT oid FROM pg_database WHERE datname = current_database()))",
            )
            .fetch_one(&mut *blocker)
            .await
            .unwrap();
            if waiting {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    tokio::select! {
        result = &mut waiter => panic!("migration completed while its lock was held: {result:?}"),
        result = waiting => result.expect("the second migration must wait on the existing advisory lock"),
    }
    // Cancel the future while it is waiting inside SQLx, as a dropped request
    // would, and verify its dedicated connection cannot return to the pool.
    drop(waiter);
    blocker.unlock().await.unwrap();
    assert_no_migration_lock(&mut blocker).await;
    migrate(&db.store).await.unwrap();
}

#[tokio::test]
async fn plain_postgres_migrates_concurrently_and_reports_semantic_unavailable() {
    let Some(db) = TestDb::unmigrated(module_path!(), PLAIN_ENV).await else {
        return;
    };
    let info = db.store.check_server().await.unwrap();
    assert!(
        info.vector.is_none(),
        "this test requires unmodified PostgreSQL"
    );
    assert!(info.supports_core());
    assert!(!info.semantic_enabled());

    let (first, second) = tokio::join!(migrate(&db.store), migrate(&db.store));
    first.unwrap();
    second.unwrap();
    let mut conn = db.conn().await;
    let before = history(&mut conn).await;
    assert_eq!(before.len(), LEGACY.iter().count());
    let fx = fixture(&mut conn, "plain").await;
    let text = "pub fn core_storage_works() {}";
    let hash = ContentHash::of(text.as_bytes());
    content::upsert_contents(
        &mut conn,
        fx.org.id,
        &[NewContent {
            hash,
            size_bytes: text.len() as u64,
            language: Some("rust".into()),
            redacted_text: Some(text.into()),
        }],
    )
    .await
    .unwrap();
    assert!(!embeddings::available(&mut conn).await.unwrap());
    let err = embeddings::register_profile(&mut conn, fx.org.id, &profile())
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::SemanticUnavailable));
    assert!(err.to_string().contains("know init"));
    assert!(
        embeddings::list_profiles(&mut conn, fx.org.id)
            .await
            .unwrap()
            .is_empty()
    );

    migrate(&db.store).await.unwrap();
    assert_eq!(history(&mut conn).await, before);
    let stored = content::get_content(&mut conn, fx.org.id, &hash)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.redacted_text.as_deref(), Some(text));
}

#[tokio::test]
async fn legacy_checksums_and_vectors_survive_upgrade() {
    let Some(db) = TestDb::unmigrated(module_path!(), ENV).await else {
        return;
    };
    LEGACY.run(db.store.pool()).await.unwrap();
    let mut conn = db.conn().await;
    let before = history(&mut conn).await;
    let fx = fixture(&mut conn, "legacy").await;
    let profile = embeddings::register_profile(&mut conn, fx.org.id, &profile())
        .await
        .unwrap();
    let hash = ContentHash::of(b"synthetic migration input");
    embeddings::upsert_embeddings(
        &mut conn,
        &profile,
        &[NewEmbedding {
            prepared_input_hash: hash,
            vector: vec![1.0, 0.0, 0.0],
        }],
    )
    .await
    .unwrap();

    migrate(&db.store).await.unwrap();
    migrate(&db.store).await.unwrap();
    assert_eq!(history(&mut conn).await, before);
    assert_eq!(
        embeddings::get_embedding(&mut conn, profile.id, &hash)
            .await
            .unwrap(),
        Some(vec![1.0, 0.0, 0.0])
    );
    assert!(
        embeddings::profile_index_ready(&mut conn, &profile)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn core_database_enables_vectors_later_without_rewriting_history() {
    let Some(db) = TestDb::unmigrated(module_path!(), ENV).await else {
        return;
    };
    // Seed the schema a plain PostgreSQL install creates, on a server where
    // extension files are available for the subsequent upgrade.
    let migrations = LEGACY
        .iter()
        .map(|original| {
            let sql = match original.version {
                1 => include_str!("../../core_migrations/0001_foundation.sql"),
                5 => include_str!("../../core_migrations/0005_embeddings.sql"),
                _ => return original.clone(),
            };
            Migration::new(
                original.version,
                original.description.clone(),
                original.migration_type,
                sql.into_sql_str(),
                original.no_tx,
            )
        })
        .collect();
    Migrator::with_migrations(migrations)
        .run(db.store.pool())
        .await
        .unwrap();
    let mut conn = db.conn().await;
    assert!(!embeddings::available(&mut conn).await.unwrap());
    let fx = fixture(&mut conn, "late-vector").await;
    let before = history(&mut conn).await;

    let (first, second) = tokio::join!(migrate(&db.store), migrate(&db.store));
    first.unwrap();
    second.unwrap();
    assert_eq!(history(&mut conn).await, before);
    assert!(embeddings::available(&mut conn).await.unwrap());
    assert!(db.store.check_server().await.unwrap().semantic_enabled());
    let profile = embeddings::register_profile(&mut conn, fx.org.id, &profile())
        .await
        .unwrap();
    let hash = ContentHash::of(b"synthetic late extension input");
    embeddings::upsert_embeddings(
        &mut conn,
        &profile,
        &[NewEmbedding {
            prepared_input_hash: hash,
            vector: vec![1.0, 0.0, 0.0],
        }],
    )
    .await
    .unwrap();
    let hits = embeddings::nearest(
        &mut conn,
        &profile,
        &[1.0, 0.0, 0.0],
        &embeddings::NearestOptions::new(5),
    )
    .await
    .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].prepared_input_hash, hash);
}

#[tokio::test]
async fn invalid_migration_history_is_rejected_and_releases_the_lock() {
    let Some(db) = TestDb::create(module_path!()).await else {
        return;
    };
    let mut conn = db.conn().await;
    let before = history(&mut conn).await;
    sqlx::query("UPDATE _sqlx_migrations SET checksum = $1 WHERE version = 1")
        .bind(vec![0_u8; 48])
        .execute(&mut *conn)
        .await
        .unwrap();
    assert!(matches!(
        migrate(&db.store).await,
        Err(StoreError::Migrate(MigrateError::VersionMismatch(1)))
    ));
    assert_no_migration_lock(&mut conn).await;
    sqlx::query("UPDATE _sqlx_migrations SET checksum = $1 WHERE version = 1")
        .bind(&before[0].1)
        .execute(&mut *conn)
        .await
        .unwrap();
    migrate(&db.store).await.unwrap();

    sqlx::query("UPDATE _sqlx_migrations SET success = false WHERE version = 1")
        .execute(&mut *conn)
        .await
        .unwrap();
    assert!(matches!(
        migrate(&db.store).await,
        Err(StoreError::Migrate(MigrateError::Dirty(1)))
    ));
    assert_no_migration_lock(&mut conn).await;
    sqlx::query("UPDATE _sqlx_migrations SET success = true WHERE version = 1")
        .execute(&mut *conn)
        .await
        .unwrap();
    migrate(&db.store).await.unwrap();
    assert_eq!(history(&mut conn).await, before);

    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         SELECT 9999, description, success, checksum, execution_time
         FROM _sqlx_migrations WHERE version = 1",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    assert!(matches!(
        migrate(&db.store).await,
        Err(StoreError::Migrate(MigrateError::VersionMissing(9999)))
    ));
    assert_no_migration_lock(&mut conn).await;
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 9999")
        .execute(&mut *conn)
        .await
        .unwrap();
    migrate(&db.store).await.unwrap();
    assert_eq!(history(&mut conn).await, before);
}
