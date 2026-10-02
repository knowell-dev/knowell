//! `know project add`.

use crate::common::Sandbox;

const WORKSPACE: &str =
    "version = 1\n# team workspace\n[workspace]\nname = \"w\"\ntrack = \"branch:main\"\n";

#[test]
fn adds_validates_and_refuses_duplicates() {
    let sb = Sandbox::new();
    let file = sb.work().join("knowell.toml");
    std::fs::write(&file, WORKSPACE).unwrap();
    std::fs::create_dir_all(sb.work().join("services/payments")).unwrap();

    let out = sb.run(&["project", "add", "services/payments"]);
    assert_eq!(out.code, 0, "{out:?}");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(
        text.starts_with(WORKSPACE),
        "existing text must stay: {text}"
    );
    assert!(text.contains("name = \"payments\""), "{text}");
    assert!(text.contains("path = \"services/payments\""), "{text}");
    let config = knowell_config::load_workspace(&file).unwrap();
    config.resolve(&sb.work()).unwrap();

    let dup = sb.run(&["project", "add", "services/payments"]);
    assert_eq!(dup.code, 1, "{dup:?}");
    assert!(dup.stderr.contains("already exists"), "{dup:?}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), text);

    // From another directory, with an explicit workspace file and options.
    std::fs::create_dir_all(sb.work().join("mono/packages/web")).unwrap();
    let elsewhere = sb.root.path();
    let out = sb.run_in(
        elsewhere,
        &[
            "--workspace",
            file.to_str().unwrap(),
            "project",
            "add",
            sb.work().join("mono").to_str().unwrap(),
            "--root",
            "packages/web",
            "--name",
            "web",
            "--track",
            "tag:v1.2.0",
        ],
    );
    assert_eq!(out.code, 0, "{out:?}");
    let config = knowell_config::load_workspace(&file).unwrap();
    let web = config
        .project
        .iter()
        .find(|p| p.name.as_str() == "web")
        .unwrap();
    assert_eq!(web.path, std::path::Path::new("mono"));
    assert_eq!(web.root.as_ref().unwrap().as_str(), "packages/web");
    assert_eq!(web.track.as_ref().unwrap().to_string(), "tag:v1.2.0");
}

#[test]
fn bad_input_changes_nothing() {
    let sb = Sandbox::new();
    let file = sb.work().join("knowell.toml");
    std::fs::write(&file, WORKSPACE).unwrap();

    let missing = sb.run(&["project", "add", "does-not-exist"]);
    assert_eq!(missing.code, 1, "{missing:?}");
    let bad_track = sb.run(&["project", "add", ".", "--track", "main"]);
    assert_eq!(bad_track.code, 2, "{bad_track:?}");
    let bad_root = sb.run(&["project", "add", ".", "--root", "../escape"]);
    assert_eq!(bad_root.code, 2, "{bad_root:?}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), WORKSPACE);

    let no_workspace = Sandbox::new();
    let out = no_workspace.run(&["project", "add", "."]);
    assert_eq!(out.code, 2, "{out:?}");
    assert!(out.stderr.contains("workspace import"), "{out:?}");
}
