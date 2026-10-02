//! `know workspace import` on the generated fixture.

use crate::common::{Sandbox, generate_fixture, git_available};

#[test]
fn import_never_guesses_tracks() {
    if !git_available() {
        eprintln!("skipping workspace::import_never_guesses_tracks: git is not available");
        return;
    }
    let sb = Sandbox::new();
    let dir = sb.work().join("fixture");
    let repos = generate_fixture(&sb, &dir);
    let dir_arg = dir.to_str().unwrap();
    let file = dir.join("knowell.toml");

    // Dry run: the file on stdout, TODOs for the tracks, exit 1, no file.
    let dry = sb.run(&["workspace", "import", dir_arg, "--dry-run"]);
    assert_eq!(dry.code, 1, "{dry:?}");
    assert!(dry.stdout.contains("[[project]]"), "{dry:?}");
    assert!(dry.stdout.contains("TODO"), "{dry:?}");
    knowell_config::parse_workspace(&dry.stdout).unwrap();
    assert!(!file.exists());

    // Not a terminal: the file is written with TODOs and the command says why it exits 1.
    let todo = sb.run(&["workspace", "import", dir_arg]);
    assert_eq!(todo.code, 1, "{todo:?}");
    assert!(todo.stderr.contains("never guesses"), "{todo:?}");
    for repo in &repos {
        assert!(todo.stderr.contains(repo.as_str()), "{repo}: {todo:?}");
    }
    assert!(file.is_file());

    // An existing file is not replaced without --force.
    let refused = sb.run(&["workspace", "import", dir_arg, "--track-current"]);
    assert_eq!(refused.code, 1, "{refused:?}");
    assert!(refused.stderr.contains("--force"), "{refused:?}");

    let tracked = sb.run(&[
        "workspace",
        "import",
        dir_arg,
        "--track-current",
        "--force",
        "--name",
        "demo",
    ]);
    assert_eq!(tracked.code, 0, "{tracked:?}");
    let config = knowell_config::load_workspace(&file).unwrap();
    assert_eq!(config.workspace.name.as_str(), "demo");
    assert_eq!(config.project.len(), repos.len());
    let resolved = config.resolve(&dir).unwrap();
    assert!(
        resolved
            .projects
            .iter()
            .all(|p| p.track.value.to_string() == "branch:main")
    );

    // The same plan again is "up to date".
    let same = sb.run(&[
        "workspace",
        "import",
        dir_arg,
        "--track-current",
        "--name",
        "demo",
    ]);
    assert_eq!(same.code, 0, "{same:?}");
    assert!(same.stdout.contains("up to date"), "{same:?}");
}

#[test]
fn empty_directory_has_nothing_to_import() {
    let sb = Sandbox::new();
    let out = sb.run(&["workspace", "import", sb.work().to_str().unwrap()]);
    assert_eq!(out.code, 1, "{out:?}");
    assert!(out.stderr.contains("no projects"), "{out:?}");
}

#[test]
fn add_and_list_need_an_engine_configuration() {
    let sb = Sandbox::new();
    std::fs::write(
        sb.work().join("knowell.toml"),
        "version = 1\n[workspace]\nname = \"w\"\ntrack = \"branch:main\"\n",
    )
    .unwrap();
    let add = sb.run(&["workspace", "add"]);
    assert_eq!(add.code, 2, "{add:?}");
    assert!(add.stderr.contains("know init"), "{add:?}");
    let list = sb.run(&["workspace", "list"]);
    assert_eq!(list.code, 2, "{list:?}");
}
