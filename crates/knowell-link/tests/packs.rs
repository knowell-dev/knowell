//! Built-in pack tests.
//!
//! - every pack under `packs/` loads, and the embedded built-in set is the
//!   same as the directory;
//! - every pack has at least two positive (`pos-*`) and one negative
//!   (`neg-*`) fixture under `tests/`, each with an `.expected` file;
//! - every fixture produces exactly the extractions (and, for packs with
//!   bindings, the bindings) its `.expected` file lists.
//!
//! An `.expected` file holds one `Extraction::describe()` line (or
//! `bind <id> <name> = <values>` line) per result, in any order; `#` lines
//! are comments, and an optional `# path: <repo path>` header gives the
//! fixture a virtual path (for packs that depend on file locations).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout
)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use knowell_core::{Name, RepoPath};
use knowell_link::{Pack, PackSet, run_pack_on_file};
use pretty_assertions::assert_eq;

fn packs_dir() -> PathBuf {
    manifest_dir().join("../../packs")
}

fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .unwrap_or_else(|| env!("CARGO_MANIFEST_DIR").into())
        .into()
}

fn disk_packs() -> Vec<Pack> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(packs_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.join("pack.toml").exists())
        .collect();
    dirs.sort();
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for dir in dirs {
        match Pack::load_dir(&dir) {
            Ok(pack) => out.push(pack),
            Err(e) => errors.push(e.to_string()),
        }
    }
    assert!(errors.is_empty(), "pack errors:\n{}", errors.join("\n"));
    out
}

fn fixture_path(name: &str, expected: &str) -> String {
    expected
        .lines()
        .find_map(|l| l.strip_prefix("# path:"))
        .map(|p| p.trim().to_owned())
        .unwrap_or_else(|| format!("tests/{name}"))
}

fn actual_lines(
    pack: &Pack,
    support: &Pack,
    name: &str,
    text: &str,
    expected: &str,
) -> Vec<String> {
    let path = RepoPath::new(fixture_path(name, expected)).unwrap();
    let project = Name::new("fixture").unwrap();
    let run = run_pack_on_file(pack, &[support], &project, &path, text);
    let mut lines: Vec<String> = run.extractions.iter().map(|e| e.describe()).collect();
    lines.extend(run.bindings);
    lines.sort();
    lines
}

fn expected_lines(expected: &str) -> Vec<String> {
    let mut lines: Vec<String> = expected
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

/// `(fixture name, source, expectation)` for every fixture of a pack.
fn fixtures(pack: &Pack) -> Vec<(String, String, Option<String>)> {
    let files: BTreeMap<String, String> = pack.fixtures().iter().cloned().collect();
    files
        .iter()
        .filter(|(name, _)| !name.ends_with(".expected"))
        .map(|(name, text)| {
            (
                name.clone(),
                text.clone(),
                files.get(&format!("{name}.expected")).cloned(),
            )
        })
        .collect()
}

#[test]
fn builtin_set_matches_the_packs_directory() {
    let builtin = PackSet::builtin().unwrap_or_else(|e| panic!("{e}"));
    let disk = disk_packs();
    let names = |packs: &[Pack]| -> Vec<String> { packs.iter().map(Pack::id).collect() };
    assert_eq!(names(builtin.packs()), names(&disk));
    for pack in &disk {
        let embedded = builtin.get(pack.name()).unwrap();
        assert_eq!(embedded.entry_ids(), pack.entry_ids(), "{}", pack.name());
        assert_eq!(embedded.limits(), pack.limits(), "{}", pack.name());
        assert!(
            !pack.description().is_empty(),
            "{} needs a description",
            pack.name()
        );
        assert!(
            !pack.limits().trim().is_empty(),
            "{} needs known limits",
            pack.name()
        );
        assert!(
            !pack.languages().is_empty(),
            "{} needs languages",
            pack.name()
        );
    }
    assert!(builtin.packs().len() >= 25);
}

/// `(pack, path)` of every file the built-in set must embed: `pack.toml`
/// and the query files, sorted.
fn disk_pack_files() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for pack in disk_packs() {
        let dir = packs_dir().join(pack.name());
        out.push((pack.name().to_owned(), "pack.toml".to_owned()));
        let mut subdirs: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n != "tests"))
            .collect();
        subdirs.sort();
        for sub in subdirs {
            let mut queries: Vec<String> = std::fs::read_dir(&sub)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".scm"))
                .collect();
            queries.sort();
            let language = sub.file_name().unwrap().to_string_lossy().into_owned();
            for query in queries {
                out.push((pack.name().to_owned(), format!("{language}/{query}")));
            }
        }
    }
    out
}

fn builtin_source() -> PathBuf {
    manifest_dir().join("src/pack/builtin.rs")
}

fn embedded_pack_files() -> Vec<(String, String)> {
    let text = std::fs::read_to_string(builtin_source()).unwrap();
    text.split("include_str!(\"../../../../packs/")
        .skip(1)
        .map(|rest| {
            let path = rest.split('"').next().unwrap();
            let (pack, file) = path.split_once('/').unwrap();
            (pack.to_owned(), file.to_owned())
        })
        .collect()
}

#[test]
fn embedded_file_list_is_current() {
    assert_eq!(
        embedded_pack_files(),
        disk_pack_files(),
        "src/pack/builtin.rs is stale: run `cargo test -p knowell-link --test packs regenerate_builtin -- --ignored`"
    );
}

