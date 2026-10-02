//! Hostile, malformed and edge-case input: truncation, garbage, minified
//! bundles, deep nesting, limits, cancellation, encodings, determinism.

mod common;

use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use common::*;
use knowell_parse::{
    ChunkContext, ChunkKind, ChunkOptions, Degradation, Language, ParseError, ParseLimits,
    SymbolKind as K, chunks, parse, parse_with, prepared_input, skeleton,
};
use pretty_assertions::assert_eq;

/// Small but varied fixtures, one per grammar family.
const FIXTURES: &[(&str, &str)] = &[
    (
        "src/a.rs",
        "/// Doc.\npub struct A { x: u32 }\nimpl A {\n    pub fn get(&self) -> u32 { self.x }\n}\n#[cfg(test)]\nmod tests { #[test] fn t() {} }\n",
    ),
    (
        "src/a.ts",
        "import { b } from './b';\n/** Doc. */\nexport class A {\n  private x = 1;\n  get(): number { return this.x; }\n}\nexport const f = (y: number) => y * 2;\n",
    ),
    (
        "src/a.py",
        "import os\n\nclass A:\n    \"\"\"Doc.\"\"\"\n    def get(self) -> int:\n        return 1\n\ndef f(y):\n    return y * 2\n",
    ),
    (
        "a.go",
        "package a\n\nimport \"fmt\"\n\n// A does.\ntype A struct{ X int }\n\nfunc (a *A) Get() int { return a.X }\n",
    ),
    (
        "A.java",
        "import java.util.List;\n/** Doc. */\npublic class A {\n  private int x;\n  public int get() { return x; }\n}\n",
    ),
    (
        "A.kt",
        "import a.b.C\nclass A(val x: Int) {\n  fun get(): Int = x\n}\nfun f(y: Int) = y * 2\n",
    ),
    (
        "A.cs",
        "using System;\nnamespace N {\n  public class A {\n    public int Get() { return 1; }\n  }\n}\n",
    ),
    (
        "a.rb",
        "require 'x'\nmodule M\n  class A\n    def get\n      1\n    end\n  end\nend\n",
    ),
    (
        "a.cpp",
        "#include <x>\nnamespace n {\nclass A {\n public:\n  int get();\n};\nint A::get() { return 1; }\n}\n",
    ),
    (
        "a.sql",
        "CREATE TABLE a (id INT PRIMARY KEY, name TEXT);\nALTER TABLE a ADD COLUMN b TEXT;\nINSERT INTO a VALUES (1, 'x');\n",
    ),
    (
        "a.proto",
        "syntax = \"proto3\";\nservice S {\n  rpc Get(Req) returns (Res);\n}\nmessage Req { string id = 1; }\n",
    ),
    (
        "openapi.yaml",
        "openapi: 3.0.0\npaths:\n  /a:\n    get:\n      operationId: getA\n",
    ),
    (
        "openapi.json",
        "{\"openapi\": \"3.0.0\", \"paths\": {\"/a\": {\"get\": {\"operationId\": \"getA\"}}}}\n",
    ),
    (
        "a.toml",
        "[package]\nname = \"a\"\n\n[dependencies]\nx = \"1\"\n",
    ),
    ("a.md", "# A\n\ntext\n\n## B\n\n```\n# code\n```\n"),
    (
        "Dockerfile",
        "FROM a AS b\nRUN x \\\n  y\nFROM c\nCOPY --from=b /x /y\n",
    ),
    (
        "a.dart",
        "import 'x.dart';\nclass A {\n  int get() => 1;\n}\n",
    ),
    (
        "a.swift",
        "import X\nstruct A {\n  func get() -> Int { 1 }\n}\n",
    ),
    (
        "a.php",
        "<?php\nnamespace N;\nclass A {\n  public function get() { return 1; }\n}\n",
    ),
    ("a.scala", "import x.Y\nclass A {\n  def get: Int = 1\n}\n"),
    (
        "a.c",
        "#include <x.h>\nstruct s { int x; };\nint f(void) { return 0; }\n",
    ),
    ("a.sh", "source x.sh\nf() {\n  echo hi\n}\n"),
    (
        "a.css",
        ".a { color: red; }\n@media (x) { .b { color: blue; } }\n",
    ),
];

