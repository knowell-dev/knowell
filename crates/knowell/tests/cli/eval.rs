//! The evaluation command CI runs must keep working exactly as before.

use crate::common::Sandbox;

#[test]
fn ci_eval_command_still_passes_against_the_baseline() {
    let sb = Sandbox::new();
    let baseline = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../eval/baselines/synthetic-small.json"
    );
    let markdown = sb.work().join("summary.md");
    let json = sb.work().join("report.json");
    let out = sb.run(&[
        "eval",
        "run",
        "--scale",
        "small",
        "--seed",
        "42",
        "--retriever",
        "grep",
        "--retriever",
        "bm25",
        "--baseline",
        baseline,
        "--markdown",
        markdown.to_str().unwrap(),
        "--json",
        json.to_str().unwrap(),
    ]);
    assert_eq!(out.code, 0, "{out:?}");
    assert!(std::fs::read_to_string(&markdown).unwrap().contains('|'));
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
    assert!(report.is_object());
}

#[test]
fn existing_commands_keep_their_arguments() {
    let sb = Sandbox::new();
    let schema = sb.run(&["config", "schema", "workspace"]);
    assert_eq!(schema.code, 0, "{schema:?}");
    let file = sb.work().join("knowell.toml");
    std::fs::write(
        &file,
        "version = 1\n[workspace]\nname = \"w\"\ntrack = \"branch:main\"\n",
    )
    .unwrap();
    let check = sb.run(&["config", "check", file.to_str().unwrap()]);
    assert_eq!(check.code, 0, "{check:?}");
    let scan = sb.run(&["secrets", "scan", sb.work().to_str().unwrap(), "--json"]);
    assert_eq!(scan.code, 0, "{scan:?}");
}
