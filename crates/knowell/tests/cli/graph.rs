//! Standalone graph commands query saved, evidenced indexes without scheduling work.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::Value;
use sqlx::Connection;

use crate::common::{Sandbox, ScratchDb, admin_url, block_on, git_available};

const ENGINE: &str = "version = 1\n\
    [database]\nmode = 'external'\nurl = 'env:KNOWELL_CLI_GRAPH_DB_URL'\n";
const WORKSPACE: &str = "version = 1\n\
    [workspace]\nname = 'synthetic-graph'\ntrack = 'branch:main'\n\
    [[project]]\nname = 'app'\npath = 'repo'\nroot = 'packages/app'\nexclude = ['src/excluded-*.ts']\n\
    [[project]]\nname = 'client'\npath = 'client'\n";
const SCOPE: &str = "export function ScopeProbe(value: number): number {\n\
    const first = value + 1;\n\
    const second = first * 2;\n\
    return second;\n\
    }\n";
const CONTRACT: &str = "GET /v1/items[item]";
const CANARY: &str = "KNOWELL_CANARY_GRAPH_EXCLUDED_6d53";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn workspace(sb: &Sandbox, text: &str) {
    write(&sb.work().join("knowell.toml"), text);
}

fn invalid(sb: &Sandbox, args: &[&str], diagnostic: &str) {
    let output = sb.run_with_timeout(args, Duration::from_secs(15));
    assert_eq!(
        output.code, 2,
        "invalid graph request must fail before DB access"
    );
    assert!(
        output.stdout.trim().is_empty(),
        "invalid request produced a report"
    );
    assert!(
        !output.stderr.trim().is_empty(),
        "invalid request omitted its diagnostic"
    );
    assert!(
        output.stderr.contains(diagnostic),
        "missing expected graph diagnostic"
    );
    assert!(
        !sb.knowell_home().join("data").exists(),
        "invalid request opened a local index"
    );
}

#[test]
fn graph_arguments_and_unknown_projects_fail_before_database_access() {
    let mut sb = Sandbox::new();
    sb.write_engine(ENGINE);
    sb.set_env("KNOWELL_CLI_GRAPH_DB_URL", "");
    workspace(&sb, WORKSPACE);
    for args in [
        vec!["trace", "ScopeProbe", "--limit", "0", "--json"],
        vec!["trace", "ScopeProbe", "--limit", "201", "--json"],
        vec!["trace", "ScopeProbe", "--max-depth", "6", "--json"],
        vec!["trace", "ScopeProbe", "--contract", CONTRACT, "--json"],
        vec!["trace", "--id", "KNOWELL_FAKE INVALID_RESULT_ID", "--json"],
        vec![
            "trace",
            "ScopeProbe",
            "--relation",
            "unknown_relation",
            "--json",
        ],
        vec!["trace", "ScopeProbe", "--format", "markdown", "--json"],
        vec!["trace", "   ", "--json"],
        vec!["impact", "ScopeProbe", "--limit", "0", "--json"],
        vec!["impact", "ScopeProbe", "--limit", "201", "--json"],
        vec!["impact", "ScopeProbe", "--max-depth", "0", "--json"],
        vec![
            "impact",
            "ScopeProbe",
            "--file",
            "src/scope.ts",
            "--project",
            "app",
            "--json",
        ],
        vec!["impact", "--file", "src/scope.ts", "--json"],
        vec!["impact", "--base", "branch:main", "--json"],
        vec![
            "impact",
            "--base",
            "branch:main",
            "--head",
            "branch:main",
            "--project",
            "app",
            "--json",
        ],
        vec![
            "impact",
            "--file",
            "../scope.ts",
            "--project",
            "app",
            "--json",
        ],
        vec!["impact", "ScopeProbe", "--head", "branch:main", "--json"],
        vec!["impact", "--id", "KNOWELL_FAKE INVALID_RESULT_ID", "--json"],
        vec!["impact", "ScopeProbe", "--format", "markdown", "--json"],
    ] {
        invalid(&sb, &args, "");
    }
    for args in [
        vec!["trace", "ScopeProbe", "--project", "missing", "--json"],
        vec!["impact", "ScopeProbe", "--project", "missing", "--json"],
        vec![
            "impact",
            "--file",
            "src/scope.ts",
            "--project",
            "missing",
            "--json",
        ],
        vec![
            "impact",
            "--base",
            "branch:main",
            "--project",
            "missing",
            "--json",
        ],
    ] {
        invalid(&sb, &args, "does not exist in workspace");
    }
}

