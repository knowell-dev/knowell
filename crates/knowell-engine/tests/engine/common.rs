//! Test harness: one fresh database per test, the acme-goods fixture as git
//! repositories, a FakeEmbedder engine and two identities.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use knowell_auth::{Grant, GrantSet, Principal, ResourceScope, Role, UserId};
use knowell_config::{
    EmbeddingPreset, EngineConfig, Origin, ResolvedWorkspace, Sourced, load_workspace, parse_engine,
};
use knowell_core::Name;
use knowell_embed::{AnyEmbedder, FAKE_MODEL, FakeEmbedder};
use knowell_engine::{Engine, EngineSettings, StaticAccess};
use knowell_eval::{Fixture, FixtureSpec, Scale, WriteOptions, generate};
use knowell_index::{GitConfigMode, IndexerConfig, Priority};
use knowell_mcp::{Caller, ClientIdentity, TransportKind};
use knowell_store::{PgConnectOptions, Store, StoreOptions};
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

/// Vector size of the fake embedder.
pub(crate) const DIMS: u32 = 64;

pub(crate) struct TestDb {
    pub(crate) store: Store,
    _guard: DropDatabase,
}

impl TestDb {
    pub(crate) async fn create(test: &str) -> Option<TestDb> {
        Self::create_from(test, ENV).await
    }

    pub(crate) async fn create_from(test: &str, variable: &str) -> Option<TestDb> {
        let Ok(url) = std::env::var(variable) else {
            assert!(
                !strict(),
                "{test} cannot run: {variable} is not set, and {STRICT_ENV}=1 forbids skipping"
            );
            eprintln!("skipping {test}: {variable} is not set");
            return None;
        };
        let admin = PgConnectOptions::from_str(&url)
            .unwrap_or_else(|_| panic!("{variable} is not a valid postgres url"));
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
        let guard = DropDatabase {
            admin: admin.clone(),
            name: name.clone(),
        };
        let options = StoreOptions {
            max_connections: 16,
            acquire_timeout: Duration::from_secs(60),
            application_name: "knowell-engine-tests".into(),
            ..StoreOptions::default()
        };
        let store = Store::connect_with(admin.database(&name), &options)
            .await
            .unwrap();
        store.migrate().await.unwrap();
        Some(TestDb {
            store,
            _guard: guard,
        })
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

/// The Small fixture (seed 42) written as git repositories.
pub(crate) struct Workspace {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) fixture: Fixture,
    pub(crate) resolved: ResolvedWorkspace,
    global_config: PathBuf,
}

impl Workspace {
    pub(crate) fn root(&self) -> PathBuf {
        self.dir.path().join("ws")
    }

    pub(crate) fn project_dir(&self, project: &str) -> PathBuf {
        self.root().join(project)
    }

    pub(crate) fn command(&self, cwd: &Path) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(cwd);
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR",
            "GIT_NAMESPACE",
            "GIT_CONFIG",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
        ] {
            cmd.env_remove(var);
        }
        cmd.env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &self.global_config)
            .env("GIT_AUTHOR_NAME", "Knowell Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Knowell Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .env("GIT_AUTHOR_DATE", "2026-02-01T00:00:00+00:00")
            .env("GIT_COMMITTER_DATE", "2026-02-01T00:00:00+00:00")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.autocrlf=false",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
            ]);
        cmd
    }

    pub(crate) fn git_in(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self
            .command(cwd)
            .args(args)
            .output()
            .expect("git must be installed to run these tests");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    pub(crate) fn git(&self, project: &str, args: &[&str]) -> String {
        self.git_in(&self.project_dir(project), args)
    }

    pub(crate) fn commit_all(&self, project: &str, message: &str) -> String {
        self.git(project, &["add", "-A"]);
        self.git(project, &["commit", "-q", "-m", message]);
        self.git(project, &["rev-parse", "HEAD"])
    }
}

pub(crate) fn git_available() -> bool {
    let available = Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(
        available || !strict(),
        "git is not available, and {STRICT_ENV}=1 forbids skipping"
    );
    available
}

