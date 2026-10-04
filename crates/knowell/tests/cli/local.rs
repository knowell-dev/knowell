//! Local indexing and retrieval through fresh CLI processes.

use std::collections::BTreeMap;
use std::future::IntoFuture;
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use knowell_core::ContentHash;
use serde_json::Value;

use crate::common::{Run, Sandbox, ScratchDb, admin_url, git_available};

const WORKSPACE: &str = "version = 1\n\
    [workspace]\nname = 'synthetic-local'\ntrack = 'branch:main'\n\
    [[project]]\nname = 'alpha'\npath = 'alpha'\n\
    [[project]]\nname = 'beta'\npath = 'beta'\n";
const ENGINE: &str = "version = 1\n\
    [database]\nmode = 'external'\nurl = 'env:KNOWELL_CLI_LOCAL_DB_URL'\n";
const ENV_CANARY: &str = "KNOWELL_CANARY_local_env_2d8f";
const ALPHA_SOURCE: &str = concat!(
    "export function AlphaToken(value: string): string {\n",
    "  return value.toUpperCase();\n",
    "}\n",
);
const BETA_SOURCE: &str = concat!(
    "export function BetaToken(value: string): string {\n",
    "  return value.toLowerCase();\n",
    "}\n",
);

fn write_workspace(sb: &Sandbox, text: &str) {
    std::fs::write(sb.work().join("knowell.toml"), text).unwrap();
}

const FAKE_PROVIDER_KEY: &str = "KNOWELL_CANARY_FAKE_GEMINI_PROVIDER_KEY";

/// A bounded loopback-only Gemini protocol stub; it records bodies, never headers.
struct MockGemini {
    address: SocketAddr,
    state: Arc<MockGeminiState>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

struct MockGeminiState {
    requests: Arc<Mutex<Vec<Value>>>,
    failed: AtomicBool,
    wrong_dimensions: AtomicBool,
}

impl MockGemini {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let state = Arc::new(MockGeminiState {
            requests: Arc::new(Mutex::new(Vec::new())),
            failed: AtomicBool::new(false),
            wrong_dimensions: AtomicBool::new(false),
        });
        let server_state = Arc::clone(&state);
        let (stop, stopping) = tokio::sync::oneshot::channel();
        let worker = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let router = Router::new()
                    .route("/v1beta/models/{model}", post(mock_gemini_request))
                    .layer(DefaultBodyLimit::max(1_048_576))
                    .with_state(Arc::clone(&server_state));
                tokio::select! {
                    _ = stopping => {},
                    _ = axum::serve(listener, router).into_future() => {
                        server_state.failed.store(true, Ordering::SeqCst);
                    }
                }
            });
        });
        Self {
            address,
            state,
            stop: Some(stop),
            worker: Some(worker),
        }
    }

    fn bodies(&self) -> Vec<Value> {
        assert!(
            !self.state.failed.load(Ordering::SeqCst),
            "the synthetic provider received an invalid request"
        );
        self.state.requests.lock().unwrap().clone()
    }
}

impl Drop for MockGemini {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

async fn mock_gemini_request(
    State(state): State<Arc<MockGeminiState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let invalid = || {
        state.failed.store(true, Ordering::SeqCst);
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid synthetic request"})),
        )
    };
    if headers
        .get("x-goog-api-key")
        .and_then(|header| header.to_str().ok())
        != Some(FAKE_PROVIDER_KEY)
    {
        return invalid();
    }
    let Some(requests) = body["requests"].as_array() else {
        return invalid();
    };
    if requests.is_empty() || requests.len() > 100 {
        return invalid();
    }
    let mut embeddings = Vec::new();
    for request in requests {
        let Some(dimensions) = request["outputDimensionality"]
            .as_u64()
            .and_then(|dimensions| usize::try_from(dimensions).ok())
            .filter(|dimensions| (128..=3072).contains(dimensions))
        else {
            return invalid();
        };
        let returned = if state.wrong_dimensions.load(Ordering::SeqCst) {
            dimensions + 1
        } else {
            dimensions
        };
        let mut values = vec![0.0f32; returned];
        values[0] = 1.0;
        embeddings.push(serde_json::json!({ "values": values }));
    }
    let mut recorded = state.requests.lock().unwrap();
    if recorded.len() >= 64 {
        return invalid();
    }
    recorded.push(body);
    (
        StatusCode::OK,
        Json(serde_json::json!({ "embeddings": embeddings })),
    )
}

