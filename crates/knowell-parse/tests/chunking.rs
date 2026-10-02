//! Chunking rules and embedding inputs.

mod common;

use common::*;
use knowell_parse::{
    ChunkContext, ChunkKind, ChunkOptions, PARSER_VERSION, PREPARED_FORMAT_VERSION, prepared_input,
};
use pretty_assertions::assert_eq;

#[test]
fn small_files_are_one_chunk() {
    let text = "use std::fmt;\n\npub fn tiny() -> u32 { 1 }\n";
    let file = parsed("src/tiny.rs", text);
    let list = chunked(&file, text, &ChunkOptions::default());
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].kind, ChunkKind::File);
    assert_eq!(list[0].text, text.trim_end());
    assert_eq!(list[0].symbol_path.as_deref(), Some("tiny"));
}

#[test]
fn small_adjacent_units_are_grouped_and_leftovers_come_first() {
    let mut text = String::from("use a::b;\nuse c::d;\n\n");
    for i in 0..6 {
        text.push_str(&format!("pub const C{i}: u32 = {i};\n"));
    }
    text.push_str("\nstatic INIT: () = setup();\n\n");
    text.push_str(&format!(
        "/// A larger function.\npub fn large() -> u32 {{\n{}    0\n}}\n",
        "    let value = compute_something_expensive(1, 2, 3);\n".repeat(12)
    ));
    let file = parsed("src/consts.rs", &text);
    let options = ChunkOptions {
        target_chars: 1000,
        min_chars: 200,
        overlap_chars: 0,
    };
    let list = chunked(&file, &text, &options);
    assert_covers(&text, &list);
    assert_eq!(list[0].kind, ChunkKind::TopLevel);
    assert!(list[0].text.starts_with("use a::b;\nuse c::d;"));
    // Six constants and the static are adjacent and small: one group.
    let group = list.iter().find(|c| c.kind == ChunkKind::Group).unwrap();
    assert!(group.text.contains("C0") && group.text.contains("C5") && group.text.contains("INIT"));
    assert_eq!(group.symbol_path, None);
    let large = chunk_for(&list, "large");
    assert_eq!(large.kind, ChunkKind::Function);
    assert!(large.text.starts_with("/// A larger function."));
}

#[test]
fn oversized_leaf_is_split_into_linked_pieces() {
    let mut body = String::new();
    for block in 0..12 {
        for line in 0..6 {
            body.push_str(&format!(
                "    let v{block}_{line} = step({block}, {line});\n"
            ));
        }
        body.push('\n');
    }
    let text = format!("/// Does many steps.\npub fn long_one() {{\n{body}}}\n");
    let file = parsed("src/long.rs", &text);
    let options = options(600);
    let list = chunked(&file, &text, &options);
    assert_covers(&text, &list);
    assert!(list.len() > 3);
    let first = &list[0];
    assert_eq!(first.kind, ChunkKind::Function);
    assert_eq!(first.symbol_path.as_deref(), Some("long_one"));
    assert!(
        first
            .text
            .starts_with("/// Does many steps.\npub fn long_one() {")
    );
    for piece in &list[1..] {
        assert_eq!(piece.kind, ChunkKind::Function);
        assert_eq!(piece.parent, Some(first.ordinal));
        assert_eq!(piece.symbol, first.symbol);
        // Breaks prefer blank lines: every piece starts at a block boundary.
        assert!(
            piece.text.trim_start().starts_with("let v"),
            "{}",
            piece.text
        );
        assert!(
            piece.text.trim_start().contains("_0 = step("),
            "{}",
            piece.text
        );
    }
    // Pieces tile the function in order.
    for pair in list.windows(2) {
        assert!(pair[0].byte_range.end <= pair[1].byte_range.start);
    }
}

