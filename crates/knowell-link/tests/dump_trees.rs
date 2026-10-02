//! Developer aid (ignored by default): prints the tree-sitter trees of every
//! rule-pack fixture so query authors can see node kinds and field names.
//!
//! `cargo test -p knowell-link --test dump_trees -- --ignored --nocapture`

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout
)]

use std::path::Path;

use knowell_core::RepoPath;
use knowell_parse::tree_sitter::Node;
use knowell_parse::{Language, ParseLimits, parse_tree};

fn print_node(node: Node<'_>, field: Option<&str>, text: &str, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    let label = field.map(|f| format!("{f}: ")).unwrap_or_default();
    let mut line = format!(
        "{indent}{label}{}{} [{}]",
        if node.is_named() { "" } else { "'" },
        node.kind(),
        node.start_position().row + 1
    );
    if node.child_count() == 0 {
        let snippet: String = text
            .get(node.byte_range())
            .unwrap_or_default()
            .chars()
            .take(60)
            .collect();
        line.push_str(&format!(" {snippet:?}"));
    }
    out.push_str(&line);
    out.push('\n');
    let mut cursor = node.walk();
    for (index, child) in node.children(&mut cursor).enumerate() {
        if !child.is_named() && child.child_count() == 0 && child.kind().len() <= 2 {
            continue;
        }
        let name = node.field_name_for_child(u32::try_from(index).unwrap());
        print_node(child, name, text, depth + 1, out);
    }
}

fn dump(label: &str, language: Language, text: &str) {
    let mut out = format!("===== {label} ({language})\n");
    match parse_tree(language, text, &ParseLimits::default()) {
        Some(tree) => print_node(tree.root_node(), None, text, 0, &mut out),
        None => out.push_str("<no tree>\n"),
    }
    println!("{out}");
}

#[test]
#[ignore = "developer aid"]
fn dump_pack_fixtures() {
    let manifest =
        std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_else(|| env!("CARGO_MANIFEST_DIR").into());
    let packs = Path::new(&manifest).join("../../packs");
    let filter = std::env::var("DUMP_FILTER").unwrap_or_default();
    let mut files = Vec::new();
    for pack in std::fs::read_dir(&packs).unwrap() {
        let tests = pack.unwrap().path().join("tests");
        if let Ok(entries) = std::fs::read_dir(&tests) {
            for entry in entries {
                files.push(entry.unwrap().path());
            }
        }
    }
    files.sort();
    for file in files {
        let display = file.display().to_string().replace('\\', "/");
        if !display.contains(&filter) || display.ends_with(".expected") {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        let name = file.file_name().unwrap().to_str().unwrap().to_owned();
        let language = if name.ends_with(".arb") {
            Language::Json
        } else {
            Language::detect(&RepoPath::new(name.clone()).unwrap(), &text)
        };
        dump(&display, language, &text);
    }
}

#[test]
#[ignore = "developer aid"]
fn dump_fixture_core_files() {
    let fixture = knowell_eval::generate(&knowell_eval::FixtureSpec {
        seed: 42,
        scale: knowell_eval::Scale::Small,
    });
    let filter = std::env::var("DUMP_FILTER").unwrap_or_default();
    for project in fixture.projects() {
        for file in &project.files {
            if file.role != knowell_eval::FileRole::Core {
                continue;
            }
            let label = format!("{}/{}", project.name, file.path);
            if filter.is_empty() || !label.contains(&filter) {
                continue;
            }
            let language = Language::detect(&file.path, &file.content);
            dump(&label, language, &file.content);
        }
    }
}