fn provider_workspace(model: &str, dimensions: u32) -> String {
    format!(
        "version = 1\n\
        [workspace]\nname = 'synthetic-local'\ntrack = 'branch:main'\ndata_policy = 'cloud'\n\
        [workspace.embedding]\nprovider = 'synthetic'\nmodel = '{model}'\npreset = 'custom'\ndimensions = {dimensions}\n\
        [[project]]\nname = 'alpha'\npath = 'alpha'\n\
        [[project]]\nname = 'beta'\npath = 'beta'\n"
    )
}

fn assert_complete_embeddings(status: &Value, expected_dimensions: u32) {
    assert_eq!(status["complete"], true);
    assert_eq!(status["tiers"]["t2"]["state"], "done");
    assert_eq!(status["embedding"]["plan"], "embed");
    assert_eq!(status["embedding"]["dimensions"], expected_dimensions);
    let coverage = &status["embedding_coverage"];
    assert_eq!(coverage["complete"], true);
    assert_eq!(coverage["generation"], status["active_generation"]);
    assert_eq!(coverage["profile"], status["embedding"]["profile"]);
    assert!(coverage["inputs"].as_u64().unwrap() > 0);
    assert_eq!(coverage["embedded"], coverage["inputs"]);
}

fn configure_mock_provider(sb: &mut Sandbox, provider: &MockGemini) {
    sb.set_env("KNOWELL_CLI_FAKE_EMBEDDING_KEY", FAKE_PROVIDER_KEY);
    sb.write_engine(&format!(
        "{ENGINE}\n[providers.synthetic]\nkind = 'gemini'\n\
        model = 'provider-default-model'\napi_key = 'env:KNOWELL_CLI_FAKE_EMBEDDING_KEY'\n\
        base_url = 'http://{}'\n",
        provider.address
    ));
}

#[test]
fn explicit_rebuild_embeds_unchanged_sources_with_the_selected_model_and_dimensions() {
    let Some((mut sb, _db, fixture)) = database_fixture("local explicit profile rebuild") else {
        return;
    };
    let original = json(&sb, &["index", "--json"], 0, &fixture);
    let provider = MockGemini::start();
    configure_mock_provider(&mut sb, &provider);
    let mut previous = original;
    for (model, dimensions) in [("synthetic-model-one", 128), ("synthetic-model-two", 256)] {
        write_workspace(&sb, &provider_workspace(model, dimensions));
        let requests_before = provider.bodies().len();
        let incomplete = json(&sb, &["index", "--json"], 1, &fixture);
        assert_eq!(incomplete["complete"], false);
        assert_eq!(incomplete["run"]["jobs"], 0);
        assert_eq!(
            provider.bodies().len(),
            requests_before,
            "default indexing must not silently reembed"
        );
        for name in ["alpha", "beta"] {
            let status = project(&incomplete, name);
            assert_eq!(
                status["active_generation"],
                project(&previous, name)["active_generation"]
            );
            assert!(
                status["incomplete_reasons"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|reason| reason
                        .as_str()
                        .is_some_and(|reason| reason.contains("know index --rebuild")))
            );
        }
        let rebuilt = json(&sb, &["index", "--rebuild", "--json"], 0, &fixture);
        assert_eq!(rebuilt["complete"], true);
        assert_eq!(rebuilt["run"]["failed"], 0);
        for name in ["alpha", "beta"] {
            let status = project(&rebuilt, name);
            assert_complete_embeddings(status, dimensions);
            assert_eq!(
                status["active_generation"].as_i64().unwrap(),
                project(&previous, name)["active_generation"]
                    .as_i64()
                    .unwrap()
                    + 1
            );
            assert_eq!(
                status["active_commit"],
                fixture.commits.get(name).unwrap().as_str()
            );
            assert_eq!(
                git(&sb, &sb.work().join(name), &["rev-parse", "HEAD"]),
                *fixture.commits.get(name).unwrap()
            );
        }
        let bodies = provider.bodies();
        assert!(bodies.len() > requests_before);
        for body in &bodies[requests_before..] {
            let text = serde_json::to_string(body).unwrap();
            assert!(!text.contains(ENV_CANARY));
            assert!(!text.contains(&fixture.fake_key));
            assert!(!text.contains(FAKE_PROVIDER_KEY));
            for request in body["requests"].as_array().unwrap() {
                assert_eq!(request["model"], format!("models/{model}"));
                assert_eq!(request["outputDimensionality"], dimensions);
            }
        }
        let unchanged = json(&sb, &["index", "--json"], 0, &fixture);
        assert_eq!(unchanged["run"]["jobs"], 0);
        assert_eq!(provider.bodies().len(), bodies.len());
        for name in ["alpha", "beta"] {
            assert_eq!(
                project(&unchanged, name)["active_generation"],
                project(&rebuilt, name)["active_generation"]
            );
        }
        previous = rebuilt;
    }
}