/// Every prefix (on character boundaries, in steps) of every fixture parses,
/// chunks and renders without panicking and with consistent output.
#[test]
fn truncated_input_never_panics() {
    let options = ChunkOptions {
        target_chars: 256,
        min_chars: 40,
        overlap_chars: 40,
    };
    for (path, text) in FIXTURES {
        let mut cut = 0;
        while cut <= text.len() {
            if text.is_char_boundary(cut) {
                let prefix = &text[..cut];
                let file = parsed(path, prefix);
                let list = chunked(&file, prefix, &options);
                let _ = skeleton_of(&file, prefix);
                for chunk in &list {
                    let context = ChunkContext::for_chunk("p", &file, chunk);
                    let _ = prepared_input(chunk, &context);
                }
                for symbol in &file.symbols {
                    assert!(prefix.get(symbol.byte_range.clone()).is_some());
                    assert!(symbol.range.end() <= file.line_count.max(1));
                }
            }
            cut += 7;
        }
    }
}

#[test]
fn syntax_errors_keep_earlier_symbols() {
    let text = "/// Doc.\npub struct A { x: u32 }\n\npub fn ok() {}\n\nimpl A {\n    pub fn broken(&self) -> u32 {\n        let y = (1 + \n";
    let file = parsed("src/broken.rs", text);
    assert!(file.has_errors);
    assert_eq!(file.degraded, None);
    expect_symbols(&file, &[(K::Struct, "A", 1, 2), (K::Function, "ok", 4, 4)]);
    let list = chunked(&file, text, &ChunkOptions::default());
    // Nothing is lost: the unparsable tail is still chunked.
    assert_covers(text, &list);
}

#[test]
fn garbage_input() {
    let garbage =
        "}}}{{{ class ( ) => => export export function function <<<>>> ;;; \0 é中 \u{feff}\n"
            .repeat(200);
    for path in [
        "g.ts",
        "g.py",
        "g.rs",
        "g.java",
        "g.json",
        "g.yaml",
        "g.sql",
        "g.md",
        "g.proto",
        "Dockerfile",
    ] {
        let file = parsed(path, &garbage);
        let list = chunked(&file, &garbage, &ChunkOptions::default());
        assert!(!list.is_empty(), "{path}");
        let _ = skeleton_of(&file, &garbage);
    }
}

#[test]
fn byte_order_mark_and_nul() {
    let text = "\u{feff}fn a() {}\n\0\nfn b() {}\n";
    let file = parsed("src/bom.rs", text);
    let names: Vec<&str> = file.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"a"), "{names:?}");
    chunked(&file, text, &ChunkOptions::default());
}

#[test]
fn huge_single_line_minified_bundle_is_bounded() {
    let bundle: String = (0..45_000)
        .map(|i| format!("var a{i}=function(b){{return b*{i}}};"))
        .collect();
    assert!(bundle.len() > 1_500_000 && !bundle.contains('\n'));
    let started = Instant::now();
    let file = parsed("dist/app.js", &bundle);
    assert_eq!(file.degraded, Some(Degradation::Minified));
    assert!(file.symbols.is_empty());
    assert_eq!(file.line_count, 1);
    let options = ChunkOptions::default();
    let list = chunked(&file, &bundle, &options);
    assert!(
        list.iter()
            .all(|c| c.kind == ChunkKind::Text && c.text.len() <= 4000)
    );
    assert!(
        list.len() <= bundle.len() / 2000 + 1,
        "{} chunks",
        list.len()
    );
    assert!(
        list.iter()
            .all(|c| c.range.start() == 1 && c.range.end() == 1)
    );
    // The pieces tile the line: only whitespace (trimmed at break points)
    // lies between consecutive pieces.
    assert_eq!(list.first().unwrap().byte_range.start, 0);
    assert_eq!(list.last().unwrap().byte_range.end, bundle.len());
    for pair in list.windows(2) {
        let gap = &bundle[pair[0].byte_range.end..pair[1].byte_range.start];
        assert!(gap.trim().is_empty(), "gap {gap:?}");
    }
    assert!(started.elapsed() < Duration::from_secs(60));
}