#[test]
fn oversized_class_becomes_header_plus_members() {
    let mut text = String::from(
        "import { Repo } from './repo';\n\n/** Service. */\nexport class Service {\n  private readonly repo: Repo;\n\n",
    );
    for i in 0..5 {
        text.push_str(&format!(
            "  /** Step {i}. */\n  async step{i}(id: string): Promise<number> {{\n"
        ));
        for j in 0..10 {
            text.push_str(&format!(
                "    const value{j} = await this.repo.load(id, {i}, {j});\n"
            ));
        }
        text.push_str("    return 0;\n  }\n\n");
    }
    text.push_str("  get size(): number { return 1; }\n}\n");
    let file = parsed("src/service.ts", &text);
    let options = options(1200);
    let list = chunked(&file, &text, &options);
    assert_covers(&text, &list);
    let header = chunk_for(&list, "Service");
    assert_eq!(header.kind, ChunkKind::Class);
    assert!(
        header
            .text
            .starts_with("/** Service. */\nexport class Service {\n  private readonly repo: Repo;")
    );
    // The largest members are elided (ties: first in source order) until the
    // header fits; elided members get their own chunks under the header.
    let mut elided = 0;
    for i in 0..5 {
        let path = format!("Service.step{i}");
        let elision = format!("  async step{i}(id: string): Promise<number> {{ … }}");
        if header.text.contains(&elision) {
            elided += 1;
            let member = chunk_for(&list, &path);
            assert_eq!(member.kind, ChunkKind::Method);
            assert_eq!(member.parent, Some(header.ordinal));
            assert!(member.text.starts_with(&format!("/** Step {i}. */")));
        } else {
            assert!(
                header.text.contains(&format!("/** Step {i}. */")),
                "{}",
                header.text
            );
            assert!(
                list.iter()
                    .all(|c| c.symbol_path.as_deref() != Some(path.as_str()))
            );
        }
    }
    assert_eq!(elided, 4, "{}", header.text);
    assert!(header.text.contains("  async step0(id: string)"));
    assert!(header.text.len() <= 1200);
    // Small members stay inline in the header.
    assert!(header.text.contains("get size(): number { return 1; }"));
    assert!(
        list.iter()
            .all(|c| c.symbol_path.as_deref() != Some("Service.size"))
    );
}

#[test]
fn test_functions_are_test_chunks() {
    let rust = "pub fn add(a: u32, b: u32) -> u32 { a + b }\n\n#[test]\nfn adds() {\n    assert_eq!(add(1, 2), 3);\n}\n";
    let file = parsed("src/math.rs", rust);
    let list = chunked(
        &file,
        rust,
        &ChunkOptions {
            min_chars: 10,
            ..ChunkOptions::default()
        },
    );
    assert_eq!(chunk_for(&list, "adds").kind, ChunkKind::Test);
    assert_eq!(chunk_for(&list, "add").kind, ChunkKind::Function);

    let go = "package math\n\nimport \"testing\"\n\nfunc TestAdd(t *testing.T) {\n\tif add(1, 2) != 3 {\n\t\tt.Fatal(\"bad\")\n\t}\n}\n\nfunc helper() int {\n\treturn 1\n}\n";
    let file = parsed("math/add_test.go", go);
    let list = chunked(
        &file,
        go,
        &ChunkOptions {
            min_chars: 10,
            ..ChunkOptions::default()
        },
    );
    assert_eq!(chunk_for(&list, "TestAdd").kind, ChunkKind::Test);
    assert_eq!(chunk_for(&list, "helper").kind, ChunkKind::Function);

    let python = "import pytest\n\n\ndef test_cancel():\n    assert cancel('s1')\n\n\ndef make_fixture():\n    return 1\n";
    let file = parsed("tests/test_billing.py", python);
    let list = chunked(
        &file,
        python,
        &ChunkOptions {
            min_chars: 10,
            ..ChunkOptions::default()
        },
    );
    assert_eq!(chunk_for(&list, "test_cancel").kind, ChunkKind::Test);
    assert_eq!(chunk_for(&list, "make_fixture").kind, ChunkKind::Function);

    let java = "class BillingTest {\n    @Test\n    void cancels() {\n        Billing billing = new Billing(new FakeRepository(\"subscriptions\"));\n        assertTrue(billing.cancel(\"s1\"));\n        assertFalse(billing.cancel(\"missing\"));\n    }\n\n    void helperWithALongName() {\n        System.out.println(\"helping with a fairly long line of text\");\n        System.out.println(\"and another fairly long line of text\");\n        System.out.println(\"and a third fairly long line of text\");\n    }\n}\n";
    let file = parsed("src/test/java/BillingTest.java", java);
    let options = ChunkOptions {
        target_chars: 256,
        min_chars: 10,
        overlap_chars: 0,
    };
    let list = chunked(&file, java, &options);
    assert_eq!(
        chunk_for(&list, "BillingTest.cancels").kind,
        ChunkKind::Test
    );
    assert_eq!(
        chunk_for(&list, "BillingTest.helperWithALongName").kind,
        ChunkKind::Method
    );
}