#[test]
fn invalid_provider_vectors_leave_failed_attempts_incomplete_until_rebuild_recovers() {
    let Some((mut sb, _db, fixture)) = database_fixture("local invalid provider coverage") else {
        return;
    };
    let provider = MockGemini::start();
    configure_mock_provider(&mut sb, &provider);
    write_workspace(&sb, &provider_workspace("synthetic-model", 128));
    provider
        .state
        .wrong_dimensions
        .store(true, Ordering::SeqCst);
    let failed = json(&sb, &["index", "--rebuild", "--json"], 1, &fixture);
    assert_eq!(failed["complete"], false);
    // Malformed provider responses are retryable failed attempts. The active
    // source generation remains available while T2 and its coverage are incomplete.
    assert!(failed["run"]["failed"].as_u64().unwrap() > 0);
    assert_eq!(
        failed["run"]["jobs"].as_u64().unwrap(),
        failed["run"]["succeeded"].as_u64().unwrap() + failed["run"]["failed"].as_u64().unwrap()
    );
    for name in ["alpha", "beta"] {
        let status = project(&failed, name);
        assert_eq!(status["complete"], false);
        assert_eq!(status["tiers"]["t2"]["state"], "running");
        assert_eq!(status["embedding_coverage"]["complete"], false);
        assert_eq!(status["embedding_coverage"]["embedded"], 0);
        assert!(status["embedding_coverage"]["inputs"].as_u64().unwrap() > 0);
        assert!(!status["incomplete_reasons"].as_array().unwrap().is_empty());
        assert_eq!(
            status["active_commit"],
            fixture.commits.get(name).unwrap().as_str()
        );
    }
    provider
        .state
        .wrong_dimensions
        .store(false, Ordering::SeqCst);
    let recovered = json(&sb, &["index", "--rebuild", "--json"], 0, &fixture);
    assert_eq!(recovered["complete"], true);
    assert_eq!(recovered["run"]["failed"], 0);
    for name in ["alpha", "beta"] {
        let status = project(&recovered, name);
        assert_complete_embeddings(status, 128);
        assert_eq!(
            status["active_generation"].as_i64().unwrap(),
            project(&failed, name)["active_generation"]
                .as_i64()
                .unwrap()
                + 1
        );
        assert_eq!(
            status["active_commit"],
            fixture.commits.get(name).unwrap().as_str()
        );
        assert_eq!(
            git(&sb, &sb.work().join(name), &["rev-parse", "HEAD"]),
            *fixture.commits.get(name).unwrap()
        );
    }
    assert!(!provider.bodies().is_empty());
}

fn strict_sandbox() -> Sandbox {
    let mut sb = Sandbox::new();
    sb.write_engine(ENGINE);
    // Validation must fail before this deliberately unavailable database.
    sb.set_env("KNOWELL_CLI_LOCAL_DB_URL", "");
    write_workspace(&sb, WORKSPACE);
    sb
}

fn assert_error(sb: &Sandbox, args: &[&str], expected: &str) {
    let output = sb.run_with_timeout(args, Duration::from_secs(15));
    assert_eq!(output.code, 2, "expected a configuration/usage error");
    assert!(
        output.stdout.trim().is_empty(),
        "errors must not contaminate JSON stdout"
    );
    assert!(
        output.stderr.contains(expected),
        "missing expected diagnostic: {expected}"
    );
    assert!(
        !sb.knowell_home().join("data").exists(),
        "validation opened the local index"
    );
}

#[test]
fn local_commands_require_explicit_valid_configuration() {
    let sb = Sandbox::new();
    for args in [
        vec!["index", "--json"],
        vec!["search", "AlphaToken", "--json"],
        vec!["status", "--json"],
    ] {
        assert_error(&sb, &args, "no engine configuration");
    }
    sb.write_engine(ENGINE);
    for args in [
        vec!["index", "--json"],
        vec!["search", "AlphaToken", "--json"],
        vec!["status", "--json"],
    ] {
        assert_error(&sb, &args, "no knowell.toml");
    }
    write_workspace(
        &sb,
        "version = 1\n[workspace]\nname = 'synthetic-local'\nunknown = true\n",
    );
    assert_error(&sb, &["index", "--json"], "unknown field");
}

