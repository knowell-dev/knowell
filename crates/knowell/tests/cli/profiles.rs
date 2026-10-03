//! Fresh-process profile catalogue reads need neither workspace nor provider credentials.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use knowell_core::Name;
use knowell_store::{Store, StoreOptions, hierarchy};
use secrecy::SecretString;
use serde_json::{Value, json};
use sqlx::Connection;
use uuid::Uuid;

use crate::common::{Run, Sandbox, ScratchDb, admin_url, block_on};

const ENGINE: &str = "version = 1\n\
    [database]\nmode = 'external'\nurl = 'env:KNOWELL_CLI_PROFILES_DB_URL'\n\
    [providers.synthetic]\nkind = 'gemini'\nmodel = 'synthetic-unavailable-model'\n\
    api_key = 'env:KNOWELL_CANARY_UNAVAILABLE_PROFILE_KEY'\n";
const WITHOUT_PROVIDER: &str = "version = 1\n\
    [database]\nmode = 'external'\nurl = 'env:KNOWELL_CLI_PROFILES_DB_URL'\n";
const MISSING_PROVIDER: &str = "KNOWELL_CANARY_UNAVAILABLE_PROFILE_KEY";
const CREATED: &str = "2021-02-03T04:05:06.123456Z";
const PROVIDER: &str = "Synthetic [provider] <script>\u{001b}[31m\t";
const MODEL: &str = "Synthetic ![model](https://example.invalid) <script> ```\u{001b}[31m\t";
const INPUT_VERSION: &str = "Synthetic [input] <version> ```\u{001b}[31m\t";
const UUID_NAME: &str = "00000000-0000-0000-0000-00000000cafe";

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Real database diagnostics must never enter failed fixture assertions.
fn prepared<T, E>(result: Result<T, E>, step: &str) -> T {
    match result {
        Ok(value) => value,
        Err(_) => panic!("cannot prepare synthetic profile fixture: {step}"),
    }
}

fn run(sb: &Sandbox, args: &[&str]) -> Run {
    block_on(async {
        let mut command = tokio::process::Command::from(sb.command(args));
        command.env_remove(MISSING_PROVIDER);
        command.stdin(Stdio::null()).kill_on_drop(true);
        let completed = prepared(
            tokio::time::timeout(Duration::from_secs(120), command.output()).await,
            "CLI deadline",
        );
        prepared(completed, "CLI process").into()
    })
}

fn report(sb: &Sandbox, args: &[&str], code: i32) -> Value {
    let output = run(sb, args);
    assert_eq!(output.code, code, "unexpected profile command exit code");
    prepared(
        serde_json::from_str(&output.stdout),
        "one complete JSON report",
    )
}

struct Fixture {
    expected: Vec<Value>,
    foreign: String,
}

fn fixture(test: &str) -> Option<(Sandbox, ScratchDb, Fixture)> {
    let db = ScratchDb::create(&admin_url(test)?);
    let mut sb = Sandbox::new();
    sb.set_env("KNOWELL_CLI_PROFILES_DB_URL", &db.url);
    sb.write_engine(ENGINE);
    // No knowell.toml, repository, source or workspace is created.
    let fixture = block_on(async {
        let store = prepared(
            Store::connect(
                &SecretString::from(db.url.clone()),
                &StoreOptions::default(),
            )
            .await,
            "store connection",
        );
        prepared(store.migrate().await, "migrations");
        let mut conn = prepared(store.acquire().await, "metadata connection");
        let org = prepared(
            hierarchy::create_organization(&mut conn, &Name::new("local").unwrap()).await,
            "local organization",
        );
        let foreign_org = prepared(
            hierarchy::create_organization(&mut conn, &Name::new("synthetic-foreign").unwrap())
                .await,
            "foreign organization",
        );
        prepared(
            hierarchy::create_organization(&mut conn, &Name::new("synthetic-empty").unwrap()).await,
            "empty organization",
        );
        let foreign_id = Uuid::from_u128(0xCA7A_FFFF);
        prepared(sqlx::query(
            "INSERT INTO embedding_profile (id, organization_id, name, provider, model, dimensions, input_format_version)
             VALUES ($1, $2, 'foreign-profile', 'foreign-provider', 'foreign-model', 32, 'synthetic-v1')",
        ).bind(foreign_id).bind(foreign_org.id).execute(&mut *conn).await, "foreign profile");
        let mut expected = Vec::new();
        for (ordinal, label) in [
            (3, "z-profile"),
            (2, "a_profile"),
            (1, "a-profile"),
            (4, UUID_NAME),
        ] {
            let id = Uuid::from_u128(0xCA7A_1000 + ordinal);
            let dimensions = 64 + i32::try_from(ordinal).unwrap();
            let (provider, model, input) = if ordinal == 3 {
                (PROVIDER, MODEL, INPUT_VERSION)
            } else {
                (
                    "removed-synthetic-provider",
                    "synthetic-unavailable-model",
                    "synthetic-input-v7",
                )
            };
            // Store-only metadata also exercises absence of a usable vector
            // index; catalogue reads must not create or repair one.
            prepared(sqlx::query(
                "INSERT INTO embedding_profile
                 (id, organization_id, name, provider, model, dimensions, input_format_version, created_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8::text::timestamptz)",
            ).bind(id).bind(org.id).bind(label).bind(provider).bind(model)
                .bind(dimensions).bind(input).bind(CREATED).execute(&mut *conn).await, "profile metadata");
            expected.push(json!({
                "id": id, "name": label, "provider": provider, "model": model,
                "dimensions": dimensions, "input_format_version": input, "created_at": CREATED,
            }));
        }
        expected.sort_by(|a, b| a["name"].as_str().unwrap().cmp(b["name"].as_str().unwrap()));
        drop(conn);
        store.close().await;
        Fixture {
            expected,
            foreign: foreign_id.to_string(),
        }
    });
    Some((sb, db, fixture))
}

