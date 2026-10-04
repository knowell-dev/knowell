//! Cooperative, database-wide runtime admission and persistent maintenance ownership.
//!
//! Locks fence connections created by `Store::connect_runtime`, including pool
//! reconnects. Administrator/raw SQL connections and builds predating this
//! protocol do not participate; bootstrap upgrades must stop those separately.

use std::time::Duration;

use sqlx::migrate::Migrate;
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use crate::StoreError;
use crate::migrations::{self, RUNTIME_PROTOCOL_VERSION, SchemaIdentity};
use crate::store::check_server_on;

// PostgreSQL advisory locks are scoped to the connected database. Keep these
// fixed across hosts and releases, independently of home/install locations.
const DATA_GATE: i64 = 0x4b4e_4f57_0000_0001;
const COORDINATOR_GATE: i64 = 0x4b4e_4f57_0000_0002;

/// Control connection holding a database's maintenance coordinator lock.
///
/// Dropping this value releases session locks but deliberately retains the
/// operation UUID in PostgreSQL. Reopen that UUID to recover; elapsed time never
/// reopens admission. No runtime pool is borrowed while holding an exclusive gate.
pub struct Maintenance {
    conn: Option<PgConnection>,
    owner: Uuid,
    exclusive: bool,
}

impl std::fmt::Debug for Maintenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Maintenance")
            .field("owner", &self.owner)
            .field("exclusive", &self.exclusive)
            .finish_non_exhaustive()
    }
}

impl Maintenance {
    pub(crate) async fn start(
        mut conn: PgConnection,
        owner: Uuid,
        timeout: Duration,
        wait_for_coordinator: bool,
    ) -> Result<Self, StoreError> {
        if owner.is_nil() {
            return Err(StoreError::invalid(
                "maintenance owner must be a non-nil operation uuid",
            ));
        }
        let started = tokio::time::timeout(timeout, async {
            if wait_for_coordinator {
                sqlx::query("SELECT pg_advisory_lock($1)")
                    .bind(COORDINATOR_GATE)
                    .execute(&mut conn)
                    .await?;
            } else {
                let owned: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                    .bind(COORDINATOR_GATE)
                    .fetch_one(&mut conn)
                    .await?;
                if !owned {
                    return Err(StoreError::MaintenanceBusy);
                }
            }
            // Preserve SQLx's historical migration serialization, including
            // bootstrap users that do not yet know the admission protocol.
            conn.lock().await.map_err(StoreError::Migrate)?;
            let recorded = owner_on(&mut conn).await?;
            if let Some(recorded) = recorded
                && recorded != owner
            {
                return Err(StoreError::MaintenanceRequired { owner: recorded });
            }
            if recorded.is_none() {
                migrations::validate_pending(&mut conn).await?;
            }
            sqlx::raw_sql(
                "CREATE TABLE IF NOT EXISTS public._knowell_maintenance (
                    singleton boolean PRIMARY KEY CHECK (singleton),
                    protocol integer NOT NULL CHECK (protocol >= 1),
                    owner uuid NOT NULL,
                    started_at timestamptz NOT NULL DEFAULT now()
                 )",
            )
            .execute(&mut conn)
            .await?;
            sqlx::query(
                "INSERT INTO public._knowell_maintenance (singleton, protocol, owner)
                 VALUES (true, $1, $2) ON CONFLICT (singleton) DO NOTHING",
            )
            .bind(i64::from(RUNTIME_PROTOCOL_VERSION))
            .bind(owner)
            .execute(&mut conn)
            .await?;
            require_owner(&mut conn, owner).await
        })
        .await
        .map_err(|_| StoreError::MaintenanceTimeout)?;
        started?;
        Ok(Self {
            conn: Some(conn),
            owner,
            exclusive: false,
        })
    }

    /// Non-secret UUID that must be recorded in the updater's durable journal.
    pub const fn owner(&self) -> Uuid {
        self.owner
    }

    /// Waits at most `timeout` for all participating runtime connections to close.
    ///
    /// On timeout the control connection is discarded: a cancelled SQL request
    /// must never later obtain a gate on a connection that is reused. Persisted
    /// maintenance intent remains and must be explicitly recovered.
    pub async fn acquire_exclusive(&mut self, timeout: Duration) -> Result<(), StoreError> {
        if self.exclusive {
            return Ok(());
        }
        let owner = self.owner;
        let conn = self.connection()?;
        let result = tokio::time::timeout(timeout, async {
            require_owner(conn, owner).await?;
            sqlx::query("SELECT pg_advisory_lock($1)")
                .bind(DATA_GATE)
                .execute(&mut *conn)
                .await?;
            require_owner(conn, owner).await
        })
        .await;
        match result {
            Ok(Ok(())) => {
                self.exclusive = true;
                Ok(())
            }
            Ok(Err(error)) => Err(error),
            Err(_) => {
                self.conn.take();
                Err(StoreError::MaintenanceTimeout)
            }
        }
    }

    /// Applies domain migrations and optional vector DDL under the exclusive gate.
    ///
    /// This uses only the coordinator connection, avoiding a shared/exclusive
    /// pool deadlock. Failures retain the operation's persistent admission block.
    pub async fn migrate(&mut self) -> Result<(), StoreError> {
        if !self.exclusive {
            return Err(StoreError::invalid(
                "acquire the exclusive maintenance gate before migrating",
            ));
        }
        let owner = self.owner;
        let conn = self.connection()?;
        require_owner(conn, owner).await?;
        let info = check_server_on(conn).await?;
        if !info.supports_core() {
            return Err(StoreError::invalid("postgresql 17 or newer is required"));
        }
        migrations::run_locked(conn, info.is_supported()).await
    }

    /// Validates domain history through this control connection without any DDL.
    pub async fn validate_schema(&mut self) -> Result<SchemaIdentity, StoreError> {
        migrations::validate(self.connection()?).await
    }

    /// Inspects known domain history without DDL; an uninitialized schema is zero.
    ///
    /// Missing known migrations are allowed for planning. Dirty, modified and
    /// unknown history remains an error and does not imply runtime compatibility.
    pub async fn inspect_schema(&mut self) -> Result<SchemaIdentity, StoreError> {
        migrations::inspect(self.connection()?).await
    }

    /// Validates the exact supported schema and explicitly reopens runtime admission.
    ///
    /// Requires the exclusive gate; callers must complete their other readiness
    /// checks before calling this method. A failed check leaves ownership intact.
    pub async fn finish(mut self) -> Result<(), StoreError> {
        if !self.exclusive {
            return Err(StoreError::invalid(
                "acquire the exclusive maintenance gate before finishing",
            ));
        }
        let owner = self.owner;
        let conn = self.connection()?;
        require_owner(conn, owner).await?;
        migrations::validate(conn).await?;
        let removed = sqlx::query(
            "DELETE FROM public._knowell_maintenance WHERE singleton = true AND owner = $1",
        )
        .bind(owner)
        .execute(&mut *conn)
        .await?;
        if removed.rows_affected() != 1 {
            return Err(StoreError::MaintenanceOwnershipLost);
        }
        if let Some(conn) = self.conn.take() {
            conn.close().await?;
        }
        Ok(())
    }

    fn connection(&mut self) -> Result<&mut PgConnection, StoreError> {
        self.conn
            .as_mut()
            .ok_or(StoreError::MaintenanceOwnershipLost)
    }
}