#[test]
fn files_over_the_size_limit_are_neither_parsed_nor_chunked() {
    let limits = ParseLimits {
        max_bytes: 1000,
        ..ParseLimits::default()
    };
    let text = "fn a() {}\n".repeat(200);
    let file = parse_with(&path("src/big.rs"), &text, &limits, None);
    assert_eq!(
        file.degraded,
        Some(Degradation::TooLarge {
            bytes: text.len(),
            limit: 1000
        })
    );
    assert!(file.symbols.is_empty());
    assert!(
        chunks(&file, &text, &ChunkOptions::default())
            .unwrap()
            .is_empty()
    );
    assert_eq!(skeleton(&file, &text).unwrap(), "");
}

#[test]
fn deeply_nested_input_is_rejected_before_parsing() {
    let text = format!("x = {}{};\n", "[\n".repeat(5000), "]\n".repeat(5000));
    let file = parsed("src/deep.js", &text);
    assert!(
        matches!(
            file.degraded,
            Some(Degradation::TooDeep { limit: 1024, .. })
        ),
        "{:?}",
        file.degraded
    );
    let list = chunked(&file, &text, &ChunkOptions::default());
    assert!(list.iter().all(|c| c.kind == ChunkKind::Text));
    assert_covers(&text, &list);

    // Deep but within the limit: parsed normally.
    let text = format!(
        "x = {}{};\nfunction after() {{ return 1; }}\n",
        "[".repeat(900),
        "]".repeat(900)
    );
    let file = parsed("src/deep_ok.js", &text);
    assert_eq!(file.degraded, None);
    expect_symbols(&file, &[(K::Function, "after", 2, 2)]);

    // Unbalanced openers, never closed.
    let text = format!("function before() {{}}\nx = {}\n", "(".repeat(1000));
    let file = parsed("src/unbalanced.js", &text);
    assert!(file.has_errors);
    chunked(&file, &text, &ChunkOptions::default());

    // Deep indentation (Python nesting is indentation, not brackets).
    let text: String = (0..300)
        .map(|i| format!("{}if x:\n", "    ".repeat(i)))
        .collect::<String>()
        + &"    ".repeat(300)
        + "pass\n";
    let file = parsed("deep.py", &text);
    chunked(&file, &text, &ChunkOptions::default());
}

#[test]
fn deeply_nested_declarations() {
    // 120 nested classes: long qualified names, no recursion limits hit.
    let mut text = String::new();
    for depth in 0..120 {
        text.push_str(&format!("{}class C{depth}:\n", "    ".repeat(depth)));
    }
    text.push_str(&format!("{}pass\n", "    ".repeat(120)));
    let file = parsed("nested.py", &text);
    assert_eq!(file.symbols.len(), 120);
    let deepest = file.symbols.last().unwrap();
    assert!(deepest.qualified_name.starts_with("C0.C1.C2."));
    assert!(deepest.qualified_name.ends_with(".C118.C119"));
    let _ = skeleton_of(&file, &text);
    chunked(&file, &text, &ChunkOptions::default());
}