#[test]
#[ignore = "rewrites src/pack/builtin.rs from the packs/ directory"]
fn regenerate_builtin() {
    let head = [
        "//! The rule packs bundled with the crate (the repository's `packs/`",
        "//! directory), embedded at build time. Fixtures under `packs/*/tests/` are",
        "//! not embedded; the pack tests read them from disk. The test",
        "//! `embedded_file_list_is_current` fails when this list and the directory",
        "//! differ; regenerate it with",
        "//! `cargo test -p knowell-link --test packs regenerate_builtin -- --ignored`.",
        "",
        "use std::collections::BTreeMap;",
        "",
        "/// `(pack, path inside the pack, content)`.",
        "pub(crate) const FILES: &[(&str, &str, &str)] = &[",
    ];
    let tail = [
        "];",
        "",
        "/// Files of every bundled pack, grouped by pack name.",
        "pub(crate) fn pack_files() -> BTreeMap<&'static str, BTreeMap<String, String>> {",
        "    let mut packs: BTreeMap<&'static str, BTreeMap<String, String>> = BTreeMap::new();",
        "    for (pack, path, content) in FILES {",
        "        packs",
        "            .entry(*pack)",
        "            .or_default()",
        "            .insert((*path).to_owned(), (*content).to_owned());",
        "    }",
        "    packs",
        "}",
    ];
    let mut lines: Vec<String> = head.iter().map(|l| (*l).to_owned()).collect();
    for (pack, file) in disk_pack_files() {
        lines.push("    (".to_owned());
        lines.push(format!("        \"{pack}\","));
        lines.push(format!("        \"{file}\","));
        lines.push(format!(
            "        include_str!(\"../../../../packs/{pack}/{file}\"),"
        ));
        lines.push("    ),".to_owned());
    }
    lines.extend(tail.iter().map(|l| (*l).to_owned()));
    let mut out = lines.join("\n");
    out.push('\n');
    std::fs::write(builtin_source(), out).unwrap();
}

#[test]
fn every_pack_has_positive_and_negative_fixtures() {
    for pack in disk_packs() {
        let all = fixtures(&pack);
        let positive = all.iter().filter(|(n, _, _)| n.starts_with("pos-")).count();
        let negative = all.iter().filter(|(n, _, _)| n.starts_with("neg-")).count();
        assert!(
            positive >= 2,
            "{}: {positive} positive fixtures",
            pack.name()
        );
        assert!(
            negative >= 1,
            "{}: {negative} negative fixtures",
            pack.name()
        );
        for (name, _, expected) in &all {
            assert!(
                name.starts_with("pos-") || name.starts_with("neg-"),
                "{}/{name}: fixtures are named pos-* or neg-*",
                pack.name()
            );
            let expected = expected
                .as_deref()
                .unwrap_or_else(|| panic!("{}/{name} has no .expected file", pack.name()));
            let lines = expected_lines(expected);
            if name.starts_with("neg-") {
                assert!(
                    lines.is_empty(),
                    "{}/{name}: negative fixtures expect nothing",
                    pack.name()
                );
            } else {
                assert!(
                    !lines.is_empty(),
                    "{}/{name}: positive fixtures expect something",
                    pack.name()
                );
            }
        }
    }
}

#[test]
fn fixtures_produce_their_expectations() {
    let packs = disk_packs();
    let constants = packs.iter().find(|p| p.name() == "constants").unwrap();
    let mut failures = Vec::new();
    let mut checked = 0;
    for pack in &packs {
        for (name, text, expected) in fixtures(pack) {
            let expected = expected.unwrap_or_default();
            let actual = actual_lines(pack, constants, &name, &text, &expected);
            let wanted = expected_lines(&expected);
            checked += 1;
            if actual != wanted {
                failures.push(format!(
                    "{}/{name}\n  expected:\n    {}\n  actual:\n    {}",
                    pack.name(),
                    wanted.join("\n    "),
                    actual.join("\n    ")
                ));
            }
        }
    }
    assert!(checked >= 75, "only {checked} fixtures");
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn fixture_runs_are_deterministic() {
    let packs = disk_packs();
    let constants = packs.iter().find(|p| p.name() == "constants").unwrap();
    for pack in &packs {
        for (name, text, expected) in fixtures(pack) {
            let expected = expected.unwrap_or_default();
            let path = RepoPath::new(fixture_path(&name, &expected)).unwrap();
            let project = Name::new("fixture").unwrap();
            let a = run_pack_on_file(pack, &[constants], &project, &path, &text);
            let b = run_pack_on_file(pack, &[constants], &project, &path, &text);
            assert_eq!(a, b, "{}/{name}", pack.name());
        }
    }
}

#[test]
#[ignore = "developer aid: prints actual fixture results"]
fn print_pack_fixtures() {
    let packs = disk_packs();
    let constants = packs.iter().find(|p| p.name() == "constants").unwrap();
    for pack in &packs {
        for (name, text, expected) in fixtures(pack) {
            let expected = expected.unwrap_or_default();
            println!("== {}/{name}", pack.name());
            for line in actual_lines(pack, constants, &name, &text, &expected) {
                println!("{line}");
            }
        }
    }
}