#[test]
fn graph_remote_transport_is_refused_before_configuration_or_credentials() {
    let sb = Sandbox::new();
    for command in ["trace", "impact"] {
        for transport in ["--hub", "--hub-url", "--oidc-audience"] {
            invalid(
                &sb,
                &[
                    command,
                    "ScopeProbe",
                    transport,
                    "KNOWELL_FAKE_UNSUPPORTED_TRANSPORT",
                    "--json",
                ],
                "not supported",
            );
        }
    }
}

/// Fixture Git has no inherited hooks, configuration, repository or credentials.
fn git(sb: &Sandbox, repository: &Path, args: &[&str]) -> String {
    let config = sb.home().join("empty-graph-git-config");
    write(&config, "");
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
        "synthetic graph Git command failed"
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn commit(sb: &Sandbox, repository: &Path) -> String {
    git(sb, repository, &["add", "--all"]);
    git(
        sb,
        repository,
        &[
            "commit",
            "--quiet",
            "--no-verify",
            "-m",
            "synthetic graph fixture",
        ],
    );
    git(sb, repository, &["rev-parse", "HEAD"])
}

struct Fixture {
    repository: PathBuf,
    app_commit: String,
    client_commit: String,
}

fn database_fixture(test: &str) -> Option<(Sandbox, ScratchDb, Fixture)> {
    let admin = admin_url(test)?;
    if !git_available() {
        eprintln!("skipping {test}: git is unavailable");
        return None;
    }
    let db = ScratchDb::create(&admin);
    let mut sb = Sandbox::new();
    sb.write_engine(ENGINE);
    sb.set_env("KNOWELL_CLI_GRAPH_DB_URL", &db.url);
    workspace(&sb, WORKSPACE);
    let repository = sb.work().join("repo");
    write(&repository.join("packages/app/src/scope.ts"), SCOPE);
    for client in ["a", "b", "c"] {
        write(
            &repository.join(format!("packages/app/src/client-{client}.ts")),
            &format!(
                "import {{ ScopeProbe }} from './scope';\nexport function client_{client}() {{ return ScopeProbe(3); }}\n"
            ),
        );
    }
    write(
        &repository.join("packages/app/src/scope.test.ts"),
        "import assert from 'node:assert/strict';\nimport test from 'node:test';\nimport { ScopeProbe } from './scope';\ntest('scope probe', () => assert.equal(ScopeProbe(3), 8));\n",
    );
    write(
        &repository.join("packages/app/src/[mark].ts"),
        "export function MarkedProbe() { return 7; }\n",
    );
    write(
        &repository.join("packages/app/src/http.ts"),
        "import express from 'express';\nconst app = express();\nexport function ItemsHandler() { return []; }\napp.get('/v1/items[item]', ItemsHandler);\n",
    );
    for path in [
        "outside/old.ts",
        "packages/application/old.ts",
        "packages/app/src/excluded-old.ts",
    ] {
        write(
            &repository.join(path),
            &format!("// {CANARY}: {path}\nexport function ExcludedProbe() {{ return 3; }}\n"),
        );
    }
    write(
        &repository.join("packages/app/.env"),
        &format!("TOKEN={CANARY}\n"),
    );
    let client = sb.work().join("client");
    write(
        &client.join("src/http.ts"),
        "import axios from 'axios';\nexport async function fetchItems() {\n  return axios.get('/v1/items[item]');\n}\n",
    );
    for path in [&repository, &client] {
        git(
            &sb,
            path,
            &[
                "init",
                "--quiet",
                "--initial-branch=main",
                "--object-format=sha1",
                "--template=",
            ],
        );
    }
    let app_commit = commit(&sb, &repository);
    let client_commit = commit(&sb, &client);
    Some((
        sb,
        db,
        Fixture {
            repository,
            app_commit,
            client_commit,
        },
    ))
}

fn json(sb: &Sandbox, args: &[&str], expected_code: i32) -> Value {
    let output = sb.run_with_timeout(args, Duration::from_secs(120));
    assert!(
        !output.all().contains(CANARY),
        "excluded source content reached a graph report"
    );
    // Captured operational errors can involve a real database; never print them.
    assert_eq!(
        output.code, expected_code,
        "unexpected graph command exit code for synthetic request {args:?}"
    );
    serde_json::from_str(&output.stdout).expect("graph stdout must contain exactly one JSON report")
}

fn gap(report: &Value, reason: &str) -> bool {
    report["gaps"]
        .as_array()
        .is_some_and(|gaps| gaps.iter().any(|gap| gap["reason"] == reason))
}

fn array<'a>(report: &'a Value, field: &str) -> &'a [Value] {
    report[field].as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn index(sb: &Sandbox) {
    let report = json(sb, &["index", "--json"], 0);
    assert_eq!(
        report["complete"], true,
        "synthetic graph index did not complete"
    );
}

