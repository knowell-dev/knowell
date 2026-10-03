//! Core migrations plus optional vector storage, preserving historical checksums.

use sqlx::migrate::{Migrate, MigrateError, Migration, Migrator};
use sqlx::{PgConnection, SqlSafeStr};

use crate::StoreError;

/// Kept immutable so databases created by the first prerelease still validate.
static ORIGINAL: Migrator = sqlx::migrate!();

/// Latest numbered domain migration this binary reads and writes.
pub const LATEST_SCHEMA_VERSION: i64 = 13;

/// Database runtime-admission protocol implemented by this build.
pub const RUNTIME_PROTOCOL_VERSION: u32 = 1;

/// Exact domain schema and admission protocol understood by a runtime.
///
/// Optional pgvector availability is independent of this identity. Historical
/// original and core-only checksums describe the same supported domain schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaIdentity {
    /// Latest applied numbered domain migration.
    pub version: i64,
    /// Runtime-admission protocol version, starting at one.
    pub runtime_protocol: u32,
}

impl SchemaIdentity {
    /// Exact schema this build requires; no older/newer compatibility is inferred.
    pub const fn current() -> Self {
        Self {
            version: LATEST_SCHEMA_VERSION,
            runtime_protocol: RUNTIME_PROTOCOL_VERSION,
        }
    }
}

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

/// Checks history without creating a table, acquiring migration locks or applying DDL.
pub(crate) async fn validate(conn: &mut PgConnection) -> Result<SchemaIdentity, StoreError> {
    let Some(history) = history(conn).await? else {
        return Err(StoreError::SchemaUninitialized);
    };
    validate_history(&history, true)
}

pub(crate) async fn inspect(conn: &mut PgConnection) -> Result<SchemaIdentity, StoreError> {
    let history = history(conn).await?.unwrap_or_default();
    let identity = validate_history(&history, false)?;
    if identity.version > LATEST_SCHEMA_VERSION {
        return Err(StoreError::SchemaNewer {
            current: identity.version,
            supported: LATEST_SCHEMA_VERSION,
        });
    }
    Ok(identity)
}

/// Rejects corrupt history before maintenance intent is persisted. Missing known
/// migrations are allowed only for this explicit administrative path.
pub(crate) async fn validate_pending(conn: &mut PgConnection) -> Result<(), StoreError> {
    if let Some(history) = history(conn).await? {
        validate_history(&history, false)?;
    }
    Ok(())
}

type HistoryEntry = (i64, bool, Vec<u8>);

async fn history(conn: &mut PgConnection) -> Result<Option<Vec<HistoryEntry>>, StoreError> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await?;
    if !exists {
        return Ok(None);
    }
    Ok(Some(
        sqlx::query_as(
            "SELECT version, success, checksum FROM public._sqlx_migrations ORDER BY version",
        )
        .fetch_all(&mut *conn)
        .await?,
    ))
}

fn validate_history(
    history: &[HistoryEntry],
    require_latest: bool,
) -> Result<SchemaIdentity, StoreError> {
    if let Some((version, _, _)) = history.iter().find(|(_, success, _)| !success) {
        return Err(StoreError::Migrate(MigrateError::Dirty(*version)));
    }
    let current = history
        .iter()
        .map(|(version, _, _)| *version)
        .max()
        .unwrap_or(0);
    if require_latest && current > LATEST_SCHEMA_VERSION {
        return Err(StoreError::SchemaNewer {
            current,
            supported: LATEST_SCHEMA_VERSION,
        });
    }
    for (version, _, checksum) in history {
        let original = ORIGINAL
            .iter()
            .find(|migration| migration.version == *version)
            .ok_or(StoreError::Migrate(MigrateError::VersionMissing(*version)))?;
        if original.checksum.as_ref() != checksum.as_slice()
            && core_variant(original).checksum.as_ref() != checksum.as_slice()
        {
            return Err(StoreError::Migrate(MigrateError::VersionMismatch(*version)));
        }
    }
    if require_latest
        && ORIGINAL.iter().any(|migration| {
            !history
                .iter()
                .any(|(version, _, _)| *version == migration.version)
        })
    {
        return Err(StoreError::SchemaMigrationRequired {
            current,
            required: LATEST_SCHEMA_VERSION,
        });
    }
    Ok(SchemaIdentity {
        version: current,
        runtime_protocol: RUNTIME_PROTOCOL_VERSION,
    })
}

/// Caller holds both SQLx's migration lock and the exclusive runtime data gate.
pub(crate) async fn run_locked(
    conn: &mut PgConnection,
    enable_vectors: bool,
) -> Result<(), StoreError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn history(core: bool) -> Vec<HistoryEntry> {
        ORIGINAL
            .iter()
            .map(|migration| {
                let resolved = if core {
                    core_variant(migration)
                } else {
                    migration.clone()
                };
                (resolved.version, true, resolved.checksum.to_vec())
            })
            .collect()
    }

    #[test]
    fn exact_schema_accepts_both_immutable_histories() {
        for core in [false, true] {
            assert_eq!(
                validate_history(&history(core), true).unwrap(),
                SchemaIdentity::current()
            );
        }
        assert_eq!(
            ORIGINAL.iter().map(|migration| migration.version).max(),
            Some(LATEST_SCHEMA_VERSION)
        );
    }

    #[test]
    fn runtime_rejects_missing_newer_dirty_and_modified_history() {
        assert!(matches!(
            validate_history(&[], true),
            Err(StoreError::SchemaMigrationRequired { current: 0, .. })
        ));
        let mut missing = history(true);
        missing.retain(|(version, _, _)| *version != 7);
        assert!(matches!(
            validate_history(&missing, true),
            Err(StoreError::SchemaMigrationRequired { .. })
        ));
        assert!(validate_history(&missing, false).is_ok());
        let mut newer = history(true);
        newer.push((14, true, vec![0; 48]));
        assert!(matches!(
            validate_history(&newer, true),
            Err(StoreError::SchemaNewer { current: 14, .. })
        ));
        let mut dirty = history(true);
        if let Some(entry) = dirty.first_mut() {
            entry.1 = false;
        }
        assert!(matches!(
            validate_history(&dirty, true),
            Err(StoreError::Migrate(MigrateError::Dirty(1)))
        ));
        let mut corrupt = history(true);
        if let Some(entry) = corrupt.first_mut() {
            entry.2 = vec![0; 48];
        }
        assert!(matches!(
            validate_history(&corrupt, true),
            Err(StoreError::Migrate(MigrateError::VersionMismatch(1)))
        ));
    }

    #[test]
    fn hostile_and_truncated_checksums_never_validate() {
        for checksum in [
            Vec::new(),
            vec![0; 1],
            vec![0; 47],
            vec![0; 49],
            b"KNOWELL_CANARY_CHECKSUM".to_vec(),
        ] {
            let mut corrupt = history(false);
            if let Some(entry) = corrupt.first_mut() {
                entry.2 = checksum;
            }
            assert!(matches!(
                validate_history(&corrupt, true),
                Err(StoreError::Migrate(MigrateError::VersionMismatch(1)))
            ));
        }
        assert!(matches!(
            validate_history(&[(-1, true, Vec::new())], false),
            Err(StoreError::Migrate(MigrateError::VersionMissing(-1)))
        ));
    }
}
