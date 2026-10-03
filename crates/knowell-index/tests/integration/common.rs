//! Test harness: one fresh database per test, generated git workspaces,
//! isolated git commands, and a counting fake embedder.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use knowell_config::{
    EmbeddingPreset, EngineConfig, Origin, ResolvedWorkspace, Sourced, load_workspace, parse_engine,
};
use knowell_core::{ContentHash, Name, RepoPath};
use knowell_embed::{
    DocumentInput, EmbedError, Embedded, Embedder, Embedding, EmbeddingProfile, FAKE_MODEL,
    FakeEmbedder, Usage,
};
use knowell_eval::{FixtureManifest, FixtureSpec, Scale, WriteOptions, generate};
use knowell_index::{GitConfigMode, Indexer, IndexerConfig};
use knowell_store::content;
use knowell_store::views::{self, GenerationPin};
use knowell_store::{PgConnectOptions, Store, StoreOptions, ViewId};
use sqlx::{ConnectOptions, Connection};

pub(crate) const ENV: &str = "KNOWELL_TEST_DATABASE_URL";

/// Set to `1` (CI database partitions, the Docker test runner) to make a
/// missing prerequisite (database URL, `git`) fail the test instead of
/// printing a skip line, so a suite that never ran cannot pass.
pub(crate) const STRICT_ENV: &str = "KNOWELL_TEST_STRICT";

/// Whether [`STRICT_ENV`] forbids skipping.
pub(crate) fn strict() -> bool {
    std::env::var(STRICT_ENV).is_ok_and(|value| value == "1")
}

/// Generous bound for anything asynchronous (watchers, workers).
pub(crate) const PATIENCE: Duration = Duration::from_secs(90);

pub(crate) fn admin_options(test: &str) -> Option<PgConnectOptions> {
    let Ok(url) = std::env::var(ENV) else {
        assert!(
            !strict(),
            "{test} cannot run: {ENV} is not set, and {STRICT_ENV}=1 forbids skipping"
        );
        eprintln!("skipping {test}: {ENV} is not set");
        return None;
    };
    match PgConnectOptions::from_str(&url) {
        Ok(options) => Some(options),
        Err(_) => panic!("{ENV} is not a valid postgres url"),
    }
}

pub(crate) struct TestDb {
    pub(crate) store: Store,
    _guard: DropDatabase,
}

impl TestDb {
    pub(crate) async fn create(test: &str) -> Option<TestDb> {
        let admin = admin_options(test)?;
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
            application_name: "knowell-index-tests".into(),
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

    pub(crate) async fn conn(&self) -> sqlx::pool::PoolConnection<sqlx::Postgres> {
        self.store.acquire().await.unwrap()
    }

    /// A single number from a SQL query.
    pub(crate) async fn count(&self, sql: &str) -> i64 {
        let mut conn = self.conn().await;
        sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.to_owned()))
            .fetch_one(&mut *conn)
            .await
            .unwrap()
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

/// A generated fixture workspace written as git repositories.
pub(crate) struct Workspace {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) manifest: FixtureManifest,
    pub(crate) resolved: ResolvedWorkspace,
    pub(crate) canaries: Vec<String>,
    global_config: PathBuf,
}

impl Workspace {
    pub(crate) fn root(&self) -> PathBuf {
        self.dir.path().join("ws")
    }

    pub(crate) fn project_dir(&self, project: &str) -> PathBuf {
        self.root().join(project)
    }

