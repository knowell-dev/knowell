//! The evaluation command CI runs must keep working exactly as before.

use crate::common::Sandbox;

#[test]
fn ci_eval_command_still_passes_against_the_baseline() {
    let sb = Sandbox::new();
    let manifest =
        std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_else(|| env!("CARGO_MANIFEST_DIR").into());
    let baseline =
        std::path::PathBuf::from(manifest).join("../../eval/baselines/synthetic-small.json");
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
        baseline.to_str().unwrap(),
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

#[test]
fn hybrid_requires_an_explicit_secret_reference_without_echoing_values() {
    let sb = Sandbox::new();
    let missing = sb.run(&["eval", "run", "--retriever", "hybrid"]);
    assert_eq!(missing.code, 2, "{missing:?}");
    assert!(missing.stderr.contains("--database-url"));
    let canary = "postgres://eval:KNOWELL_CANARY_eval_pw@localhost/eval";
    let pasted = sb.run(&[
        "eval",
        "run",
        "--retriever",
        "hybrid",
        "--database-url",
        canary,
    ]);
    assert_eq!(pasted.code, 2, "{pasted:?}");
    assert!(!pasted.all().contains("KNOWELL_CANARY_eval_pw"));
    assert!(pasted.stderr.contains("reference"));
}

#[test]
fn hybrid_cli_measures_all_retrievers_reproducibly() {
    let Some(admin) = crate::common::admin_url("eval::hybrid") else {
        return;
    };
    let mut sb = Sandbox::new();
    sb.set_env("KNOWELL_EVAL_TEST_DB", &admin);
    let report_path = sb.work().join("hybrid.json");
    let run = || {
        sb.run(&[
            "eval",
            "run",
            "--retriever",
            "grep",
            "--retriever",
            "bm25",
            "--retriever",
            "hybrid",
            "--database-url",
            "env:KNOWELL_EVAL_TEST_DB",
            "--json",
            report_path.to_str().unwrap(),
        ])
    };
    let first = run();
    assert_eq!(first.code, 0, "{first:?}");
    let text = std::fs::read_to_string(&report_path).unwrap();
    let report = knowell_eval::Report::from_json(&text).unwrap();
    assert_eq!(report.retrievers.len(), 3);
    assert!(
        report
            .retriever("hybrid")
            .unwrap()
            .overall
            .recall_at_10
            .unwrap()
            > 0.0
    );
    let manifest =
        std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_else(|| env!("CARGO_MANIFEST_DIR").into());
    let baseline_path =
        std::path::PathBuf::from(manifest).join("../../eval/baselines/synthetic-small-hybrid.json");
    let baseline =
        knowell_eval::Report::from_json(&std::fs::read_to_string(baseline_path).unwrap()).unwrap();
    let comparison = knowell_eval::compare(&report, &baseline, 1e-4).unwrap();
    assert!(
        !comparison.has_regressions(),
        "{}",
        comparison.to_markdown()
    );
    let second = run();
    assert_eq!(second.code, 0, "{second:?}");
    assert_eq!(text, std::fs::read_to_string(&report_path).unwrap());
}

#[test]
fn hybrid_refuses_a_server_without_pgvector() {
    let Ok(admin) = std::env::var("KNOWELL_TEST_PLAIN_DATABASE_URL") else {
        eprintln!("skipping eval::hybrid_plain: KNOWELL_TEST_PLAIN_DATABASE_URL is not set");
        return;
    };
    let mut sb = Sandbox::new();
    sb.set_env("KNOWELL_EVAL_TEST_DB", &admin);
    let result = sb.run(&[
        "eval",
        "run",
        "--retriever",
        "hybrid",
        "--database-url",
        "env:KNOWELL_EVAL_TEST_DB",
    ]);
    assert_eq!(result.code, 2, "{result:?}");
    assert!(result.stderr.contains("requires pgvector"));
}

#[test]
fn live_eval_rejects_invalid_requests_before_resolving_secrets() {
    let sb = Sandbox::new();
    for (key, dimensions, budget, expected) in [
        (
            "KNOWELL_CANARY_pasted_live_key",
            "768",
            "500000",
            "reference",
        ),
        ("env:KNOWELL_MISSING_LIVE_KEY", "64", "500000", "dimensions"),
        ("env:KNOWELL_MISSING_LIVE_KEY", "768", "0", "budget"),
        ("env:KNOWELL_MISSING_LIVE_KEY", "768", "500001", "budget"),
    ] {
        let out = sb.run(&[
            "eval",
            "live",
            "--database-url",
            "env:KNOWELL_MISSING_EVAL_DB",
            "--api-key-ref",
            key,
            "--dimensions",
            dimensions,
            "--max-tokens",
            budget,
            "--json",
            "never-written.json",
        ]);
        assert_eq!(out.code, 2, "{out:?}");
        assert!(out.stderr.contains(expected), "{out:?}");
        assert!(!out.all().contains("KNOWELL_CANARY_pasted_live_key"));
        assert!(!sb.work().join("never-written.json").exists());
    }
}
