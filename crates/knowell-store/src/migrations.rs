//! Core migrations plus optional vector storage, preserving historical checksums.

use sqlx::migrate::{Migrate, Migration, Migrator};
use sqlx::{PgConnection, PgPool, SqlSafeStr};

use crate::StoreError;

/// Kept immutable so databases created by the first prerelease still validate.
static ORIGINAL: Migrator = sqlx::migrate!();

fn core_variant(original: &Migration) -> Migration {
    let sql = match original.version {
        1 => include_str!("../core_migrations/0001_foundation.sql"),
        5 => include_str!("../core_migrations/0005_embeddings.sql"),
        _ => return original.clone(),
    };
    Migration::new(
        original.version,
        original.description.clone(),
        original.migration_type,
        sql.into_sql_str(),
        original.no_tx,
    )
}

pub(crate) async fn run(pool: &PgPool, enable_vectors: bool) -> Result<(), StoreError> {
    let mut conn = pool.acquire().await?;
    // Migration selection and optional DDL share SQLx's migration lock. Closing
    // this dedicated connection releases it on success, error and cancellation.
    // It must never return to the pool holding a session advisory lock.
    conn.close_on_drop();
    conn.lock().await.map_err(StoreError::Migrate)?;
    let result = run_locked(&mut conn, enable_vectors).await;
    let closed = conn.close().await;
    result?;
    closed?;
    Ok(())
}

async fn run_locked(conn: &mut PgConnection, enable_vectors: bool) -> Result<(), StoreError> {
    conn.ensure_migrations_table("_sqlx_migrations")
        .await
        .map_err(StoreError::Migrate)?;
    let applied = conn
        .list_applied_migrations("_sqlx_migrations")
        .await
        .map_err(StoreError::Migrate)?;
    let migrations = ORIGINAL
        .iter()
        .map(|original| {
            if applied
                .iter()
                .any(|done| done.version == original.version && done.checksum == original.checksum)
            {
                // Validate the exact historical SQL, without rewriting history
                // or disabling SQLx's checksum/dirty/missing-migration checks.
                original.clone()
            } else {
                core_variant(original)
            }
        })
        .collect();
    let mut migrator = Migrator::with_migrations(migrations);
    // Already held across selection and both schema stages by run().
    migrator.set_locking(false);
    migrator
        .run(&mut *conn)
        .await
        .map_err(StoreError::Migrate)?;
    if enable_vectors {
        use sqlx::Connection;
        let mut tx = conn.begin().await?;
        sqlx::raw_sql(include_str!("../core_migrations/vector.sql"))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    Ok(())
}