fn snapshot(db: &ScratchDb) -> Value {
    block_on(async {
        let mut conn = prepared(
            sqlx::PgConnection::connect(&db.url).await,
            "snapshot connection",
        );
        let state = prepared(sqlx::query_scalar(
            "SELECT jsonb_build_object(
             'profiles', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM embedding_profile t),
             'jobs', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM job t),
             'sources', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM source t),
             'workspaces', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM workspace t),
             'projects', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM project t),
             'views', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM view t),
             'generations', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM view_generation t),
             'indexes', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY indexname), '[]'::jsonb)
                        FROM pg_indexes t WHERE schemaname = 'public'))",
        ).fetch_one(&mut conn).await, "profile and index snapshot");
        prepared(conn.close().await, "snapshot close");
        state
    })
}

#[test]
fn hostile_profile_selectors_fail_without_echo_or_database_access() {
    let mut sb = Sandbox::new();
    sb.write_engine(ENGINE);
    sb.set_env("KNOWELL_CLI_PROFILES_DB_URL", "");
    let destination = sb.work().join("preserved-profile.out");
    write(&destination, "synthetic existing profile report");
    let oversized = "KNOWELL_CANARY_OVERSIZED_PROFILE".repeat(8);
    for hostile in [
        "KNOWELL_CANARY_BAD_PROFILE",
        "KNOWELL_CANARY_BAD profile",
        "KNOWELL_CANARY_BAD\u{001b}[31m\nprofile",
        oversized.as_str(),
    ] {
        for (selector, diagnostic) in [
            ("name", "profile name"),
            ("id", "profile id"),
            ("organization", "organization name"),
        ] {
            let mut args = match selector {
                "name" => vec!["profile", "show", hostile],
                "id" => vec!["profile", "show", "--id", hostile],
                _ => vec!["profile", "list", "--organization", hostile],
            };
            args.extend(["--json", "--output", destination.to_str().unwrap()]);
            let output = run(&sb, &args);
            assert_eq!(output.code, 2);
            assert!(output.stdout.trim().is_empty());
            assert!(
                output.stderr.contains(diagnostic),
                "selector validation did not precede database access"
            );
            assert!(
                !output.all().contains("KNOWELL_CANARY"),
                "rejected selector was echoed"
            );
            assert!(!output.stderr.contains('\u{001b}'));
            assert_eq!(
                std::fs::read_to_string(&destination).unwrap(),
                "synthetic existing profile report"
            );
        }
    }
    for args in [
        vec!["profile", "show"],
        vec!["profile", "show", "synthetic", "--id", UUID_NAME],
        vec!["profile", "list", "--json", "--format", "markdown"],
    ] {
        let output = run(&sb, &args);
        assert_eq!(output.code, 2);
        assert!(output.stdout.trim().is_empty());
    }
    assert!(
        !sb.knowell_home().join("data").exists(),
        "invalid input opened the indexer"
    );
}

