//! Test harness: one fresh database per test.
//!
//! `KNOWELL_TEST_DATABASE_URL` points to an admin connection of a
//! PostgreSQL server with pgvector. Each test creates a randomly named
//! database, migrates it, and drops it (also on panic). Without the
//! variable, tests print one skip line and pass (they fail under
//! `KNOWELL_TEST_STRICT=1`).
//! `KNOWELL_TEST_PLAIN_DATABASE_URL` selects a second, unmodified PostgreSQL
//! server for tests which require pgvector to be unavailable.

use std::str::FromStr;
use std::time::Duration;

use knowell_core::{Name, RepoPath, TrackTarget};
use knowell_store::hierarchy::{self, Organization, Project, Workspace};
use knowell_store::views::{self, View};
use knowell_store::{PgConnectOptions, SourceKind, Store, StoreOptions};
use sqlx::{ConnectOptions, Connection};

pub(crate) const ENV: &str = "KNOWELL_TEST_DATABASE_URL";
pub(crate) const PLAIN_ENV: &str = "KNOWELL_TEST_PLAIN_DATABASE_URL";

/// Set to `1` (CI database partitions, the Docker test runner) to make a
/// missing prerequisite (database URL, `git`) fail the test instead of
/// printing a skip line, so a suite that never ran cannot pass.
pub(crate) const STRICT_ENV: &str = "KNOWELL_TEST_STRICT";

/// Whether [`STRICT_ENV`] forbids skipping.
pub(crate) fn strict() -> bool {
    std::env::var(STRICT_ENV).is_ok_and(|value| value == "1")
}

/// The admin connection options, or `None` (with a skip notice) when the
/// variable is unset. The URL itself is never printed.
pub(crate) fn admin_options(test: &str) -> Option<PgConnectOptions> {
    admin_options_from(test, ENV)
}

fn admin_options_from(test: &str, variable: &str) -> Option<PgConnectOptions> {
    let Ok(url) = std::env::var(variable) else {
        assert!(
            !strict(),
            "{test} cannot run: {variable} is not set, and {STRICT_ENV}=1 forbids skipping"
        );
        eprintln!("skipping {test}: {variable} is not set");
        return None;
    };
    match PgConnectOptions::from_str(&url) {
        Ok(options) => Some(options),
        Err(_) => panic!("{variable} is not a valid postgres url"),
    }
}

pub(crate) struct TestDb {
    pub(crate) store: Store,
    // Declared after `store`, so it drops (and drops the database) last.
    _guard: DropDatabase,
}

impl TestDb {
    pub(crate) async fn create(test: &str) -> Option<TestDb> {
        let db = Self::unmigrated(test, ENV).await?;
        db.store.migrate().await.unwrap();
        Some(db)
    }

    pub(crate) async fn unmigrated(test: &str, variable: &str) -> Option<TestDb> {
        let admin = admin_options_from(test, variable)?;
        let name = format!("knowell_test_{}", uuid::Uuid::now_v7().simple());
        let mut conn = admin
            .connect()
            .await
            .unwrap_or_else(|e| panic!("cannot reach the test server: {e}"));
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        // From here on the database is dropped even if a later step panics.
        let guard = DropDatabase {
            admin: admin.clone(),
            name: name.clone(),
        };
        let options = StoreOptions {
            max_connections: 12,
            acquire_timeout: Duration::from_secs(30),
            application_name: "knowell-store-tests".into(),
            ..StoreOptions::default()
        };
        let store = Store::connect_with(admin.database(&name), &options)
            .await
            .unwrap();
        Some(TestDb {
            store,
            _guard: guard,
        })
    }

    pub(crate) async fn conn(&self) -> sqlx::pool::PoolConnection<sqlx::Postgres> {
        self.store.acquire().await.unwrap()
    }
}

struct DropDatabase {
    admin: PgConnectOptions,
    name: String,
}

impl Drop for DropDatabase {
    fn drop(&mut self) {
        let admin = self.admin.clone();
        let name = self.name.clone();
        // Drop runs inside the test's runtime; use a separate thread with its
        // own runtime. WITH (FORCE) ends the pool's open sessions.
        let handle = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                if let Ok(mut conn) = admin.connect().await {
                    let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
                        "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
                    )))
                    .execute(&mut conn)
                    .await;
                    let _ = conn.close().await;
                }
            });
        });
        let _ = handle.join();
    }
}

/// `let db = require_db!();` — returns from the test when no server is set.
macro_rules! require_db {
    () => {
        match crate::common::TestDb::create(module_path!()).await {
            Some(db) => db,
            None => return,
        }
    };
}
pub(crate) use require_db;

pub(crate) fn name(text: &str) -> Name {
    Name::new(text).unwrap()
}

pub(crate) fn path(text: &str) -> RepoPath {
    RepoPath::new(text).unwrap()
}

pub(crate) fn commit(n: u8) -> String {
    format!("{n:02x}").repeat(20)
}

/// One organization with a workspace, a git source, a project and a view
/// following `branch:main`.
pub(crate) struct Fixture {
    pub(crate) org: Organization,
    pub(crate) workspace: Workspace,
    pub(crate) project: Project,
    pub(crate) view: View,
}

pub(crate) async fn fixture(conn: &mut sqlx::PgConnection, tag: &str) -> Fixture {
    let org = hierarchy::create_organization(conn, &name(&format!("org-{tag}")))
        .await
        .unwrap();
    let workspace = hierarchy::create_workspace(conn, org.id, &name("main"))
        .await
        .unwrap();
    let source = hierarchy::create_source(conn, org.id, SourceKind::Git, &format!("/repos/{tag}"))
        .await
        .unwrap();
    let project = hierarchy::create_project(conn, workspace.id, source.id, &name(tag), None)
        .await
        .unwrap();
    let target: TrackTarget = "branch:main".parse().unwrap();
    let view = views::create_view(conn, project.id, &target).await.unwrap();
    Fixture {
        org,
        workspace,
        project,
        view,
    }
}

/// Adds another project (own source, own `branch:main` view) to a workspace.
pub(crate) async fn add_project(
    conn: &mut sqlx::PgConnection,
    fx: &Fixture,
    tag: &str,
) -> (Project, View) {
    let source =
        hierarchy::create_source(conn, fx.org.id, SourceKind::Git, &format!("/repos/{tag}"))
            .await
            .unwrap();
    let project = hierarchy::create_project(conn, fx.workspace.id, source.id, &name(tag), None)
        .await
        .unwrap();
    let target: TrackTarget = "branch:main".parse().unwrap();
    let view = views::create_view(conn, project.id, &target).await.unwrap();
    (project, view)
}