/// Acquires one session-level shared gate per physical pooled connection.
pub(crate) async fn admit(conn: &mut PgConnection) -> Result<(), StoreError> {
    let admitted: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock_shared($1)")
        .bind(DATA_GATE)
        .fetch_one(&mut *conn)
        .await?;
    if !admitted {
        return Err(StoreError::MaintenanceBusy);
    }
    check_open(conn).await?;
    migrations::validate(conn).await?;
    Ok(())
}

pub(crate) async fn check_open(conn: &mut PgConnection) -> Result<(), StoreError> {
    if let Some(owner) = owner_on(conn).await? {
        return Err(StoreError::MaintenanceRequired { owner });
    }
    Ok(())
}

pub(crate) async fn owner_on(conn: &mut PgConnection) -> Result<Option<Uuid>, StoreError> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public._knowell_maintenance') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await?;
    if !exists {
        return Ok(None);
    }
    let recorded: Option<(i32, Uuid)> = sqlx::query_as(
        "SELECT protocol, owner FROM public._knowell_maintenance WHERE singleton = true",
    )
    .fetch_optional(&mut *conn)
    .await?;
    match recorded {
        Some((protocol, _)) if i64::from(protocol) != i64::from(RUNTIME_PROTOCOL_VERSION) => {
            Err(StoreError::Corrupt(
                "unsupported database runtime admission protocol; update the engine".into(),
            ))
        }
        Some((_, owner)) => Ok(Some(owner)),
        None => Ok(None),
    }
}

async fn require_owner(conn: &mut PgConnection, owner: Uuid) -> Result<(), StoreError> {
    if owner_on(conn).await? == Some(owner) {
        Ok(())
    } else {
        Err(StoreError::MaintenanceOwnershipLost)
    }
}