#[test]
fn fresh_processes_read_exact_catalogue_with_no_workspace_or_provider() {
    let Some((sb, db, fixture)) = fixture("persisted CLI profiles") else {
        return;
    };
    let before = snapshot(&db);
    let expected = json!({"organization": "local", "profiles": fixture.expected});
    assert_eq!(report(&sb, &["profile", "list", "--json"], 0), expected);
    assert!(!sb.work().join("knowell.toml").exists());
    for profile in &fixture.expected {
        let name = profile["name"].as_str().unwrap();
        let id = profile["id"].as_str().unwrap();
        let expected = json!({"organization": "local", "profile": profile});
        assert_eq!(
            report(&sb, &["profile", "show", name, "--json"], 0),
            expected
        );
        assert_eq!(
            report(&sb, &["profile", "show", "--id", id, "--json"], 0),
            expected
        );
    }
    // UUID-shaped positional names remain exact names, never inferred IDs.
    let missing = json!({"organization": "local", "profile": null});
    assert_eq!(
        report(&sb, &["profile", "show", "--id", UUID_NAME, "--json"], 1),
        missing
    );
    assert_eq!(
        report(
            &sb,
            &["profile", "show", "--id", &fixture.foreign, "--json"],
            1
        ),
        missing
    );
    assert_eq!(
        report(&sb, &["profile", "show", "missing-profile", "--json"], 1),
        missing
    );
    assert_eq!(
        report(
            &sb,
            &[
                "profile",
                "list",
                "--organization",
                "synthetic-empty",
                "--json"
            ],
            0
        ),
        json!({"organization": "synthetic-empty", "profiles": []})
    );
    // Workspace discovery and an explicit selector must both be irrelevant
    // to this organization-only operation, even when the file is malformed.
    let malformed = sb.work().join("knowell.toml");
    write(&malformed, "synthetic malformed workspace [");
    assert_eq!(
        report(
            &sb,
            &[
                "--workspace",
                malformed.to_str().unwrap(),
                "profile",
                "list",
                "--json"
            ],
            0
        ),
        expected
    );
    let nonexistent = sb.work().join("missing-workspace.toml");
    assert_eq!(
        report(
            &sb,
            &[
                "--workspace",
                nonexistent.to_str().unwrap(),
                "profile",
                "list",
                "--json"
            ],
            0
        ),
        expected
    );
    sb.write_engine(WITHOUT_PROVIDER);
    assert_eq!(report(&sb, &["profile", "list", "--json"], 0), expected);
    assert_eq!(
        snapshot(&db),
        before,
        "catalogue reads changed profiles, sources, jobs or indexes"
    );
}

#[test]
fn profile_human_reports_escape_controls_and_write_complete_atomic_outputs() {
    let Some((sb, db, fixture)) = fixture("CLI profile outputs") else {
        return;
    };
    let before = snapshot(&db);
    let profile = fixture
        .expected
        .iter()
        .find(|profile| profile["name"] == "z-profile")
        .unwrap();
    let id = profile["id"].as_str().unwrap();
    for format in ["text", "markdown"] {
        for subject in ["list", "show"] {
            let mut args = vec!["profile", subject];
            if subject == "show" {
                args.extend(["--id", id]);
            }
            args.extend(["--format", format]);
            let output = run(&sb, &args);
            assert_eq!(output.code, 0);
            assert!(output.stdout.contains(id));
            assert!(output.stdout.contains(CREATED));
            assert!(output.stdout.contains("67"));
            assert!(!output.stdout.chars().any(|c| c.is_control() && c != '\n'));
            if format == "markdown" {
                assert!(
                    output
                        .stdout
                        .contains("\\!\\[model\\]\\(https://example.invalid\\)")
                );
                assert!(output.stdout.contains("\\<script\\>"));
                assert!(output.stdout.contains("\\`\\`\\`"));
                assert!(!output.stdout.contains("![model](https://example.invalid)"));
            }
        }
    }
    for format in ["text", "json", "markdown"] {
        let destination = sb.work().join(format!("profile-{format}.out"));
        write(&destination, "synthetic profile report to replace");
        let args = ["profile", "show", "z-profile", "--format", format];
        let stdout = run(&sb, &args);
        assert_eq!(stdout.code, 0);
        let mut file_args = args.to_vec();
        file_args.extend(["--output", destination.to_str().unwrap()]);
        let output = run(&sb, &file_args);
        assert_eq!(output.code, 0);
        assert!(output.stdout.is_empty());
        let bytes = std::fs::read(&destination).unwrap();
        assert_eq!(
            bytes,
            stdout.stdout.as_bytes(),
            "file report differs from complete stdout report"
        );
        if format == "json" {
            let written: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                written,
                json!({"organization": "local", "profile": profile})
            );
        }
        let directory = sb.work().join(format!("blocked-{format}.out"));
        std::fs::create_dir(&directory).unwrap();
        write(
            &directory.join("preserved-bytes"),
            "synthetic nested report bytes",
        );
        let failure = run(
            &sb,
            &[
                "profile",
                "show",
                "z-profile",
                "--format",
                format,
                "--output",
                directory.to_str().unwrap(),
            ],
        );
        assert_eq!(failure.code, 2);
        assert!(failure.stdout.trim().is_empty());
        assert_eq!(std::fs::read(&destination).unwrap(), bytes);
        assert_eq!(
            std::fs::read_to_string(directory.join("preserved-bytes")).unwrap(),
            "synthetic nested report bytes"
        );
        assert_eq!(
            std::fs::read_dir(&directory).unwrap().count(),
            1,
            "failed output left temporary files in destination"
        );
    }
    let human_missing = run(
        &sb,
        &[
            "profile",
            "show",
            "--id",
            &fixture.foreign,
            "--format",
            "markdown",
        ],
    );
    assert_eq!(human_missing.code, 1);
    assert!(!human_missing.stdout.contains(&fixture.foreign));
    assert!(!human_missing.stdout.contains("foreign-profile"));
    assert_eq!(snapshot(&db), before);
}