/// Compare complete job rows and index data, allowing registration metadata to refresh.
fn snapshot(db: &ScratchDb) -> Value {
    block_on(async {
        let Ok(mut connection) = sqlx::PgConnection::connect(&db.url).await else {
            panic!("cannot connect to synthetic graph database");
        };
        let result = sqlx::query_scalar::<_, Value>(
            "SELECT jsonb_build_object(\
              'jobs', (SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM job t),\
              'generations', (SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM view_generation t),\
              'files', (SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM file_version t),\
              'content', (SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM content t),\
              'chunks', (SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM chunk t),\
              'symbols', (SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM symbol t),\
              'occurrences', (SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM occurrence t),\
              'edges', (SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM edge t),\
              'contracts', (SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM contract t),\
              'views', (SELECT COALESCE(jsonb_agg(jsonb_build_array(id, last_generation, active_generation, active_commit) ORDER BY id), '[]'::jsonb) FROM view))",
        ).fetch_one(&mut connection).await;
        let Ok(result) = result else {
            panic!("cannot snapshot synthetic graph jobs and index");
        };
        assert!(
            connection.close().await.is_ok(),
            "cannot close synthetic graph database"
        );
        result
    })
}

fn assert_evidence(evidence: &Value, fixture: &Fixture, stale: bool) {
    let expected = match evidence["project"].as_str() {
        Some("app") => &fixture.app_commit,
        Some("client") => &fixture.client_commit,
        _ => panic!("graph evidence named an unexpected synthetic project"),
    };
    assert_eq!(evidence["commit"].as_str(), Some(expected.as_str()));
    assert_eq!(evidence["view"], "branch:main");
    assert!(
        evidence["path"]
            .as_str()
            .is_some_and(|path| path.starts_with("src/"))
    );
    let start = evidence["lines"]["start"].as_u64().unwrap();
    let end = evidence["lines"]["end"].as_u64().unwrap();
    assert!(
        start >= 1 && end >= start && end <= 5,
        "graph source lines are not real fixture lines"
    );
    assert_eq!(evidence["content_hash"].as_str().unwrap().len(), 64);
    assert_eq!(
        evidence["index_state"],
        if stale && evidence["project"] == "app" {
            "stale"
        } else {
            "current"
        }
    );
}

