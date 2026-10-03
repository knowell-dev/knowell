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