#[test]
fn local_commands_reject_non_standalone_roles_before_opening_a_store() {
    let sb = strict_sandbox();
    for role in ["hub", "worker"] {
        sb.write_engine(&format!("{ENGINE}\n[server]\nrole = '{role}'\n"));
        for args in [
            vec!["index", "--json"],
            vec!["search", "AlphaToken", "--json"],
            vec!["status", "--json"],
        ] {
            assert_error(&sb, &args, "require role `standalone`");
        }
    }
}

#[test]
fn a_missing_configured_provider_cannot_silently_become_lexical_only() {
    let sb = strict_sandbox();
    write_workspace(
        &sb,
        "version = 1\n[workspace]\nname = 'synthetic-local'\ntrack = 'branch:main'\n\
         [workspace.embedding]\nprovider = 'missing'\n\
         [[project]]\nname = 'alpha'\npath = 'alpha'\n",
    );
    for args in [
        vec!["index", "--json"],
        vec!["search", "AlphaToken", "--json"],
        vec!["status", "--json"],
    ] {
        assert_error(&sb, &args, "provider");
    }
}

#[test]
fn a_missing_provider_secret_fails_before_database_access() {
    let mut sb = strict_sandbox();
    sb.set_env("KNOWELL_CLI_FAKE_EMBEDDING_KEY", "");
    sb.write_engine(&format!(
        "{ENGINE}\n[providers.synthetic]\nkind = 'gemini'\n\
        model = 'synthetic-model'\napi_key = 'env:KNOWELL_CLI_FAKE_EMBEDDING_KEY'\n\
        base_url = 'http://127.0.0.1:9'\n"
    ));
    write_workspace(&sb, &provider_workspace("synthetic-model", 128));
    for args in [
        vec!["index", "--json"],
        vec!["search", "AlphaToken", "--json"],
        vec!["status", "--json"],
    ] {
        assert_error(&sb, &args, "resolved to an empty value");
    }
}

#[test]
fn project_and_search_validation_precede_database_access() {
    let sb = strict_sandbox();
    for args in [
        vec!["status", "--project", "unknown", "--json"],
        vec!["search", "AlphaToken", "--project", "unknown", "--json"],
    ] {
        assert_error(&sb, &args, "does not exist in workspace");
    }
    for args in [
        vec![
            "status",
            "--project",
            "alpha",
            "--project",
            "alpha",
            "--json",
        ],
        vec![
            "search",
            "AlphaToken",
            "--project",
            "alpha",
            "--project",
            "alpha",
            "--json",
        ],
    ] {
        assert_error(&sb, &args, "selected more than once");
    }
    for limit in ["0", "101"] {
        assert_error(
            &sb,
            &["search", "AlphaToken", "--limit", limit, "--json"],
            "limit",
        );
    }
    assert_error(&sb, &["search", "   ", "--json"], "query");
}