#[test]
fn graph_trace_has_real_contract_and_symbol_evidence_with_global_limits() {
    let Some((sb, db, fixture)) =
        database_fixture("graph_trace_has_real_contract_and_symbol_evidence_with_global_limits")
    else {
        return;
    };
    index(&sb);
    let before = snapshot(&db);
    let args = [
        "trace",
        "ScopeProbe",
        "--project",
        "app",
        "--direction",
        "upstream",
        "--max-depth",
        "3",
        "--json",
    ];
    let full = json(&sb, &args, 0);
    assert_eq!(full["workspace"], "synthetic-graph");
    assert!(array(&full, "registration_issues").is_empty());
    assert!(array(&full, "nodes").len() >= 5 && array(&full, "edges").len() >= 3);
    assert_eq!(full["truncated"], false);
    assert!(!gap(&full, "relations_not_ready"));
    assert!(
        array(&full, "nodes")
            .iter()
            .any(|node| node["label"] == "ScopeProbe")
    );
    assert!(
        array(&full, "nodes")
            .iter()
            .any(|node| node["label"] == "src/scope.ts")
    );
    for node in array(&full, "nodes") {
        assert_evidence(&node["evidence"], &fixture, false);
    }
    let start_id = array(&full, "nodes")
        .iter()
        .find(|node| node["label"] == "ScopeProbe")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let by_id = json(
        &sb,
        &[
            "trace",
            "--id",
            start_id,
            "--direction",
            "upstream",
            "--json",
        ],
        0,
    );
    assert!(
        array(&by_id, "nodes")
            .iter()
            .any(|node| node["label"] == "ScopeProbe")
    );
    for limit in ["1", "2", "3"] {
        let args = [
            "trace",
            "ScopeProbe",
            "--project",
            "app",
            "--direction",
            "upstream",
            "--max-depth",
            "3",
            "--limit",
            limit,
            "--json",
        ];
        let clipped = json(&sb, &args, 0);
        let nodes = array(&clipped, "nodes");
        assert!(nodes.len() <= limit.parse::<usize>().unwrap());
        assert_eq!(nodes.first(), array(&full, "nodes").first());
        assert_eq!(clipped["truncated"], true);
        assert!(gap(&clipped, "limit_reached"));
        let ids: BTreeSet<_> = nodes
            .iter()
            .map(|node| node["node"].as_str().unwrap())
            .collect();
        assert!(
            array(&clipped, "edges")
                .iter()
                .all(|edge| ids.contains(edge["from"].as_str().unwrap())
                    && ids.contains(edge["to"].as_str().unwrap()))
        );
        assert_eq!(
            clipped,
            json(&sb, &args, 0),
            "limited graph reads must be deterministic"
        );
    }
    let contract = json(
        &sb,
        &[
            "trace",
            "--contract",
            CONTRACT,
            "--direction",
            "both",
            "--json",
        ],
        0,
    );
    assert!(
        array(&contract, "nodes")
            .iter()
            .any(|node| node["kind"] == "endpoint" && node["label"] == CONTRACT)
    );
    let mut source_projects = BTreeSet::new();
    for evidence in array(&contract, "nodes")
        .iter()
        .filter_map(|node| node.get("evidence"))
        .chain(
            array(&contract, "edges")
                .iter()
                .flat_map(|edge| array(edge, "evidence")),
        )
    {
        assert_evidence(evidence, &fixture, false);
        source_projects.insert(evidence["project"].as_str().unwrap());
    }
    assert_eq!(
        source_projects,
        BTreeSet::from(["app", "client"]),
        "contract trace must cite both endpoints' sources"
    );
    assert!(
        array(&contract, "edges")
            .iter()
            .any(|edge| edge["relation"] == "http_call")
    );
    assert!(
        array(&contract, "edges")
            .iter()
            .any(|edge| edge["relation"] == "http_route")
    );
    let markdown = sb.run_with_timeout(
        &[
            "trace",
            "--contract",
            CONTRACT,
            "--direction",
            "both",
            "--format",
            "markdown",
        ],
        Duration::from_secs(120),
    );
    assert_eq!(markdown.code, 0, "contract Markdown report failed");
    assert!(markdown.stdout.contains("## Knowell trace") && markdown.stdout.contains("### Edges"));
    assert!(markdown.stdout.contains("GET /v1/items\\[item\\]"));
    assert!(
        markdown.stdout.contains("app:src/http.ts:")
            && markdown.stdout.contains("client:src/http.ts:")
    );
    assert!(
        !markdown.stdout.contains(CONTRACT),
        "Markdown contract label was not escaped"
    );
    assert!(
        before == snapshot(&db),
        "trace queries changed jobs or saved index data"
    );
}

