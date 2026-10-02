//! `know ci init`.

use crate::common::Sandbox;

#[test]
fn github_workflow_is_written_once_and_never_clobbered() {
    let sb = Sandbox::new();
    let repo = sb.work().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let dir = repo.to_str().unwrap();
    let workflow = repo.join(".github/workflows/knowell.yml");

    let dry = sb.run(&["ci", "init", "github", "--dir", dir, "--dry-run"]);
    assert_eq!(dry.code, 0, "{dry:?}");
    assert!(dry.stdout.contains("knowell-dev/knowell-action"), "{dry:?}");
    assert!(dry.stdout.contains("nothing was written"));
    assert!(!workflow.exists());

    let first = sb.run(&["ci", "init", "github", "--dir", dir]);
    assert_eq!(first.code, 0, "{first:?}");
    assert!(first.stdout.contains("created"), "{first:?}");
    let content = std::fs::read_to_string(&workflow).unwrap();
    assert!(content.contains("pull_request"));

    let again = sb.run(&["ci", "init", "github", "--dir", dir]);
    assert_eq!(again.code, 0, "{again:?}");
    assert!(again.stdout.contains("unchanged"), "{again:?}");

    std::fs::write(&workflow, "# my own edits\n").unwrap();
    let refused = sb.run(&["ci", "init", "github", "--dir", dir]);
    assert_eq!(refused.code, 1, "{refused:?}");
    assert!(refused.stderr.contains("--force"), "{refused:?}");
    assert_eq!(
        std::fs::read_to_string(&workflow).unwrap(),
        "# my own edits\n"
    );

    let forced = sb.run(&["ci", "init", "github", "--dir", dir, "--force"]);
    assert_eq!(forced.code, 0, "{forced:?}");
    assert_eq!(std::fs::read_to_string(&workflow).unwrap(), content);
}

#[test]
fn modes_validate_their_options() {
    let sb = Sandbox::new();
    let dir = sb.work();
    let dir = dir.to_str().unwrap();
    let missing = sb.run(&[
        "ci",
        "init",
        "gitlab",
        "--mode",
        "index-update",
        "--dir",
        dir,
    ]);
    assert_eq!(missing.code, 2, "{missing:?}");
    assert!(missing.stderr.contains("hub_url"), "{missing:?}");

    let ok = sb.run(&[
        "ci",
        "init",
        "gitea",
        "--mode",
        "index-update",
        "--hub-url",
        "https://knowell.example.com",
        "--branch",
        "main",
        "--dir",
        dir,
    ]);
    assert_eq!(ok.code, 0, "{ok:?}");
    assert!(ok.stdout.contains("KNOWELL_HUB_TOKEN"), "{ok:?}");
    assert!(sb.work().join(".gitea/workflows/knowell.yml").is_file());
}