    /// A git command in `cwd` with isolated, deterministic settings.
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
                "-c",
                "advice.detachedHead=false",
            ]);
        cmd
    }

    /// Runs git in a project; panics with stderr on failure.
    pub(crate) fn git(&self, project: &str, args: &[&str]) -> String {
        let output = self
            .command(&self.project_dir(project))
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

    pub(crate) fn head(&self, project: &str) -> String {
        self.git(project, &["rev-parse", "HEAD"])
    }

    pub(crate) fn write(&self, project: &str, rel: &str, text: &str) {
        let file = self.project_dir(project).join(rel);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(file, text).unwrap();
    }

    pub(crate) fn read(&self, project: &str, rel: &str) -> String {
        std::fs::read_to_string(self.project_dir(project).join(rel)).unwrap()
    }

    /// Stages everything and commits; returns the new commit.
    pub(crate) fn commit_all(&self, project: &str, message: &str) -> String {
        self.git(project, &["add", "-A"]);
        self.git(project, &["commit", "-q", "-m", message]);
        self.head(project)
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

/// The Small fixture (seed 42) as git repositories, resolved; restricted to
/// `projects` when given.
pub(crate) fn fixture_workspace(projects: Option<&[&str]>) -> Workspace {
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
    if let Some(keep) = projects {
        resolved
            .projects
            .retain(|p| keep.contains(&p.name.as_str()));
        assert_eq!(
            resolved.projects.len(),
            keep.len(),
            "unknown fixture project"
        );
    }
    let global_config = dir.path().join("empty-global.gitconfig");
    std::fs::write(&global_config, "").unwrap();
    Workspace {
        canaries: fixture.canaries(),
        dir,
        manifest,
        resolved,
        global_config,
    }
}

/// Vector size used by the tests (small: fast and plenty for a fake).
pub(crate) const DIMS: u32 = 64;

/// Makes every project embed with `provider` (model of the fake embedder).
pub(crate) fn embed_with(resolved: &mut ResolvedWorkspace, provider: &str) {
    for project in &mut resolved.projects {
        project.embedding.provider = Some(Sourced {
            value: name(provider),
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
}

/// An engine config with a local provider `local` (ollama kind) and a cloud
/// provider `cloud` (gemini kind; the key is only a reference, never read).
pub(crate) fn engine() -> EngineConfig {
    parse_engine(&format!(
        "version = 1\n[providers.local]\nkind = \"ollama\"\nmodel = \"{FAKE_MODEL}\"\n[providers.cloud]\nkind = \"gemini\"\napi_key = \"env:KNOWELL_TEST_UNUSED_PROVIDER_KEY\"\nmodel = \"{FAKE_MODEL}\"\n"
    ))
    .unwrap()
}

/// `FakeEmbedder` that counts calls and inputs, and can simulate an outage.
#[derive(Debug)]
pub(crate) struct CountingEmbedder {
    inner: FakeEmbedder,
    calls: AtomicU64,
    inputs: AtomicU64,
    /// While set, every call fails like a provider timeout.
    down: AtomicBool,
    /// Simulated provider latency per call, in milliseconds.
    delay_ms: AtomicU64,
}

impl CountingEmbedder {
    pub(crate) fn new() -> Self {
        Self {
            inner: FakeEmbedder::new(DIMS).unwrap(),
            calls: AtomicU64::new(0),
            inputs: AtomicU64::new(0),
            down: AtomicBool::new(false),
            delay_ms: AtomicU64::new(0),
        }
    }

    /// Makes every call take `delay` (a slow remote provider).
    pub(crate) fn set_delay(&self, delay: Duration) {
        let ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
        self.delay_ms.store(ms, Ordering::SeqCst);
    }

    pub(crate) fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    pub(crate) fn calls(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }

    pub(crate) fn inputs(&self) -> u64 {
        self.inputs.load(Ordering::SeqCst)
    }
}

impl Embedder for CountingEmbedder {
    fn profile(&self) -> &EmbeddingProfile {
        self.inner.profile()
    }

    async fn embed_documents_with_usage(
        &self,
        documents: &[DocumentInput],
    ) -> Result<Embedded, EmbedError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let delay = self.delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        if self.down.load(Ordering::SeqCst) {
            return Err(EmbedError::Timeout {
                provider: "fake",
                attempts: 1,
            });
        }
        self.inputs
            .fetch_add(documents.len() as u64, Ordering::SeqCst);
        self.inner.embed_documents_with_usage(documents).await
    }

    async fn embed_query_with_usage(&self, query: &str) -> Result<(Embedding, Usage), EmbedError> {
        self.inner.embed_query_with_usage(query).await
    }
}

/// Engine settings for tests: isolated git config, quick retries.
pub(crate) fn config(data_dir: &Path) -> IndexerConfig {
    let mut config = IndexerConfig::new(data_dir, name("acme"));
    config.git_config = GitConfigMode::Isolated;
    config.jobs.backoff.base = Duration::from_millis(10);
    config.jobs.backoff.max = Duration::from_millis(50);
    config.watch.debounce = Duration::from_millis(100);
    config
}

/// An indexer with the counting embedder registered as `local` and `cloud`.
pub(crate) fn indexer(
    db: &TestDb,
    data_dir: &Path,
    embedder: &Arc<CountingEmbedder>,
) -> Indexer<CountingEmbedder> {
    indexer_with(db, config(data_dir), embedder)
}

pub(crate) fn indexer_with(
    db: &TestDb,
    config: IndexerConfig,
    embedder: &Arc<CountingEmbedder>,
) -> Indexer<CountingEmbedder> {
    Indexer::builder(db.store.clone(), config)
        .engine(&engine())
        .embedder(name("local"), Arc::clone(embedder))
        .embedder(name("cloud"), Arc::clone(embedder))
        .build()
        .unwrap()
}

/// The view of the (only) registered project named `project`.
pub(crate) fn view_of(registration: &knowell_index::Registration, project: &str) -> ViewId {
    registration
        .views
        .iter()
        .find(|v| v.project.as_str() == project)
        .unwrap_or_else(|| panic!("project {project} is not registered"))
        .view
}

pub(crate) async fn active_pin(db: &TestDb, view: ViewId) -> GenerationPin {
    let mut conn = db.conn().await;
    let row = views::get_view(&mut conn, view).await.unwrap().unwrap();
    GenerationPin {
        view,
        generation: row
            .active_generation
            .expect("view has an active generation"),
    }
}

/// Path → content hash of the view's active generation.
pub(crate) async fn active_files(
    db: &TestDb,
    view: ViewId,
) -> std::collections::BTreeMap<RepoPath, ContentHash> {
    let pin = active_pin(db, view).await;
    let mut conn = db.conn().await;
    content::files_at(&mut conn, pin)
        .await
        .unwrap()
        .into_iter()
        .map(|f| (f.path, f.content_hash))
        .collect()
}

/// Waits until `check` holds, polling every 100 ms up to [`PATIENCE`].
pub(crate) async fn wait_for<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}