/// Chunking and skeletons walk nesting with explicit stacks: hundreds of
/// nested classes, each larger than the chunk target, on a 512 KiB stack.
#[test]
fn deep_nesting_does_not_recurse() {
    let worker = std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(|| {
            // Java: 600 nested classes; the innermost holds a large method.
            let depth = 600;
            let mut text = String::new();
            for level in 0..depth {
                text.push_str(&format!("class C{level} {{\n"));
            }
            text.push_str("void work() {\n");
            for i in 0..120 {
                text.push_str(&format!("    int value{i} = compute({i}, {i}, {i});\n"));
            }
            text.push_str("}\n");
            text.push_str(&"}\n".repeat(depth));
            let file = parsed("Nested.java", &text);
            assert_eq!(file.degraded, None);
            assert_eq!(
                file.symbols.iter().filter(|s| s.kind == K::Class).count(),
                depth
            );
            let list = chunked(&file, &text, &ChunkOptions::default());
            assert_covers(&text, &list);
            let skeleton = skeleton_of(&file, &text);
            assert!(skeleton.lines().count() >= depth * 2);

            // Python: indentation nesting. (tree-sitter-python serialises
            // its indent stack into a 1 KiB buffer, so it tracks ~500 levels.)
            let depth = 400;
            let mut text = String::new();
            for level in 0..depth {
                text.push_str(&format!("{}class C{level}:\n", " ".repeat(level)));
            }
            let pad = " ".repeat(depth);
            for i in 0..120 {
                text.push_str(&format!("{pad}value_{i} = compute({i}, {i}, {i})\n"));
            }
            let file = parsed("nested.py", &text);
            assert_eq!(file.degraded, None);
            assert_eq!(
                file.symbols.iter().filter(|s| s.kind == K::Class).count(),
                depth
            );
            let list = chunked(&file, &text, &ChunkOptions::default());
            assert_covers(&text, &list);
            assert!(skeleton_of(&file, &text).lines().count() >= depth);
        })
        .unwrap();
    worker.join().unwrap();
}

/// Flat files with thousands of declarations stay linear (sibling lookups
/// are done in one tree walk) and finish within the parse budget.
#[test]
fn flat_files_with_many_declarations() {
    let header: String = (0..15_000)
        .map(|i| format!("/* Register {i}. */\n#define REG_{i} 0x{i:04x}\n"))
        .collect();
    let file = parsed("include/regs.h", &header);
    assert_eq!(file.degraded, None);
    assert_eq!(file.symbols.len(), 15_000);
    let last = file.symbols.last().unwrap();
    assert_eq!(last.name, "REG_14999");
    assert_eq!(last.doc.as_deref(), Some("Register 14999."));
    chunked(&file, &header, &ChunkOptions::default());

    let dump: String = (0..10_000)
        .map(|i| format!("-- row {i}\nINSERT INTO plans (id, name) VALUES ({i}, 'plan {i}');\n"))
        .collect();
    let file = parsed("db/seed.sql", &dump);
    assert_eq!(file.degraded, None);
    assert_eq!(file.blocks.len(), 10_000);
    assert_eq!(file.blocks[9_999].range.start(), 19_999);
    assert_eq!(file.blocks[9_999].subject.as_deref(), Some("plans"));
    chunked(&file, &dump, &ChunkOptions::default());
}

#[test]
fn timeout_and_cancellation() {
    let text = "export function f(a: number): number { return a + 1; }\n".repeat(5000);
    let limits = ParseLimits {
        timeout: Duration::ZERO,
        ..ParseLimits::default()
    };
    let file = parse_with(&path("src/slow.ts"), &text, &limits, None);
    assert_eq!(file.degraded, Some(Degradation::Timeout));
    assert!(file.symbols.is_empty());
    let list = chunked(&file, &text, &ChunkOptions::default());
    assert!(list.iter().all(|c| c.kind == ChunkKind::Text));

    let cancel = AtomicBool::new(true);
    let file = parse_with(
        &path("src/slow.ts"),
        &text,
        &ParseLimits::default(),
        Some(&cancel),
    );
    assert_eq!(file.degraded, Some(Degradation::Cancelled));
}

#[test]
fn symbol_limit_truncates_and_says_so() {
    let text: String = (0..100).map(|i| format!("fn f{i}() {{}}\n")).collect();
    let limits = ParseLimits {
        max_symbols: 10,
        ..ParseLimits::default()
    };
    let file = parse_with(&path("src/many.rs"), &text, &limits, None);
    assert!(file.symbols.len() <= 10);
    assert_eq!(file.degraded, Some(Degradation::Truncated { limit: 10 }));
    chunked(&file, &text, &ChunkOptions::default());
}

