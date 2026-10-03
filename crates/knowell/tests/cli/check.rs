//! SARIF source locations from the real CLI, including monorepo sub-roots.

use std::path::Path;
use std::process::Command;

use serde_json::Value;
use url::Url;

use crate::common::Sandbox;

fn repository(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    let mut command = Command::new("git");
    // A caller running in a git hook must not redirect this fixture's writes.
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
        .args(["init", "--quiet", "--initial-branch=main", "--template="])
        .arg(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .output()
        .unwrap();
    assert!(output.status.success(), "git init failed");
}

fn client(path: &Path, endpoint: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        format!("// synthetic caller\n\nexport const load = () => fetch(\"{endpoint}\");\n"),
    )
    .unwrap();
}

fn report(sb: &Sandbox, config: &Path, cwd: &Path) -> Value {
    let output = sb.run_in(
        cwd,
        &[
            "--workspace",
            config.to_str().unwrap(),
            "check",
            "--format",
            "sarif",
            "--fail-on",
            "never",
        ],
    );
    assert_eq!(output.code, 0, "{output:?}");
    serde_json::from_str(&output.stdout).unwrap()
}

fn assert_locations(report: &Value, repo: &Path, expected: &[(&str, &str)]) {
    let run = &report["runs"][0];
    let results: Vec<&Value> = run["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|result| result["ruleId"] == "link.endpoint_without_provider")
        .collect();
    assert_eq!(results.len(), expected.len(), "{results:?}");
    let repo = std::fs::canonicalize(repo).unwrap();
    for (project, path) in expected {
        let result = results
            .iter()
            .find(|result| result["properties"]["project"] == *project)
            .unwrap();
        let physical = &result["locations"][0]["physicalLocation"];
        let artifact = &physical["artifactLocation"];
        assert!(artifact.get("uriBaseId").is_none());
        let base = run["originalUriBaseIds"][project]["uri"].as_str().unwrap();
        let uri = artifact["uri"].as_str().unwrap();
        // GitHub maps an absolute URI against its checkout without using URI base IDs.
        let resolved = Url::parse(uri).unwrap();
        assert_eq!(resolved.scheme(), "file");
        assert_eq!(resolved.query(), None);
        assert_eq!(resolved.fragment(), None);
        let file = std::fs::canonicalize(resolved.to_file_path().unwrap()).unwrap();
        let project_root = Url::parse(base).unwrap().to_file_path().unwrap();
        assert!(file.starts_with(std::fs::canonicalize(project_root).unwrap()));
        assert_eq!(file, std::fs::canonicalize(repo.join(path)).unwrap());
        assert_eq!(file.strip_prefix(&repo).unwrap(), Path::new(path));
        assert_eq!(physical["region"]["startLine"], 3);
        assert_eq!(physical["region"]["endLine"], 3);
        assert!(
            std::fs::read_to_string(file)
                .unwrap()
                .lines()
                .nth(2)
                .unwrap()
                .contains("fetch")
        );
    }
}

#[test]
fn sarif_single_repo_resolves_encoded_paths_from_a_nested_working_directory() {
    let sb = Sandbox::new();
    let repo = sb.work().join("checkout with spaces");
    repository(&repo);
    let path = "src/café #100%.ts";
    client(&repo.join(path), "/v1/missing");
    let config = repo.join("knowell.toml");
    std::fs::write(
        &config,
        "version = 1\n[workspace]\nname = \"demo\"\ntrack = \"worktree\"\n\
         [[project]]\nname = \"frontend\"\npath = \".\"\n",
    )
    .unwrap();
    let report = report(&sb, &config, &repo.join("src"));
    assert_locations(&report, &repo, &[("frontend", path)]);
    assert!(report.to_string().contains("caf%C3%A9%20%23100%25.ts"));
}

#[test]
fn sarif_monorepo_distinguishes_matching_filenames_in_project_subroots() {
    let sb = Sandbox::new();
    let repo = sb.work().join("monorepo");
    repository(&repo);
    client(&repo.join("apps/web/src/client.ts"), "/v1/web-missing");
    client(&repo.join("services/api/src/client.ts"), "/v1/api-missing");
    std::fs::create_dir(repo.join("config")).unwrap();
    let config = repo.join("config/knowell.toml");
    std::fs::write(
        &config,
        "version = 1\n[workspace]\nname = \"demo\"\ntrack = \"worktree\"\n\
         [[project]]\nname = \"frontend\"\npath = \"..\"\nroot = \"apps/web\"\n\
         [[project]]\nname = \"backend\"\npath = \"..\"\nroot = \"services/api\"\n",
    )
    .unwrap();
    let report = report(&sb, &config, &repo.join("apps/web"));
    assert_locations(
        &report,
        &repo,
        &[
            ("frontend", "apps/web/src/client.ts"),
            ("backend", "services/api/src/client.ts"),
        ],
    );
}

fn fixture_git(repo: &Path, args: &[&str]) -> String {
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
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=Synthetic Check",
            "-c",
            "user.email=synthetic@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "gc.auto=0",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .output()
        .unwrap();
    assert!(output.status.success(), "synthetic git operation failed");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn remove_loose_fixture_blob(repo: &Path, id: &str) {
    assert_eq!(id.len(), 40);
    assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let (directory, filename) = id.split_at(2);
    std::fs::remove_file(repo.join(".git/objects").join(directory).join(filename)).unwrap();
}

#[test]
fn diff_checks_filter_project_scope_before_reading_rename_candidates() {
    let sb = Sandbox::new();
    let repo = sb.work().join("scoped-diff");
    repository(&repo);
    let config = repo.join("knowell.toml");
    std::fs::write(
        &config,
        "version = 1\n[workspace]\nname = 'synthetic'\ntrack = 'worktree'\n\
         [[project]]\nname = 'frontend'\npath = '.'\nroot = 'apps/web'\nexclude = ['private/**']\n",
    )
    .unwrap();
    client(&repo.join("apps/web/changed.ts"), "/v1/old");
    client(&repo.join("apps/web/unchanged.ts"), "/v1/unchanged");
    let denied = [
        ("apps/web/.env.old", "apps/web/.env.new"),
        ("apps/web/private/old.ts", "apps/web/private/new.ts"),
        ("sibling/old.ts", "sibling/new.ts"),
    ];
    for (index, (old, _)) in denied.iter().enumerate() {
        let path = repo.join(old);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!("// synthetic forbidden old candidate {index}\n"),
        )
        .unwrap();
    }
    fixture_git(&repo, &["add", "--all"]);
    fixture_git(
        &repo,
        &["commit", "--quiet", "-m", "initial synthetic diff"],
    );
    let base = fixture_git(&repo, &["rev-parse", "HEAD"]);
    let mut inaccessible = Vec::new();
    for (old, new) in denied {
        inaccessible.push(fixture_git(&repo, &["rev-parse", &format!("HEAD:{old}")]));
        std::fs::remove_file(repo.join(old)).unwrap();
        std::fs::write(
            repo.join(new),
            format!("// distinct synthetic forbidden new candidate at {new}\n"),
        )
        .unwrap();
    }
    client(&repo.join("apps/web/changed.ts"), "/v1/after");
    fixture_git(&repo, &["add", "--all"]);
    fixture_git(
        &repo,
        &["commit", "--quiet", "-m", "advance synthetic diff"],
    );
    let head = fixture_git(&repo, &["rev-parse", "HEAD"]);
    let index = std::fs::read(repo.join(".git/index")).unwrap();
    for (_, new) in denied {
        inaccessible.push(fixture_git(&repo, &["rev-parse", &format!("HEAD:{new}")]));
    }
    for id in &inaccessible {
        remove_loose_fixture_blob(&repo, id);
    }
    let output = sb.run_in(
        &repo,
        &[
            "--workspace",
            config.to_str().unwrap(),
            "check",
            "--diff-base",
            &base,
            "--format",
            "sarif",
            "--fail-on",
            "never",
        ],
    );
    assert_eq!(output.code, 0, "{output:?}");
    let report: Value = serde_json::from_str(&output.stdout).unwrap();
    let results = report["runs"][0]["results"].as_array().unwrap();
    assert_eq!(
        results.len(),
        1,
        "a failed diff must not report every finding"
    );
    assert_eq!(results[0]["ruleId"], "link.endpoint_without_provider");
    let location = &results[0]["locations"][0]["physicalLocation"];
    assert_eq!(location["region"]["startLine"], 3);
    assert_eq!(location["region"]["endLine"], 3);
    let file = Url::parse(location["artifactLocation"]["uri"].as_str().unwrap())
        .unwrap()
        .to_file_path()
        .unwrap();
    assert_eq!(
        std::fs::canonicalize(file).unwrap(),
        std::fs::canonicalize(repo.join("apps/web/changed.ts")).unwrap()
    );
    assert!(
        results[0]["message"]["text"]
            .as_str()
            .unwrap()
            .contains("/v1/after")
    );
    assert!(!output.stderr.contains("reporting all findings"));
    assert_eq!(fixture_git(&repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(std::fs::read(repo.join(".git/index")).unwrap(), index);
}

#[test]
fn corrupt_allowed_diff_blob_is_an_operational_error_without_a_report_all_fallback() {
    let sb = Sandbox::new();
    let repo = sb.work().join("corrupt-allowed-diff");
    repository(&repo);
    let config = repo.join("knowell.toml");
    std::fs::write(
        &config,
        "version = 1\n[workspace]\nname = 'synthetic'\ntrack = 'worktree'\n\
         [[project]]\nname = 'frontend'\npath = '.'\nroot = 'apps/web'\n",
    )
    .unwrap();
    client(&repo.join("apps/web/removed.ts"), "/v1/removed");
    client(&repo.join("apps/web/unchanged.ts"), "/v1/unchanged");
    fixture_git(&repo, &["add", "--all"]);
    fixture_git(
        &repo,
        &["commit", "--quiet", "-m", "initial synthetic allowed diff"],
    );
    let base = fixture_git(&repo, &["rev-parse", "HEAD"]);
    let old_blob = fixture_git(&repo, &["rev-parse", "HEAD:apps/web/removed.ts"]);
    std::fs::remove_file(repo.join("apps/web/removed.ts")).unwrap();
    client(&repo.join("apps/web/added.ts"), "/v1/added");
    fixture_git(&repo, &["add", "--all"]);
    fixture_git(
        &repo,
        &["commit", "--quiet", "-m", "advance synthetic allowed diff"],
    );
    let new_blob = fixture_git(&repo, &["rev-parse", "HEAD:apps/web/added.ts"]);
    assert_ne!(
        old_blob, new_blob,
        "the rename candidates must require blob comparison"
    );

    // A fallback would return both findings, including the unchanged caller.
    let full_report = report(&sb, &config, &repo);
    let results = full_report["runs"][0]["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    for endpoint in ["/v1/added", "/v1/unchanged"] {
        assert!(results.iter().any(|finding| {
            finding["ruleId"] == "link.endpoint_without_provider"
                && finding["message"]["text"]
                    .as_str()
                    .is_some_and(|message| message.contains(endpoint))
        }));
    }

    let head = fixture_git(&repo, &["rev-parse", "HEAD"]);
    let index = std::fs::read(repo.join(".git/index")).unwrap();
    assert_eq!(old_blob.len(), 40);
    assert!(old_blob.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let (directory, filename) = old_blob.split_at(2);
    let blob_path = repo.join(".git/objects").join(directory).join(filename);
    // Git may mark loose objects read-only; only this synthetic object is altered.
    let mut permissions = std::fs::metadata(&blob_path).unwrap().permissions();
    // On Windows this clears an attribute; Unix retains its existing mode below.
    #[cfg(windows)]
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(permissions.mode() | 0o200);
    }
    std::fs::set_permissions(&blob_path, permissions).unwrap();
    // Only the removed allowed blob is corrupt; current source extraction succeeds.
    std::fs::write(&blob_path, b"not a zlib stream").unwrap();
    let output_path = repo.join("failed-diff.sarif");
    for write_file in [false, true] {
        let mut args = vec![
            "--workspace",
            config.to_str().unwrap(),
            "check",
            "--diff-base",
            &base,
            "--format",
            "sarif",
            "--fail-on",
            "never",
        ];
        if write_file {
            args.extend(["--output", output_path.to_str().unwrap()]);
        }
        let output = sb.run_in(&repo, &args);
        assert_eq!(
            output.code, 2,
            "an allowed corrupt blob is an operational failure"
        );
        assert!(
            output.stdout.trim().is_empty(),
            "a failed diff must not emit a report"
        );
        assert!(
            output
                .stderr
                .contains("cannot compare selected files in project `frontend`")
        );
        assert!(!output.stderr.contains("reporting all findings"));
        assert!(
            !output_path.exists(),
            "a failed diff must not create a report file"
        );
    }
    assert_eq!(fixture_git(&repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(std::fs::read(repo.join(".git/index")).unwrap(), index);
    assert_eq!(std::fs::read(blob_path).unwrap(), b"not a zlib stream");
}

#[test]
fn invalid_diff_base_is_rejected_without_echoing_hostile_input() {
    let sb = Sandbox::new();
    let repo = sb.work().join("hostile-diff-base");
    repository(&repo);
    client(&repo.join("src/client.ts"), "/v1/missing");
    let config = repo.join("knowell.toml");
    std::fs::write(
        &config,
        "version = 1\n[workspace]\nname = 'synthetic'\ntrack = 'worktree'\n\
         [[project]]\nname = 'frontend'\npath = '.'\n",
    )
    .unwrap();
    let hostile = "KNOWELL_CANARY_diff_base_\u{001b}[31m\nsynthetic";
    let output = sb.run_in(
        &repo,
        &[
            "--workspace",
            config.to_str().unwrap(),
            "check",
            "--diff-base",
            hostile,
            "--format",
            "sarif",
            "--fail-on",
            "never",
        ],
    );
    assert_eq!(
        output.code, 2,
        "an invalid diff base is an operational failure"
    );
    assert!(output.stdout.trim().is_empty());
    assert!(
        output
            .stderr
            .contains("--diff-base must name a full commit id")
    );
    assert!(!output.all().contains("KNOWELL_CANARY_diff_base_"));
    assert!(!output.all().contains('\u{001b}'));
    assert!(!output.stderr.contains("reporting all findings"));
}