/// Fixture git never inherits the developer's repository, hooks or identity.
fn git(sb: &Sandbox, repository: &Path, args: &[&str]) -> String {
    let config = sb.home().join("empty-git-config");
    if !config.exists() {
        std::fs::write(&config, "").unwrap();
    }
    let mut command = Command::new("git");
    for name in [
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
        "GIT_TEMPLATE_DIR",
    ] {
        command.env_remove(name);
    }
    let output = command
        .args([
            "-c",
            "user.name=Knowell Test",
            "-c",
            "user.email=knowell-test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.autocrlf=false",
        ])
        .args(args)
        .current_dir(repository)
        .env("HOME", sb.home())
        .env("USERPROFILE", sb.home())
        .env("GIT_CONFIG_GLOBAL", config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Knowell Test")
        .env("GIT_AUTHOR_EMAIL", "knowell-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Knowell Test")
        .env("GIT_COMMITTER_EMAIL", "knowell-test@example.invalid")
        .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "synthetic fixture git command failed"
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct Fixture {
    commits: BTreeMap<String, String>,
    fake_key: String,
}

fn fixture(sb: &Sandbox) -> Fixture {
    let fake_key = format!("AIza{}{}", "FaKe".repeat(8), "FaK");
    let mut commits = BTreeMap::new();
    for (name, source) in [("alpha", ALPHA_SOURCE), ("beta", BETA_SOURCE)] {
        let repository = sb.work().join(name);
        std::fs::create_dir_all(repository.join("src")).unwrap();
        std::fs::write(repository.join("src/token.ts"), source).unwrap();
        // This tracked file must be excluded by path before its content is read.
        std::fs::write(repository.join(".env"), format!("TOKEN={ENV_CANARY}\n")).unwrap();
        // Allowed source is scanned before indexing and before snippets are returned.
        std::fs::write(
            repository.join("src/config.ts"),
            format!("export const apiKey = '{fake_key}';\n"),
        )
        .unwrap();
        git(
            sb,
            &repository,
            &[
                "init",
                "--quiet",
                "--initial-branch=main",
                "--object-format=sha1",
                "--template=",
            ],
        );
        git(sb, &repository, &["add", "--all"]);
        git(
            sb,
            &repository,
            &[
                "commit",
                "--quiet",
                "--no-verify",
                "-m",
                "synthetic local CLI fixture",
            ],
        );
        commits.insert(
            name.to_owned(),
            git(sb, &repository, &["rev-parse", "HEAD"]),
        );
    }
    write_workspace(sb, WORKSPACE);
    Fixture { commits, fake_key }
}

fn database_fixture(test: &str) -> Option<(Sandbox, ScratchDb, Fixture)> {
    let admin = admin_url(test)?;
    if !git_available() {
        eprintln!("skipping {test}: git is unavailable");
        return None;
    }
    let db = ScratchDb::create(&admin);
    let mut sb = Sandbox::new();
    sb.set_env("KNOWELL_CLI_LOCAL_DB_URL", &db.url);
    sb.write_engine(ENGINE);
    let initialized = sb.run_with_timeout(&["init"], Duration::from_secs(120));
    assert!(
        !initialized.all().contains(&db.url),
        "database url leaked during explicit local fixture initialization"
    );
    assert_eq!(
        initialized.code, 0,
        "explicit local fixture database initialization failed"
    );
    let fixture = fixture(&sb);
    Some((sb, db, fixture))
}

fn json(sb: &Sandbox, args: &[&str], expected_code: i32, fixture: &Fixture) -> Value {
    let output: Run = sb.run_with_timeout(args, Duration::from_secs(120));
    let all = output.all();
    assert!(
        !all.contains(ENV_CANARY),
        "excluded file content reached CLI output"
    );
    assert!(
        !all.contains(&fixture.fake_key),
        "unredacted source secret reached CLI output"
    );
    // Do not print captured output on failure: database errors must remain safe too.
    assert_eq!(output.code, expected_code, "unexpected local CLI exit code");
    serde_json::from_str(&output.stdout).expect("stdout must contain exactly one JSON value")
}

fn project<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|project| project["project"] == name)
        .unwrap_or_else(|| panic!("missing synthetic project {name}"))
}

fn gap(report: &Value, reason: &str) -> bool {
    report["gaps"]
        .as_array()
        .is_some_and(|gaps| gaps.iter().any(|gap| gap["reason"] == reason))
}

fn assert_ready(project: &Value, commit: &str) {
    assert_eq!(project["target"], "branch:main");
    assert_eq!(project["active_commit"], commit);
    assert_eq!(project["latest_seen_commit"], commit);
    assert!(project["active_generation"].as_i64().is_some());
    assert!(project["building_generation"].is_null());
    assert!(project["last_error"].is_null());
    for tier in ["t0", "t1", "t3"] {
        assert_eq!(project["tiers"][tier]["state"], "done");
    }
    assert_eq!(project["tiers"]["t2"]["state"], "skipped");
    assert_eq!(project["tiers"]["t2"]["reason"], "no_provider");
    assert_eq!(project["embedding"]["plan"], "skip");
    assert_eq!(project["embedding"]["reason"], "no_provider");
    assert!(project["embedding_coverage"].is_null());
    assert!(project["active_tree_hash"].is_null());
}