pub(crate) fn fixture_workspace() -> Workspace {
    let fixture = generate(&FixtureSpec {
        seed: 42,
        scale: Scale::Small,
    });
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let manifest = fixture
        .write_to(&root, &WriteOptions { git: true })
        .unwrap();
    let config = load_workspace(&root.join(&manifest.workspace_config)).unwrap();
    let mut resolved = config.resolve(&root).unwrap();
    for project in &mut resolved.projects {
        project.embedding.provider = Some(Sourced {
            value: name("local"),
            origin: Origin::Workspace,
        });
        project.embedding.model = Some(Sourced {
            value: FAKE_MODEL.to_owned(),
            origin: Origin::Workspace,
        });
        project.embedding.preset = Sourced {
            value: EmbeddingPreset::Custom,
            origin: Origin::Workspace,
        };
        project.embedding.dimensions = Sourced {
            value: DIMS,
            origin: Origin::Workspace,
        };
    }
    let global_config = dir.path().join("empty-global.gitconfig");
    std::fs::write(&global_config, "").unwrap();
    Workspace {
        dir,
        fixture,
        resolved,
        global_config,
    }
}

/// A local provider `local` (ollama kind, the fake model) and a cloud
/// provider `cloud` (gemini kind; its key is only a reference, never read).
pub(crate) fn engine_config() -> EngineConfig {
    parse_engine(&format!(
        "version = 1\n[providers.local]\nkind = \"ollama\"\nmodel = \"{FAKE_MODEL}\"\n[providers.cloud]\nkind = \"gemini\"\napi_key = \"env:KNOWELL_TEST_UNUSED_PROVIDER_KEY\"\nmodel = \"{FAKE_MODEL}\"\n"
    ))
    .unwrap()
}

pub(crate) fn indexer_config(data_dir: &Path) -> IndexerConfig {
    let mut config = IndexerConfig::new(data_dir, name("acme"));
    config.git_config = GitConfigMode::Isolated;
    config.jobs.backoff.base = Duration::from_millis(10);
    config.jobs.backoff.max = Duration::from_millis(50);
    config
}

pub(crate) fn alice() -> UserId {
    UserId::new(uuid::Uuid::from_u128(0xA11CE))
}

pub(crate) fn bob() -> UserId {
    UserId::new(uuid::Uuid::from_u128(0xB0B))
}

/// Alice is the local user (organization admin); Bob is a hub subject who
/// may only read `billing-api`.
pub(crate) fn access() -> StaticAccess {
    let mut bob_grants = GrantSet::new();
    bob_grants.add(
        Grant::new(
            Principal::User(bob()),
            Role::Member,
            ResourceScope::project(name("acme-goods"), name("billing-api")),
        )
        .unwrap(),
    );
    StaticAccess::local_admin(alice()).with_subject("bob", bob(), bob_grants)
}

pub(crate) fn alice_caller() -> Caller {
    Caller {
        principal: knowell_mcp::Principal::LocalUser,
        transport: TransportKind::Stdio,
        client: Some(ClientIdentity {
            name: "claude-code".into(),
            version: "2.0".into(),
        }),
    }
}

pub(crate) fn bob_caller() -> Caller {
    Caller {
        principal: knowell_mcp::Principal::Subject { id: "bob".into() },
        transport: TransportKind::StreamableHttp,
        client: Some(ClientIdentity {
            name: "codex".into(),
            version: "1.0".into(),
        }),
    }
}

/// Builds the engine over `db` and indexes the whole workspace.
pub(crate) async fn indexed_engine(db: &TestDb, ws: &Workspace, data_dir: &Path) -> Engine {
    indexed_engine_with_access(db, ws, data_dir, Arc::new(access())).await
}

pub(crate) async fn indexed_engine_with_access(
    db: &TestDb,
    ws: &Workspace,
    data_dir: &Path,
    access: Arc<dyn knowell_engine::AccessResolver>,
) -> Engine {
    let embedder = Arc::new(AnyEmbedder::Fake(FakeEmbedder::new(DIMS).unwrap()));
    let engine = Engine::builder(db.store.clone(), indexer_config(data_dir))
        .engine_config(&engine_config())
        .embedder(name("local"), Arc::clone(&embedder))
        .embedder(name("cloud"), embedder)
        .workspace(ws.resolved.clone())
        .settings(EngineSettings::default())
        .access(access)
        .build()
        .await
        .unwrap();
    let (_, outcomes) = engine
        .indexer()
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    for outcome in outcomes {
        assert!(
            !matches!(outcome, knowell_index::SyncOutcome::Failed { .. }),
            "{outcome:?}"
        );
    }
    engine
}
