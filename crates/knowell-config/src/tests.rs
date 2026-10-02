//! Cross-cutting tests: examples, loading from disk and secret safety.

use std::path::Path;

use crate::*;

const FAKE_KEY: &str = "AIzaSyFAKE0000000000000000000000000000";

fn rendered(err: &ConfigError) -> String {
    format!("{err} | {err:?}")
}

#[test]
fn shipped_examples_parse_resolve_and_cross_check() {
    let engine = parse_engine(include_str!("../examples/engine.toml")).unwrap();
    let ws = parse_workspace(include_str!("../examples/knowell.toml")).unwrap();
    let base = std::env::current_dir().unwrap();
    let resolved = ws.resolve(&base).unwrap();
    assert!(resolved.projects.len() >= 6);
    let issues = resolved.check_against(&engine);
    assert!(issues.is_empty(), "{issues:?}");
}

#[test]
fn example_exercises_overrides() {
    let ws = parse_workspace(include_str!("../examples/knowell.toml")).unwrap();
    let r = ws.resolve(&std::env::current_dir().unwrap()).unwrap();
    assert!(r.projects.iter().any(|p| p.track.origin == Origin::Project));
    assert!(
        r.projects
            .iter()
            .any(|p| p.track.origin == Origin::Workspace)
    );
    assert!(
        r.projects
            .iter()
            .any(|p| p.data_policy.origin == Origin::Project)
    );
    assert!(r.projects.iter().any(|p| p.root.is_some()));
}

#[test]
fn load_from_disk_names_the_file_in_errors() {
    let dir = tempfile::tempdir().unwrap();
    let ok = dir.path().join("engine.toml");
    std::fs::write(&ok, "version = 1\n").unwrap();
    assert!(load_engine(&ok).is_ok());

    let bad = dir.path().join("knowell.toml");
    std::fs::write(&bad, "version = 1\n[workspace]\nname = \"a\"\nbogus = 1\n").unwrap();
    let err = load_workspace(&bad).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("knowell.toml:4:1"), "{text}");

    let missing = dir.path().join("nope.toml");
    assert!(matches!(
        load_workspace(&missing),
        Err(ConfigError::Read { .. })
    ));
}

#[test]
fn literal_key_in_secret_field_is_never_echoed() {
    let engine =
        format!("version = 1\n[providers.gemini]\nkind = \"gemini\"\napi_key = \"{FAKE_KEY}\"\n");
    let err = parse_engine(&engine).unwrap_err();
    assert!(matches!(err, ConfigError::Parse { line: Some(4), .. }));
    assert!(!rendered(&err).contains(FAKE_KEY), "{}", rendered(&err));
    assert!(rendered(&err).contains("secret reference"));

    let hub = format!(
        "version = 1\n[server]\nrole = \"edge\"\n[hub]\nurl = \"https://h.example\"\ntoken = \"{FAKE_KEY}\"\n"
    );
    let err = parse_engine(&hub).unwrap_err();
    assert!(!rendered(&err).contains(FAKE_KEY));

    let db = format!("version = 1\n[database]\nmode = \"external\"\nurl = \"{FAKE_KEY}\"\n");
    let err = parse_engine(&db).unwrap_err();
    assert!(!rendered(&err).contains(FAKE_KEY));
}

#[test]
fn key_pasted_into_other_fields_is_not_echoed() {
    let cases = [
        format!("version = 1\n[server]\nrole = \"{FAKE_KEY}\"\n"),
        format!("version = 1\n[server]\nlisten = \"{FAKE_KEY}\"\n"),
        format!("version = \"{FAKE_KEY}\"\n"),
        format!("version = 1\n[telemetry]\nenabled = \"{FAKE_KEY}\"\n"),
        format!("version = 1\n[providers.g]\nkind = \"{FAKE_KEY}\"\n"),
        format!("version = 1\n[database]\nmode = \"{FAKE_KEY}\"\n"),
    ];
    for text in cases {
        let err = parse_engine(&text).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{text}");
        assert!(!rendered(&err).contains(FAKE_KEY), "{}", rendered(&err));
    }
}

#[test]
fn key_pasted_into_workspace_fields_is_not_echoed() {
    let head = "version = 1\n[workspace]\nname = \"shop\"\n";
    let cases = [
        format!("{head}track = \"{FAKE_KEY}\"\n"),
        format!("{head}data_policy = \"{FAKE_KEY}\"\n"),
        format!("{head}[workspace.embedding]\npreset = \"{FAKE_KEY}\"\n"),
        format!("{head}[[project]]\nname = \"a\"\npath = \"a\"\nroot = \"{FAKE_KEY}/../x\"\n"),
        format!("version = 1\n[workspace]\nname = \"{FAKE_KEY}\"\n"),
    ];
    for text in cases {
        let err = parse_workspace(&text).unwrap_err();
        assert!(!rendered(&err).contains(FAKE_KEY), "{}", rendered(&err));
    }
}

#[test]
fn validation_issues_do_not_echo_urls() {
    let text = format!(
        "version = 1\n[providers.p]\nkind = \"openai-compatible\"\nbase_url = \"ftp://user:{FAKE_KEY}@host\"\n"
    );
    let err = parse_engine(&text).unwrap_err();
    assert!(matches!(err, ConfigError::Invalid { .. }));
    assert!(!rendered(&err).contains(FAKE_KEY));

    let hub = format!(
        "version = 1\n[server]\nrole = \"edge\"\n[hub]\nurl = \"{FAKE_KEY}\"\ntoken = \"env:T\"\n"
    );
    let err = parse_engine(&hub).unwrap_err();
    assert!(!rendered(&err).contains(FAKE_KEY));
}

#[test]
fn debug_of_loaded_config_shows_references_only() {
    let cfg = parse_engine(
        "version = 1\n[providers.g]\nkind = \"gemini\"\napi_key = \"env:GEMINI_API_KEY\"\n",
    )
    .unwrap();
    let debug = format!("{cfg:?}");
    assert!(debug.contains("env:GEMINI_API_KEY"));
}

#[test]
fn hostile_input_never_panics() {
    let long_brackets = "[".repeat(5000);
    let long_lines = "a = 1\n".repeat(2000);
    let inputs = [
        "",
        "\u{0}",
        "version",
        "version = 1\n[",
        "version = 1\n[[workspace]]",
        "version = 1\n[workspace]\nname = \"\u{e9}\u{e9}\u{e9}\"",
        "= = =",
        long_brackets.as_str(),
        long_lines.as_str(),
    ];
    for text in inputs {
        let _ = parse_engine(text);
        let _ = parse_workspace(text);
    }
    let _ = load_engine(Path::new(""));
}
