//! Synthetic databases exercise admission across independent pools and recovery.

use std::time::Duration;

use knowell_store::{Store, StoreError, StoreOptions};
use uuid::Uuid;

use crate::common::{ENV, TestDb};

fn options() -> StoreOptions {
    StoreOptions {
        max_connections: 3,
        acquire_timeout: Duration::from_secs(2),
        application_name: "knowell-admission-test".into(),
        ..StoreOptions::default()
    }
}

async fn runtime(db: &TestDb) -> Result<Store, StoreError> {
    Store::connect_runtime_with(
        db.store.pool().connect_options().as_ref().clone(),
        &options(),
    )
    .await
}

#[tokio::test]
async fn runtime_requires_initialized_exact_schema_without_ddl() {
    let Some(db) = TestDb::unmigrated(module_path!(), ENV).await else {
        return;
    };
    assert_eq!(db.store.inspect_schema().await.unwrap().version, 0);
    assert!(matches!(
        runtime(&db).await,
        Err(StoreError::SchemaUninitialized)
    ));
    let mut conn = db.conn().await;
    let history_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert!(!history_exists);
    drop(conn);
    db.store.migrate().await.unwrap();
    let running = runtime(&db).await.unwrap();
    assert_eq!(
        running.validate_schema().await.unwrap(),
        Store::latest_schema()
    );
    assert!(matches!(
        running.migrate().await,
        Err(StoreError::RuntimeMigrationForbidden)
    ));
    assert!(matches!(
        running.begin_maintenance(Uuid::now_v7()).await,
        Err(StoreError::RuntimeMigrationForbidden)
    ));
    running.close().await;
}

#[tokio::test]
async fn independent_runtime_pools_fence_exclusive_migration_and_recovery() {
    let Some(db) = TestDb::create(module_path!()).await else {
        return;
    };
    let first = runtime(&db).await.unwrap();
    let second = runtime(&db).await.unwrap();
    let first_connection = first.acquire().await.unwrap();
    let second_connection = second.acquire().await.unwrap();
    let owner = Uuid::now_v7();
    let mut maintenance = db.store.begin_maintenance(owner).await.unwrap();
    assert_eq!(first.maintenance_owner().await.unwrap(), Some(owner));
    assert!(
        matches!(runtime(&db).await, Err(StoreError::MaintenanceRequired { owner: actual }) if actual == owner)
    );
    assert!(
        matches!(first.acquire().await, Err(StoreError::MaintenanceRequired { owner: actual }) if actual == owner)
    );
    assert!(matches!(
        db.store.begin_maintenance(Uuid::now_v7()).await,
        Err(StoreError::MaintenanceBusy)
    ));
    assert!(matches!(
        maintenance
            .acquire_exclusive(Duration::from_millis(30))
            .await,
        Err(StoreError::MaintenanceTimeout)
    ));
    assert_eq!(db.store.maintenance_owner().await.unwrap(), Some(owner));
    drop(maintenance);
    drop(first_connection);
    drop(second_connection);
    first.close().await;
    second.close().await;
    let mut recovered = recover(&db, owner).await;
    recovered
        .acquire_exclusive(Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(
        recovered.validate_schema().await.unwrap(),
        Store::latest_schema()
    );
    recovered.migrate().await.unwrap();
    recovered.finish().await.unwrap();
    assert_eq!(db.store.maintenance_owner().await.unwrap(), None);
    let runtime = runtime(&db).await.unwrap();
    runtime.close().await;
}

#[tokio::test]
async fn maintenance_status_does_not_borrow_or_admit_a_runtime_connection() {
    let Some(db) = TestDb::create(module_path!()).await else {
        return;
    };
    let running = Store::connect_runtime_with(
        db.store.pool().connect_options().as_ref().clone(),
        &StoreOptions {
            max_connections: 1,
            ..options()
        },
    )
    .await
    .unwrap();
    let connection = running.acquire().await.unwrap();
    let owner = Uuid::now_v7();
    let mut maintenance = db.store.begin_maintenance(owner).await.unwrap();
    // The only pooled session is checked out, so a status query must neither
    // wait for it nor create another shared-gated data session.
    assert_eq!(running.maintenance_owner().await.unwrap(), Some(owner));
    connection.close().await.unwrap();
    maintenance
        .acquire_exclusive(Duration::from_secs(2))
        .await
        .unwrap();
    // Metadata remains observable while the exclusive data gate is held.
    assert_eq!(running.maintenance_owner().await.unwrap(), Some(owner));
    running.close().await;
    maintenance.finish().await.unwrap();
}

#[tokio::test]
async fn dropped_maintenance_never_expires_or_allows_another_owner() {
    let Some(db) = TestDb::create(module_path!()).await else {
        return;
    };
    let owner = Uuid::now_v7();
    let maintenance = db.store.begin_maintenance(owner).await.unwrap();
    drop(maintenance);
    let mut recovered = recover(&db, owner).await;
    assert_eq!(db.store.maintenance_owner().await.unwrap(), Some(owner));
    recovered
        .acquire_exclusive(Duration::from_secs(2))
        .await
        .unwrap();
    assert!(matches!(
        runtime(&db).await,
        Err(StoreError::MaintenanceBusy)
    ));
    // A future may be cancelled after obtaining the gate. Dropping it must
    // preserve ownership, and only the same operation can reopen admission.
    drop(recovered);
    let mut recovered = recover(&db, owner).await;
    let other = db.store.begin_maintenance(Uuid::now_v7()).await;
    assert!(matches!(other, Err(StoreError::MaintenanceBusy)));
    recovered
        .acquire_exclusive(Duration::from_secs(2))
        .await
        .unwrap();
    recovered.finish().await.unwrap();
}

#[tokio::test]
async fn replacement_connections_keep_typed_schema_and_maintenance_refusals() {
    let Some(db) = TestDb::create(module_path!()).await else {
        return;
    };
    let running = runtime(&db).await.unwrap();
    // Remove all current physical sessions without closing the pool. A new
    // physical session must execute admission again, rather than reuse a
    // process-only heartbeat lock or cached schema result.
    let connection = running.acquire().await.unwrap();
    connection.close().await.unwrap();
    let owner = Uuid::now_v7();
    let mut maintenance = db.store.begin_maintenance(owner).await.unwrap();
    assert!(
        matches!(running.acquire().await, Err(StoreError::MaintenanceRequired { owner: actual }) if actual == owner)
    );
    running.close().await;
    maintenance
        .acquire_exclusive(Duration::from_secs(2))
        .await
        .unwrap();
    maintenance.finish().await.unwrap();

    let mut conn = db.conn().await;
    let required = knowell_store::Store::latest_schema().version;
    let previous = required - 1;
    sqlx::query("DELETE FROM public._sqlx_migrations WHERE version = $1")
        .bind(required)
        .execute(&mut *conn)
        .await
        .unwrap();
    assert_eq!(db.store.inspect_schema().await.unwrap().version, previous);
    assert!(matches!(
        runtime(&db).await,
        Err(StoreError::SchemaMigrationRequired {
            current,
            required: expected
        }) if current == previous && expected == required
    ));
    drop(conn);
}

async fn recover(db: &TestDb, owner: Uuid) -> knowell_store::Maintenance {
    // Disconnect cleanup is asynchronous in PostgreSQL. Bound the wait and
    // never treat a timestamp/expired heartbeat as maintenance ownership.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match db.store.begin_maintenance(owner).await {
                Ok(maintenance) => return maintenance,
                Err(StoreError::MaintenanceBusy) => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(error) => panic!("cannot recover synthetic maintenance: {error}"),
            }
        }
    })
    .await
    .unwrap()
}