#[test]
fn empty_and_blank_files() {
    for text in ["", "\n\n", "   \t\r\n"] {
        let file = parsed("src/empty.rs", text);
        assert!(file.symbols.is_empty() && file.imports.is_empty());
        assert_eq!(file.degraded, None);
        assert!(!file.has_errors);
        assert!(
            chunks(&file, text, &ChunkOptions::default())
                .unwrap()
                .is_empty()
        );
        assert_eq!(skeleton(&file, text).unwrap(), "");
    }
    assert_eq!(parsed("src/empty.rs", "").line_count, 0);
}

#[test]
fn crlf_matches_lf() {
    let lf = "import { b } from './b';\n\n/**\n * Doc line.\n */\nexport class A {\n  get(): number {\n    return 1;\n  }\n}\n";
    let crlf = lf.replace('\n', "\r\n");
    let a = parsed("src/a.ts", lf);
    let b = parsed("src/a.ts", &crlf);
    let shape = |f: &knowell_parse::ParsedFile| {
        f.symbols
            .iter()
            .map(|s| {
                (
                    s.kind,
                    s.qualified_name.clone(),
                    s.range,
                    s.signature.clone(),
                    s.doc.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(shape(&a), shape(&b));
    assert!(
        b.symbols
            .iter()
            .all(|s| !s.signature.contains('\r') && !s.doc.as_deref().unwrap_or("").contains('\r'))
    );
    assert_eq!(b.line_count, 10);
    assert_eq!(skeleton_of(&a, lf), skeleton_of(&b, &crlf));
    let list = chunked(&b, &crlf, &options(256));
    assert_covers(&crlf, &list);
}

#[test]
fn non_ascii_identifiers() {
    let python = "def grüße(名前: str) -> str:\n    \"\"\"Grüßt.\"\"\"\n    return 名前\n\nclass Ölçüm:\n    değer = 1\n";
    let file = parsed("src/i18n.py", python);
    expect_symbols(
        &file,
        &[
            (K::Function, "grüße", 1, 3),
            (K::Class, "Ölçüm", 5, 6),
            (K::Field, "Ölçüm.değer", 6, 6),
        ],
    );
    assert_eq!(
        symbol(&file, K::Function, "grüße").doc.as_deref(),
        Some("Grüßt.")
    );
    chunked(&file, python, &options(256));

    let js = "const café = () => 'crème';\nclass Größe { messen() { return 'ü'; } }\n";
    let file = parsed("src/i18n.js", js);
    expect_symbols(
        &file,
        &[
            (K::Function, "café", 1, 1),
            (K::Method, "Größe.messen", 2, 2),
        ],
    );
    let java = "public class Ölçüm {\n    public int değer() { return 1; }\n}\n";
    let file = parsed("Ölçüm.java", java);
    expect_symbols(&file, &[(K::Method, "Ölçüm.değer", 2, 2)]);
    let go = "package x\n\nfunc Größe() int { return 1 }\n";
    let file = parsed("x.go", go);
    assert_eq!(
        symbol(&file, K::Function, "Größe").visibility,
        Some(knowell_parse::Visibility::Public)
    );
}

#[test]
fn output_is_deterministic() {
    for (path, text) in FIXTURES {
        let first = parsed(path, text);
        // Parse other files in between so that caches are warm and shared.
        for (other_path, other_text) in FIXTURES.iter().rev() {
            let _ = parsed(other_path, other_text);
        }
        let second = parsed(path, text);
        assert_eq!(first, second, "{path}");
        let options = options(256);
        let a = chunks(&first, text, &options).unwrap();
        let b = chunks(&second, text, &options).unwrap();
        assert_eq!(a, b, "{path}");
        assert_eq!(skeleton_of(&first, text), skeleton_of(&second, text));
        for (x, y) in a.iter().zip(&b) {
            let cx = ChunkContext::for_chunk("p", &first, x);
            let cy = ChunkContext::for_chunk("p", &second, y);
            assert_eq!(prepared_input(x, &cx), prepared_input(y, &cy));
        }
    }
}

#[test]
fn parsing_is_thread_safe() {
    let handles: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(|| {
                FIXTURES
                    .iter()
                    .map(|(p, t)| parse(&path(p), t))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    for result in &results[1..] {
        assert_eq!(result, &results[0]);
    }
}

/// The raw-tree API for dependents (contract rule packs) applies the same
/// bounds as `parse`.
#[test]
fn raw_trees_for_dependents() {
    use knowell_parse::tree_sitter::{Query, QueryCursor, StreamingIterator};
    use knowell_parse::{Tier, parse_tree, ts_language};

    for language in Language::ALL {
        let has_grammar = ts_language(*language).is_some();
        let expected = language.tier() != Tier::TextOnly && *language != Language::Dockerfile;
        assert_eq!(has_grammar, expected, "{language}");
    }

    let text = "fn a() {}\nfn b() {}\n";
    let limits = ParseLimits::default();
    let tree = parse_tree(Language::Rust, text, &limits).unwrap();
    assert_eq!(tree.root_node().kind(), "source_file");
    let grammar = ts_language(Language::Rust).unwrap();
    let query = Query::new(&grammar, "(function_item name: (identifier) @name)").unwrap();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), text.as_bytes());
    let mut names = Vec::new();
    while let Some(m) = matches.next() {
        for capture in m.captures() {
            names.push(&text[capture.node.byte_range()]);
        }
    }
    assert_eq!(names, ["a", "b"]);

    // Same bounds as `parse`.
    assert!(parse_tree(Language::Rust, "", &limits).is_none());
    assert!(parse_tree(Language::Html, "<p>x</p>", &limits).is_none());
    assert!(parse_tree(Language::Dockerfile, "FROM a\n", &limits).is_none());
    let small = ParseLimits {
        max_bytes: 4,
        ..ParseLimits::default()
    };
    assert!(parse_tree(Language::Rust, text, &small).is_none());
    assert!(parse_tree(Language::JavaScript, &"var a=1;".repeat(1000), &limits).is_none());
    let deep = format!("x = {};\n", "[\n".repeat(2000));
    assert!(parse_tree(Language::JavaScript, &deep, &limits).is_none());
    let instant = ParseLimits {
        timeout: Duration::ZERO,
        ..ParseLimits::default()
    };
    let big = "export function f(a: number): number { return a + 1; }\n".repeat(5000);
    assert!(parse_tree(Language::TypeScript, &big, &instant).is_none());
    // Syntax errors still yield a tree.
    let broken = parse_tree(Language::Rust, "fn a( {", &limits).unwrap();
    assert!(broken.root_node().has_error());
}

#[test]
fn mismatched_text_is_an_error() {
    let file = parsed("src/a.rs", "fn a() {}\n");
    let err = chunks(&file, "fn b() {}\n", &ChunkOptions::default()).unwrap_err();
    assert!(matches!(err, ParseError::TextMismatch { .. }));
    assert!(err.to_string().contains("src/a.rs"));
    assert!(skeleton(&file, "fn a() {}").is_err());
}

#[test]
fn generated_files_are_flagged() {
    let go = "// Code generated by protoc-gen-go. DO NOT EDIT.\n// source: billing.proto\n\npackage billing\n\ntype Plan struct{}\n";
    let file = parsed("api/billing/plan.go", go);
    assert!(file.is_generated);
    // Still analysed: generated code is linked to its source, not hidden.
    expect_symbols(&file, &[(K::Struct, "Plan", 6, 6)]);

    let client = "/* tslint:disable */\n/**\n * Billing API\n * NOTE: This class is auto generated by OpenAPI Generator (https://openapi-generator.tech).\n * Do not edit the class manually.\n */\nexport class BillingApi {}\n";
    assert!(parsed("src/client/api.ts", client).is_generated);
    assert!(
        parsed(
            "Cargo.lock",
            "# This file is automatically @generated by Cargo.\nversion = 4\n"
        )
        .is_generated
    );
    assert!(parsed("go.sum", "x v1 h1:abc=\n").is_generated);
    assert!(!parsed("src/a.rs", "fn a() {}\n").is_generated);
    assert_eq!(parsed("go.sum", "").language, Language::Text);
}