#[test]
fn text_chunker_uses_paragraphs_headings_and_overlap() {
    let mut text = String::new();
    for section in 0..4 {
        text.push_str(&format!("# Section {section}\n\n"));
        for paragraph in 0..5 {
            text.push_str(&format!(
                "Paragraph {paragraph} of section {section} says something useful about the system.\nIt continues on a second line.\n\n"
            ));
        }
    }
    let file = parsed("notes/design.txt", &text);
    assert!(file.symbols.is_empty());
    let options = ChunkOptions {
        target_chars: 700,
        min_chars: 300,
        overlap_chars: 120,
    };
    let list = chunked(&file, &text, &options);
    assert_covers(&text, &list);
    assert!(
        list.iter()
            .all(|c| c.kind == ChunkKind::Text && c.symbol_path.is_none())
    );
    // Headings start chunks once the current chunk is big enough.
    assert!(
        list.iter()
            .filter(|c| c.text.contains("# Section "))
            .all(|c| {
                let at = c.text.find("# Section ").unwrap();
                at == 0 || c.text[..at].len() <= 120
            })
    );
    // Consecutive chunks overlap by at most the configured bytes.
    for pair in list.windows(2) {
        let overlap = pair[0]
            .byte_range
            .end
            .saturating_sub(pair[1].byte_range.start);
        assert!(overlap <= 120, "overlap {overlap}");
    }
    assert!(
        list.windows(2)
            .any(|p| p[0].byte_range.end > p[1].byte_range.start)
    );
}

#[test]
fn chunk_options_are_normalised() {
    let text = "fn a() {}\n".repeat(200);
    let file = parsed("src/a.rs", &text);
    let tiny = ChunkOptions {
        target_chars: 1,
        min_chars: 10_000,
        overlap_chars: 10_000,
    };
    let list = chunked(&file, &text, &tiny);
    assert!(list.iter().all(|c| c.text.len() <= 256));
}

#[test]
fn prepared_input_layout_and_hash() {
    let text = "/// Service.\npub struct Service;\n\nimpl Service {\n    /// Cancels.\n    pub fn cancel(&self) -> bool {\n        true\n    }\n}\n";
    let file = parsed("src/service.rs", text);
    let options = ChunkOptions {
        target_chars: 256,
        min_chars: 10,
        overlap_chars: 0,
    };
    let list = chunked(&file, text, &options);
    let chunk = chunk_for(&list, "Service");
    let chunk = if chunk.text.starts_with("impl") {
        chunk
    } else {
        list.iter()
            .find(|c| c.text.starts_with("impl Service"))
            .unwrap()
    };
    let context = ChunkContext::for_chunk("billing-api", &file, chunk);
    let input = prepared_input(chunk, &context);
    assert_eq!(input.title, "src/service.rs · Service");
    assert_eq!(
        input.text,
        format!(
            "path: src/service.rs\nlanguage: rust\nproject: billing-api\nsymbol: Service (class)\n\n{}",
            chunk.text
        )
    );
    // Same input, same hash; any change to the input changes it.
    assert_eq!(input.hash, prepared_input(chunk, &context).hash);
    let other = ChunkContext {
        project: "other",
        ..context
    };
    assert_ne!(input.hash, prepared_input(chunk, &other).hash);
    assert_eq!((PARSER_VERSION, PREPARED_FORMAT_VERSION), (1, 1));
}

#[test]
fn chunk_context_carries_the_container() {
    let mut text =
        String::from("# Billing service.\nclass Service:\n    \"\"\"Handles billing.\"\"\"\n\n");
    text.push_str("    def cancel(self, subscription_id):\n        \"\"\"Cancels.\"\"\"\n");
    for i in 0..40 {
        text.push_str(&format!("        step_{i} = self.repo.run({i})\n"));
        if i % 8 == 7 {
            text.push('\n');
        }
    }
    let file = parsed("billing/service.py", &text);
    let list = chunked(&file, &text, &options(500));
    let method_pieces: Vec<_> = list
        .iter()
        .filter(|c| c.symbol_path.as_deref() == Some("Service.cancel"))
        .collect();
    assert!(method_pieces.len() > 1);
    // The first piece of the method: its container is the class.
    let first = ChunkContext::for_chunk("p", &file, method_pieces[0]);
    assert_eq!(first.container_signature, Some("class Service"));
    assert_eq!(first.doc, Some("Handles billing."));
    // Continuation pieces: the container is the method itself.
    let next = ChunkContext::for_chunk("p", &file, method_pieces[1]);
    assert_eq!(
        next.container_signature,
        Some("def cancel(self, subscription_id)")
    );
    assert_eq!(next.doc, Some("Cancels."));
    let input = prepared_input(method_pieces[1], &next);
    assert!(
        input
            .text
            .contains("\ncontainer: def cancel(self, subscription_id)\ndoc: Cancels.\n\n")
    );
}