#[test]
fn graph_impact_scopes_diff_and_preserves_saved_versions_without_indexing() {
    let Some((sb, db, fixture)) =
        database_fixture("graph_impact_scopes_diff_and_preserves_saved_versions_without_indexing")
    else {
        return;
    };
    index(&sb);
    let before = snapshot(&db);
    // Different selectors may intentionally resolve to the same saved commit.
    let unchanged = json(
        &sb,
        &[
            "impact",
            "--base",
            &fixture.app_commit,
            "--head",
            "branch:main",
            "--project",
            "app",
            "--json",
        ],
        0,
    );
    assert!(gap(&unchanged, "not_found"));
    for field in ["changed", "impacted", "tests"] {
        assert!(
            array(&unchanged, field).is_empty(),
            "same-commit diff invented changed evidence"
        );
    }
    write(
        &fixture.repository.join("packages/app/src/scope.ts"),
        &SCOPE.replace("value + 1", "value + 2"),
    );
    let mut hidden = Vec::new();
    for (number, old) in [
        "outside/old.ts",
        "packages/application/old.ts",
        "packages/app/src/excluded-old.ts",
    ]
    .into_iter()
    .enumerate()
    {
        std::fs::remove_file(fixture.repository.join(old)).unwrap();
        let new = old.replace("old.ts", "new.ts");
        write(
            &fixture.repository.join(&new),
            &format!("// {CANARY}: {number}\nexport function ExcludedProbe() {{ return 4; }}\n"),
        );
        hidden.push(new);
    }
    write(
        &fixture.repository.join("packages/app/.env"),
        &format!("TOKEN={CANARY}_advanced\n"),
    );
    let head = commit(&sb, &fixture.repository);
    // Tree metadata remains present. Only forbidden blob reads observe these removals.
    for (number, path) in hidden.iter().enumerate() {
        let object = git(
            &sb,
            &fixture.repository,
            &["rev-parse", &format!("{head}:{path}")],
        );
        assert_eq!(object.len(), 40);
        assert!(
            object
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        );
        std::fs::rename(
            fixture
                .repository
                .join(".git/objects")
                .join(&object[..2])
                .join(&object[2..]),
            sb.home().join(format!("hidden-graph-{number}.blob")),
        )
        .unwrap();
    }
    let report = json(
        &sb,
        &[
            "impact",
            "--base",
            &fixture.app_commit,
            "--head",
            &head,
            "--project",
            "app",
            "--json",
        ],
        0,
    );
    let changed = array(&report, "changed");
    assert!(changed.iter().any(|item| item["name"] == "ScopeProbe"));
    assert!(
        changed
            .iter()
            .any(|item| item["kind"] == "file" && item["evidence"]["path"] == "src/scope.ts")
    );
    assert!(
        array(&report, "impacted")
            .iter()
            .any(|item| item["evidence"]["path"]
                .as_str()
                .is_some_and(|path| path.starts_with("src/client-"))),
        "impact must contain imported dependents"
    );
    assert!(
        array(&report, "tests")
            .iter()
            .any(|item| item["kind"] == "test" && item["evidence"]["path"] == "src/scope.test.ts"),
        "impact must cite the synthetic test dependency"
    );
    for item in changed
        .iter()
        .chain(array(&report, "impacted"))
        .chain(array(&report, "tests"))
    {
        assert_evidence(&item["evidence"], &fixture, true);
    }
    assert!(
        report["risk"].is_object(),
        "evidenced impact must assess risk"
    );
    let without_tests = json(
        &sb,
        &[
            "impact",
            "--base",
            &fixture.app_commit,
            "--head",
            &head,
            "--project",
            "app",
            "--no-tests",
            "--json",
        ],
        0,
    );
    assert!(
        array(&without_tests, "tests").is_empty(),
        "--no-tests returned suggested tests"
    );
    assert_eq!(
        without_tests["risk"], report["risk"],
        "hiding suggested tests changed assessed risk"
    );
    let mut expected_without_tests = report.clone();
    expected_without_tests
        .as_object_mut()
        .unwrap()
        .remove("tests");
    assert_eq!(
        without_tests, expected_without_tests,
        "--no-tests changed evidence, gaps or truncation beyond omitting tests"
    );
    let serialized = serde_json::to_string(&report).unwrap();
    for forbidden in [CANARY, "outside/", "application/", "excluded-", ".env"] {
        assert!(
            !serialized.contains(forbidden),
            "impact included a forbidden source path or value"
        );
    }
    let alias = json(
        &sb,
        &[
            "impact",
            "--diff-base",
            &fixture.app_commit,
            "--head",
            &head,
            "--project",
            "app",
            "--json",
        ],
        0,
    );
    assert_eq!(report, alias, "diff alias changed impact semantics");
    let file = json(
        &sb,
        &[
            "impact",
            "--file",
            "src/[mark].ts",
            "--project",
            "app",
            "--json",
        ],
        0,
    );
    assert!(
        array(&file, "changed")
            .iter()
            .any(|item| item["evidence"]["path"] == "src/[mark].ts")
    );
    let markdown = sb.run_with_timeout(
        &[
            "impact",
            "--file",
            "src/[mark].ts",
            "--project",
            "app",
            "--format",
            "markdown",
        ],
        Duration::from_secs(120),
    );
    assert_eq!(markdown.code, 0, "file impact Markdown report failed");
    assert!(
        markdown.stdout.contains("## Knowell impact")
            && markdown.stdout.contains("src/\\[mark\\].ts")
    );
    assert!(
        markdown.stdout.contains(&fixture.app_commit),
        "Markdown omitted the cited saved version"
    );
    assert!(
        !markdown.stdout.contains("src/[mark].ts"),
        "Markdown path was not escaped"
    );
    assert!(
        before == snapshot(&db),
        "impact queries scheduled work or changed saved index data"
    );
}