#[test]
fn fresh_processes_index_status_and_search_with_source_evidence() {
    let Some((sb, _db, fixture)) = database_fixture("local fresh process retrieval") else {
        return;
    };
    let indexed = json(&sb, &["index", "--json"], 0, &fixture);
    assert_eq!(indexed["workspace"], "synthetic-local");
    assert_eq!(indexed["complete"], true);
    assert_eq!(indexed["run"]["failed"], 0);
    assert!(indexed["run"]["jobs"].as_u64().unwrap() > 0);
    assert_eq!(indexed["run"]["jobs"], indexed["run"]["succeeded"]);
    assert!(indexed["issues"].as_array().unwrap().is_empty());
    assert!(indexed["incomplete_reasons"].as_array().unwrap().is_empty());
    assert_eq!(indexed["projects"].as_array().unwrap().len(), 2);
    for name in ["alpha", "beta"] {
        let status = project(&indexed, name);
        assert_ready(status, fixture.commits.get(name).unwrap());
        assert_eq!(status["complete"], true);
        assert!(status["incomplete_reasons"].as_array().unwrap().is_empty());
        assert_eq!(status["sync"]["outcome"], "queued");
        assert_eq!(status["sync"]["target"]["type"], "commit");
        assert_eq!(
            status["sync"]["target"]["id"],
            fixture.commits.get(name).unwrap().as_str()
        );
    }

    let status = json(
        &sb,
        &["status", "--project", "alpha", "--json"],
        0,
        &fixture,
    );
    assert_eq!(status["workspace"], "synthetic-local");
    assert_eq!(status["projects"].as_array().unwrap().len(), 1);
    assert_ready(
        project(&status, "alpha"),
        fixture.commits.get("alpha").unwrap(),
    );
    assert_eq!(
        project(&status, "alpha")["active_generation"],
        project(&indexed, "alpha")["active_generation"]
    );

    let search = json(
        &sb,
        &[
            "search",
            "AlphaToken",
            "--project",
            "alpha",
            "--limit",
            "1",
            "--no-snippets",
            "--json",
        ],
        0,
        &fixture,
    );
    assert_eq!(search["workspace"], "synthetic-local");
    assert!(search["registration_issues"].as_array().unwrap().is_empty());
    assert!(search["query_class"].is_string());
    assert!(search["more_available"].is_boolean());
    let hits = search["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    let hit = &hits[0];
    assert!(hit.get("snippet").is_none());
    let evidence = &hit["evidence"];
    assert_eq!(evidence["project"], "alpha");
    assert_eq!(evidence["view"], "branch:main");
    assert_eq!(
        evidence["commit"],
        fixture.commits.get("alpha").unwrap().as_str()
    );
    assert_eq!(evidence["path"], "src/token.ts");
    assert_eq!(evidence["lines"]["start"], 1);
    assert_eq!(evidence["lines"]["end"], 3);
    assert_eq!(
        evidence["content_hash"],
        ContentHash::of(ALPHA_SOURCE.as_bytes()).to_string()
    );
    assert_eq!(evidence["index_state"], "current");

    let repeat = json(&sb, &["index", "--json"], 0, &fixture);
    assert_eq!(repeat["complete"], true);
    assert_eq!(repeat["run"]["jobs"], 0);
    for name in ["alpha", "beta"] {
        assert_eq!(
            project(&repeat, name)["active_generation"],
            project(&indexed, name)["active_generation"]
        );
        assert_eq!(project(&repeat, name)["sync"]["outcome"], "up_to_date");
    }

    let snippets = json(
        &sb,
        &[
            "search",
            "apiKey",
            "--project",
            "alpha",
            "--limit",
            "100",
            "--json",
        ],
        0,
        &fixture,
    );
    assert!(
        snippets["hits"]
            .as_array()
            .is_some_and(|hits| !hits.is_empty())
    );
    assert!(snippets["hits"].as_array().unwrap().iter().any(|hit| {
        hit["snippet"]["trust"] == "untrusted" && hit["snippet"]["text"].is_string()
    }));
    let excluded = json(
        &sb,
        &["search", ENV_CANARY, "--project", "alpha", "--json"],
        0,
        &fixture,
    );
    assert!(excluded["hits"].as_array().is_none_or(Vec::is_empty));
    assert!(gap(&excluded, "no_candidates_in_selected_ref"));
}

#[test]
fn unchanged_commit_with_changed_exclusions_refreshes_automatically_then_stays_idle() {
    let Some((sb, _db, fixture)) = database_fixture("local exclusion policy refresh") else {
        return;
    };
    let original = json(&sb, &["index", "--json"], 0, &fixture);
    let before = json(
        &sb,
        &["search", "AlphaToken", "--project", "alpha", "--json"],
        0,
        &fixture,
    );
    assert_eq!(before["hits"].as_array().unwrap().len(), 1);
    assert_eq!(before["hits"][0]["evidence"]["path"], "src/token.ts");
    write_workspace(
        &sb,
        &WORKSPACE.replace(
            "path = 'alpha'\n",
            "path = 'alpha'\nexclude = ['src/token.ts']\n",
        ),
    );

    // This is plain refresh: only content policy changed, not the Git target.
    let refreshed = json(&sb, &["index", "--json"], 0, &fixture);
    assert_eq!(refreshed["complete"], true);
    assert!(refreshed["run"]["jobs"].as_u64().unwrap() > 0);
    assert_eq!(refreshed["run"]["failed"], 0);
    let alpha = project(&refreshed, "alpha");
    assert_ready(alpha, fixture.commits.get("alpha").unwrap());
    assert_eq!(alpha["sync"]["outcome"], "queued");
    assert_eq!(
        alpha["active_generation"].as_i64().unwrap(),
        project(&original, "alpha")["active_generation"]
            .as_i64()
            .unwrap()
            + 1
    );
    assert_eq!(project(&refreshed, "beta")["sync"]["outcome"], "up_to_date");
    assert_eq!(
        project(&refreshed, "beta")["active_generation"],
        project(&original, "beta")["active_generation"]
    );
    for name in ["alpha", "beta"] {
        assert_eq!(
            git(&sb, &sb.work().join(name), &["rev-parse", "HEAD"]),
            *fixture.commits.get(name).unwrap()
        );
    }
    let excluded = json(
        &sb,
        &["search", "AlphaToken", "--project", "alpha", "--json"],
        0,
        &fixture,
    );
    assert!(excluded["hits"].as_array().is_none_or(Vec::is_empty));
    assert!(gap(&excluded, "no_candidates_in_selected_ref"));
    let retained = json(
        &sb,
        &["search", "apiKey", "--project", "alpha", "--json"],
        0,
        &fixture,
    );
    let hits = retained["hits"].as_array().unwrap();
    assert!(!hits.is_empty());
    assert!(hits.iter().all(|hit| {
        hit["evidence"]["path"] == "src/config.ts"
            && hit["evidence"]["commit"] == fixture.commits.get("alpha").unwrap().as_str()
    }));

    let repeated = json(&sb, &["index", "--json"], 0, &fixture);
    assert_eq!(repeated["complete"], true);
    assert_eq!(repeated["run"]["jobs"], 0);
    for name in ["alpha", "beta"] {
        assert_eq!(project(&repeated, name)["sync"]["outcome"], "up_to_date");
        assert_eq!(
            project(&repeated, name)["active_generation"],
            project(&refreshed, name)["active_generation"]
        );
    }
}

#[test]
fn search_and_status_do_not_index_an_unindexed_project() {
    let Some((sb, _db, fixture)) = database_fixture("local retrieval does not autoindex") else {
        return;
    };
    let before = json(
        &sb,
        &["status", "--project", "alpha", "--json"],
        0,
        &fixture,
    );
    assert!(project(&before, "alpha")["active_generation"].is_null());
    let search = json(
        &sb,
        &["search", "AlphaToken", "--project", "alpha", "--json"],
        1,
        &fixture,
    );
    assert!(gap(&search, "project_not_indexed"));
    assert!(search["hits"].as_array().is_none_or(Vec::is_empty));
    let after = json(
        &sb,
        &["status", "--project", "alpha", "--json"],
        0,
        &fixture,
    );
    assert!(project(&after, "alpha")["active_generation"].is_null());
    assert!(project(&after, "alpha")["building_generation"].is_null());
    assert!(project(&after, "alpha")["active_commit"].is_null());
}

#[test]
fn a_missing_ref_reports_failure_without_replacing_the_active_generation() {
    let Some((sb, _db, fixture)) = database_fixture("local missing ref preservation") else {
        return;
    };
    let indexed = json(&sb, &["index", "--json"], 0, &fixture);
    let old = project(&indexed, "alpha");
    git(
        &sb,
        &sb.work().join("alpha"),
        &["branch", "-m", "main", "preserved"],
    );

    let failed = json(&sb, &["index", "--json"], 1, &fixture);
    assert_eq!(failed["complete"], false);
    let alpha = project(&failed, "alpha");
    assert_eq!(alpha["complete"], false);
    assert_eq!(alpha["sync"]["outcome"], "failed");
    assert!(!alpha["incomplete_reasons"].as_array().unwrap().is_empty());
    assert_eq!(alpha["active_generation"], old["active_generation"]);
    assert_eq!(alpha["active_commit"], old["active_commit"]);
    assert_eq!(project(&failed, "beta")["complete"], true);

    let status = json(
        &sb,
        &["status", "--project", "alpha", "--json"],
        0,
        &fixture,
    );
    let alpha = project(&status, "alpha");
    assert_eq!(alpha["target"], "branch:main");
    assert_eq!(alpha["active_generation"], old["active_generation"]);
    assert_eq!(alpha["active_commit"], old["active_commit"]);
    assert!(
        alpha["last_error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
    );

    let search = json(
        &sb,
        &["search", "AlphaToken", "--project", "alpha", "--json"],
        1,
        &fixture,
    );
    assert!(gap(&search, "ref_not_found"));
    let after = json(
        &sb,
        &["status", "--project", "alpha", "--json"],
        0,
        &fixture,
    );
    assert_eq!(
        project(&after, "alpha")["active_generation"],
        old["active_generation"]
    );
    assert_eq!(
        project(&after, "alpha")["active_commit"],
        old["active_commit"]
    );
}

#[test]
fn partial_registration_is_reported_and_is_not_a_successful_full_index() {
    let Some((sb, _db, fixture)) = database_fixture("local partial registration") else {
        return;
    };
    // Metadata registration validates exclusion policies before a view exists;
    // unavailable source paths instead fail later during target resolution.
    write_workspace(&sb, &format!("{WORKSPACE}exclude = ['a[']\n"));
    let partial = json(&sb, &["index", "--json"], 1, &fixture);
    assert_eq!(partial["complete"], false);
    assert_eq!(partial["projects"].as_array().unwrap().len(), 1);
    assert_eq!(project(&partial, "alpha")["complete"], true);
    assert_ready(
        project(&partial, "alpha"),
        fixture.commits.get("alpha").unwrap(),
    );
    assert!(
        partial["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["project"] == "beta")
    );

    let status = json(&sb, &["status", "--project", "beta", "--json"], 0, &fixture);
    assert!(status["projects"].as_array().unwrap().is_empty());
    assert!(
        status["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["project"] == "beta")
    );
    let search = sb.run_with_timeout(
        &["search", "BetaToken", "--project", "beta", "--json"],
        Duration::from_secs(120),
    );
    assert_eq!(search.code, 2);
    assert!(search.stdout.trim().is_empty());
    assert!(search.stderr.contains("could not be registered"));
    assert!(!search.all().contains(ENV_CANARY));
    assert!(!search.all().contains(&fixture.fake_key));

    // A selected healthy project remains queryable without hiding the other issue.
    let alpha = json(
        &sb,
        &["search", "AlphaToken", "--project", "alpha", "--json"],
        0,
        &fixture,
    );
    assert!(!alpha["hits"].as_array().unwrap().is_empty());
    assert!(
        alpha["registration_issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["project"] == "beta")
    );
}

#[test]
fn an_unavailable_source_fails_explicitly_without_blocking_a_healthy_project() {
    let Some((sb, _db, fixture)) = database_fixture("local unavailable source") else {
        return;
    };
    std::fs::rename(sb.work().join("beta"), sb.work().join("unavailable-beta")).unwrap();
    let failed = json(&sb, &["index", "--json"], 1, &fixture);
    assert_eq!(failed["complete"], false);
    assert_eq!(failed["projects"].as_array().unwrap().len(), 2);
    assert!(failed["issues"].as_array().unwrap().is_empty());
    assert_eq!(project(&failed, "alpha")["complete"], true);
    assert_ready(
        project(&failed, "alpha"),
        fixture.commits.get("alpha").unwrap(),
    );
    let beta = project(&failed, "beta");
    assert_eq!(beta["complete"], false);
    assert_eq!(beta["sync"]["outcome"], "failed");
    assert!(beta["active_generation"].is_null());
    assert!(
        beta["last_error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
    );
    assert!(!beta["incomplete_reasons"].as_array().unwrap().is_empty());

    let status = json(&sb, &["status", "--project", "beta", "--json"], 0, &fixture);
    assert_eq!(status["projects"].as_array().unwrap().len(), 1);
    assert!(project(&status, "beta")["active_generation"].is_null());
    assert!(
        project(&status, "beta")["last_error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
    );
    let search = sb.run_with_timeout(
        &["search", "BetaToken", "--project", "beta", "--json"],
        Duration::from_secs(120),
    );
    assert_eq!(search.code, 2);
    assert!(search.stdout.trim().is_empty());
    assert!(search.stderr.contains("internal error"));
    assert!(!search.all().contains(ENV_CANARY));
    assert!(!search.all().contains(&fixture.fake_key));

    let healthy = json(
        &sb,
        &["search", "AlphaToken", "--project", "alpha", "--json"],
        0,
        &fixture,
    );
    assert!(!healthy["hits"].as_array().unwrap().is_empty());
    assert!(
        healthy["hits"]
            .as_array()
            .unwrap()
            .iter()
            .all(|hit| hit["evidence"]["project"] == "alpha")
    );
    assert!(!gap(&healthy, "ref_not_found"));
    let after = json(&sb, &["status", "--project", "beta", "--json"], 0, &fixture);
    assert_eq!(
        project(&after, "beta")["last_error"],
        project(&status, "beta")["last_error"]
    );
    assert!(project(&after, "beta")["active_generation"].is_null());
}