#[test]
fn graph_missing_indexes_subjects_and_refs_are_explicit_and_read_only() {
    let Some((sb, db, fixture)) =
        database_fixture("graph_missing_indexes_subjects_and_refs_are_explicit_and_read_only")
    else {
        return;
    };
    // Status describes the saved state successfully even before indexing;
    // graph retrieval below must independently report the unavailable data.
    let status = json(&sb, &["status", "--json"], 0);
    assert_eq!(array(&status, "projects").len(), 2);
    assert!(array(&status, "projects").iter().all(|project| {
        project.get("active_generation") == Some(&Value::Null)
            && project.get("building_generation") == Some(&Value::Null)
            && project.get("active_commit") == Some(&Value::Null)
    }));
    let empty = snapshot(&db);
    for args in [
        vec!["trace", "ScopeProbe", "--project", "app", "--json"],
        vec!["impact", "ScopeProbe", "--project", "app", "--json"],
        vec![
            "impact",
            "--file",
            "src/scope.ts",
            "--project",
            "app",
            "--json",
        ],
    ] {
        let report = json(&sb, &args, 1);
        assert!(gap(&report, "project_not_indexed"));
        for field in ["nodes", "edges", "changed", "impacted", "tests"] {
            assert!(array(&report, field).is_empty());
        }
        assert!(
            report.get("risk").is_none(),
            "missing index must not imply an assessed risk"
        );
    }
    assert!(
        empty == snapshot(&db),
        "missing-index graph query started indexing"
    );
    index(&sb);
    let indexed = snapshot(&db);
    for args in [
        vec!["trace", "UnknownGraphProbe", "--project", "app", "--json"],
        vec!["impact", "UnknownGraphProbe", "--project", "app", "--json"],
        vec![
            "impact",
            "--file",
            "src/not-there.ts",
            "--project",
            "app",
            "--json",
        ],
    ] {
        let report = json(&sb, &args, 1);
        assert!(gap(&report, "not_found"));
        assert!(array(&report, "nodes").is_empty() && array(&report, "changed").is_empty());
    }
    for args in [
        vec![
            "impact",
            "--base",
            "branch:missing",
            "--head",
            &fixture.app_commit,
            "--project",
            "app",
            "--json",
        ],
        vec![
            "impact",
            "--base",
            &fixture.app_commit,
            "--head",
            "branch:missing",
            "--project",
            "app",
            "--json",
        ],
    ] {
        let report = json(&sb, &args, 1);
        assert!(gap(&report, "ref_not_found"));
        assert!(array(&report, "changed").is_empty() && array(&report, "impacted").is_empty());
        assert!(
            report.get("risk").is_none(),
            "missing ref must not imply an assessed risk"
        );
    }
    assert!(
        indexed == snapshot(&db),
        "missing subject/ref graph query changed jobs or index data"
    );
}
