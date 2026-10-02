//! Integration tests: build SCIP messages in memory, encode, read back, query.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use knowell_core::{ContentHash, RepoPath};
use knowell_scip::{
    Freshness, Limits, OccurrenceRole, ScipError, ScipIndex, SymbolKind, read_index_bytes,
    read_index_file,
};
use protobuf::{EnumOrUnknown, Message};
use scip::types as pb;

const CANCEL: &str = "rust-analyzer cargo pay 0.1.0 service/Payment#cancel().";
const TRAIT: &str = "rust-analyzer cargo pay 0.1.0 service/Refundable#";
const TRAIT_CANCEL: &str = "rust-analyzer cargo pay 0.1.0 service/Refundable#cancel().";
const IMPL_TYPE: &str = "rust-analyzer cargo pay 0.1.0 service/Payment#";

fn occ(range: &[i32], symbol: &str, roles: i32) -> pb::Occurrence {
    pb::Occurrence {
        range: range.to_vec(),
        symbol: symbol.to_owned(),
        symbol_roles: roles,
        ..Default::default()
    }
}

fn info(symbol: &str, kind: pb::symbol_information::Kind, name: &str) -> pb::SymbolInformation {
    pb::SymbolInformation {
        symbol: symbol.to_owned(),
        kind: EnumOrUnknown::new(kind),
        display_name: name.to_owned(),
        documentation: vec!["first".into(), "second".into()],
        ..Default::default()
    }
}

fn doc(path: &str, text: &str, occurrences: Vec<pb::Occurrence>) -> pb::Document {
    pb::Document {
        relative_path: path.to_owned(),
        language: "rust".into(),
        text: text.to_owned(),
        occurrences,
        ..Default::default()
    }
}

fn sample_message() -> pb::Index {
    let mut def = occ(&[1, 11, 17], CANCEL, 1);
    def.enclosing_range = vec![1, 4, 3, 5];
    let mut cancel_info = info(CANCEL, pb::symbol_information::Kind::Method, "cancel");
    cancel_info.relationships = vec![pb::Relationship {
        symbol: TRAIT_CANCEL.into(),
        is_implementation: true,
        ..Default::default()
    }];
    let mut service = doc(
        "src/service.rs",
        "line0\nfn cancel() {}\n",
        vec![
            def,
            occ(&[0, 0, 5], IMPL_TYPE, 1),
            occ(&[6, 2, 6, 8], "local 1", 1),
        ],
    );
    service.symbols = vec![
        cancel_info,
        info("local 1", pb::symbol_information::Kind::Variable, "x"),
    ];
    let mut trait_doc = doc(
        "src/traits.rs",
        "",
        vec![occ(&[2, 0, 9], TRAIT, 1), occ(&[3, 0, 9], TRAIT_CANCEL, 1)],
    );
    trait_doc.symbols = vec![info(
        TRAIT,
        pb::symbol_information::Kind::Trait,
        "Refundable",
    )];
    let main = doc(
        "src/main.rs",
        "",
        vec![
            occ(&[10, 4, 10, 10], CANCEL, 0),
            occ(&[11, 4, 10], CANCEL, 8),
            occ(&[12, 0, 5], "local 1", 0),
        ],
    );
    pb::Index {
        metadata: protobuf::MessageField::some(pb::Metadata {
            tool_info: protobuf::MessageField::some(pb::ToolInfo {
                name: "rust-analyzer".into(),
                version: "1.0".into(),
                ..Default::default()
            }),
            project_root: "file:///work/pay".into(),
            ..Default::default()
        }),
        documents: vec![service, trait_doc, main],
        external_symbols: vec![info(
            "scheme pkg m 1 ext/Thing#",
            pb::symbol_information::Kind::Class,
            "Thing",
        )],
        ..Default::default()
    }
}

fn read(msg: &pb::Index) -> ScipIndex {
    let bytes = msg.write_to_bytes().unwrap();
    read_index_bytes(&bytes, &Limits::default()).unwrap()
}

fn path(p: &str) -> RepoPath {
    RepoPath::new(p).unwrap()
}

#[test]
fn round_trip_maps_symbols_and_metadata() {
    let idx = read(&sample_message());
    assert_eq!(idx.metadata().tool_name, "rust-analyzer");
    assert_eq!(idx.metadata().project_root, "file:///work/pay");
    assert_eq!(idx.documents().len(), 3);
    let id = idx.find_symbol(CANCEL).unwrap();
    let sym = idx.symbol(id).unwrap();
    assert_eq!(sym.display_name, "cancel");
    assert_eq!(sym.kind, SymbolKind::Method);
    assert_eq!(sym.documentation, "first\n\nsecond");
    assert_eq!(sym.relationships.len(), 1);
    assert!(sym.relationships[0].is_implementation);
    assert!(idx.find_symbol("scheme pkg m 1 ext/Thing#").is_some());
    assert_eq!(idx.report().invalid_occurrences, 0);
}

#[test]
fn display_name_falls_back_to_moniker() {
    let mut msg = sample_message();
    msg.documents[2]
        .occurrences
        .push(occ(&[20, 0, 3], "scip-x npm p 1 mod/helper().", 0));
    let idx = read(&msg);
    let id = idx.find_symbol("scip-x npm p 1 mod/helper().").unwrap();
    assert_eq!(idx.symbol(id).unwrap().display_name, "helper");
    msg.documents[2]
        .occurrences
        .push(occ(&[21, 0, 3], "not a parseable symbol", 0));
    let idx = read(&msg);
    let id = idx.find_symbol("not a parseable symbol").unwrap();
    assert_eq!(
        idx.symbol(id).unwrap().display_name,
        "not a parseable symbol"
    );
}

#[test]
fn ranges_convert_to_one_based_lines() {
    let idx = read(&sample_message());
    let id = idx.find_symbol(CANCEL).unwrap();
    let defs = idx.definitions(id);
    assert_eq!(defs.len(), 1);
    let d = defs[0];
    assert_eq!(d.path.as_str(), "src/service.rs");
    assert_eq!(d.occurrence.line_range.start(), 2);
    assert_eq!(d.occurrence.line_range.end(), 2);
    assert_eq!(d.occurrence.span.start_character, 11);
    assert_eq!(d.occurrence.role, OccurrenceRole::Definition);
    let enc = d.occurrence.enclosing_range.unwrap();
    assert_eq!((enc.start(), enc.end()), (2, 4));
}

#[test]
fn references_and_roles() {
    let idx = read(&sample_message());
    let id = idx.find_symbol(CANCEL).unwrap();
    let refs = idx.references(id);
    assert_eq!(refs.len(), 2);
    assert_eq!(refs[0].occurrence.role, OccurrenceRole::Reference);
    assert_eq!(refs[1].occurrence.role, OccurrenceRole::ReadAccess);
    assert!(refs.iter().all(|r| r.path.as_str() == "src/main.rs"));
    assert_eq!(idx.occurrences(id).len(), 3);
}

#[test]
fn implementations_follow_relationships() {
    let idx = read(&sample_message());
    let target = idx.find_symbol(TRAIT_CANCEL).unwrap();
    let imps = idx.implementations(target);
    assert_eq!(imps.len(), 1);
    assert_eq!(imps[0].path.as_str(), "src/service.rs");
    let implementer = idx.find_symbol(CANCEL).unwrap();
    assert_eq!(idx.implementing_symbols(target), vec![implementer]);
    assert!(idx.implementations(implementer).is_empty());
}

#[test]
fn local_symbols_are_scoped_per_document() {
    let idx = read(&sample_message());
    assert!(idx.find_symbol("local 1").is_none());
    let a = idx
        .find_local_symbol(&path("src/service.rs"), "local 1")
        .unwrap();
    let b = idx
        .find_local_symbol(&path("src/main.rs"), "local 1")
        .unwrap();
    assert_ne!(a, b);
    assert!(idx.symbol(a).unwrap().is_local());
    assert_eq!(idx.symbol(a).unwrap().kind, SymbolKind::Variable);
    assert_eq!(idx.occurrences(a).len(), 1);
    assert_eq!(idx.occurrences(b).len(), 1);
    assert!(idx.references(a).is_empty());
}

#[test]
fn symbol_at_position() {
    let idx = read(&sample_message());
    let hit = idx.symbol_at(&path("src/main.rs"), 10, 5).unwrap();
    assert_eq!(
        idx.symbol(hit.occurrence.symbol).unwrap().scip_symbol,
        CANCEL
    );
    assert!(idx.symbol_at(&path("src/main.rs"), 10, 10).is_none());
    assert!(idx.symbol_at(&path("src/main.rs"), 99, 0).is_none());
    assert!(idx.symbol_at(&path("src/absent.rs"), 0, 0).is_none());
}

#[test]
fn symbol_at_prefers_innermost_nested_span() {
    let mut msg = sample_message();
    msg.documents[2]
        .occurrences
        .push(occ(&[30, 0, 30, 40], IMPL_TYPE, 0));
    msg.documents[2]
        .occurrences
        .push(occ(&[30, 10, 30, 14], TRAIT, 0));
    let idx = read(&msg);
    let hit = idx.symbol_at(&path("src/main.rs"), 30, 12).unwrap();
    assert_eq!(
        idx.symbol(hit.occurrence.symbol).unwrap().scip_symbol,
        TRAIT
    );
    let hit = idx.symbol_at(&path("src/main.rs"), 30, 20).unwrap();
    assert_eq!(
        idx.symbol(hit.occurrence.symbol).unwrap().scip_symbol,
        IMPL_TYPE
    );
}

#[test]
fn invalid_ranges_are_dropped_and_counted() {
    let mut msg = sample_message();
    let bad = &mut msg.documents[2].occurrences;
    bad.push(occ(&[], CANCEL, 0));
    bad.push(occ(&[1, 2], CANCEL, 0));
    bad.push(occ(&[1, 2, 3, 4, 5], CANCEL, 0));
    bad.push(occ(&[-1, 0, 4], CANCEL, 0));
    bad.push(occ(&[0, 9, 4], CANCEL, 0));
    bad.push(occ(&[5, 0, 4, 0], CANCEL, 0));
    bad.push(occ(&[i32::MIN, 0, i32::MAX], CANCEL, 0));
    bad.push(occ(&[1, 0, 4], "", 0));
    let mut bad_enclosing = occ(&[40, 0, 3], CANCEL, 1);
    bad_enclosing.enclosing_range = vec![9, 9];
    bad.push(bad_enclosing);
    let idx = read(&msg);
    assert_eq!(idx.report().invalid_occurrences, 8);
    assert_eq!(idx.report().invalid_enclosing_ranges, 1);
    let id = idx.find_symbol(CANCEL).unwrap();
    // The occurrence with the bad enclosing range survives without it.
    let defs = idx.definitions(id);
    assert_eq!(defs.len(), 2);
    assert!(defs.iter().any(|d| d.occurrence.enclosing_range.is_none()));
}

#[test]
fn escaping_and_invalid_paths_are_rejected() {
    let mut msg = sample_message();
    for p in [
        "../secret.rs",
        "src/../../x.rs",
        "/etc/passwd",
        "C:/Windows/x.rs",
        "src\\win.rs",
        "./a.rs",
        "a//b.rs",
        "",
        "nul\0.rs",
    ] {
        msg.documents
            .push(doc(p, "", vec![occ(&[0, 0, 1], CANCEL, 0)]));
    }
    let idx = read(&msg);
    assert_eq!(idx.documents().len(), 3);
    assert_eq!(idx.report().rejected_document_count, 9);
    assert_eq!(idx.report().rejected_documents.len(), 9);
    assert!(
        idx.documents()
            .iter()
            .all(|d| !d.path.as_str().contains(".."))
    );
}

#[test]
fn rejected_document_list_is_capped() {
    let mut msg = pb::Index::new();
    for i in 0..250 {
        msg.documents.push(doc(&format!("../x{i}.rs"), "", vec![]));
    }
    let idx = read(&msg);
    assert_eq!(idx.report().rejected_document_count, 250);
    assert_eq!(idx.report().rejected_documents.len(), 100);
}

#[test]
fn duplicate_document_paths_are_merged() {
    let mut msg = sample_message();
    msg.documents
        .push(doc("src/main.rs", "", vec![occ(&[50, 0, 3], CANCEL, 0)]));
    let idx = read(&msg);
    assert_eq!(idx.documents().len(), 3);
    assert_eq!(
        idx.document(&path("src/main.rs"))
            .unwrap()
            .occurrences
            .len(),
        4
    );
}

#[test]
fn freshness_contract() {
    let mut idx = read(&sample_message());
    let text = "line0\nfn cancel() {}\n";
    let current = ContentHash::of(text.as_bytes());
    let changed = ContentHash::of(b"edited");
    let svc = path("src/service.rs");
    assert_eq!(idx.coverage(&svc, &current), Freshness::Fresh);
    assert!(idx.covers(&svc, &current));
    assert_eq!(idx.coverage(&svc, &changed), Freshness::Stale);
    assert!(!idx.covers(&svc, &changed));
    // main.rs has no embedded text: unknown until a manifest supplies a hash.
    let main = path("src/main.rs");
    assert_eq!(idx.coverage(&main, &current), Freshness::Unknown);
    assert!(!idx.covers(&main, &current));
    assert_eq!(
        idx.coverage(&path("nope.rs"), &current),
        Freshness::NotIndexed
    );
    let n = idx.apply_manifest([
        (main.clone(), current),
        (svc.clone(), changed), // must not override the embedded-text hash
        (path("nope.rs"), current),
    ]);
    assert_eq!(n, 1);
    assert!(idx.covers(&main, &current));
    assert!(idx.covers(&svc, &current));
    assert!(idx.source_revision().is_none());
    idx.set_source_revision("abc123");
    assert_eq!(idx.source_revision(), Some("abc123"));
}

#[test]
fn retain_documents_drops_stale_occurrences() {
    let mut idx = read(&sample_message());
    let id = idx.find_symbol(CANCEL).unwrap();
    assert_eq!(idx.occurrences(id).len(), 3);
    idx.retain_documents(|d| d.path.as_str() != "src/main.rs");
    assert_eq!(idx.occurrences(id).len(), 1);
    assert!(idx.document(&path("src/main.rs")).is_none());
    assert!(idx.symbol_at(&path("src/main.rs"), 10, 5).is_none());
}

#[test]
fn limits_are_enforced() {
    let bytes = sample_message().write_to_bytes().unwrap();
    let tiny = Limits {
        max_bytes: 10,
        ..Limits::default()
    };
    assert!(matches!(
        read_index_bytes(&bytes, &tiny),
        Err(ScipError::TooLarge { .. })
    ));
    let l = Limits {
        max_documents: 2,
        ..Limits::default()
    };
    assert!(matches!(
        read_index_bytes(&bytes, &l),
        Err(ScipError::LimitExceeded {
            what: "documents",
            ..
        })
    ));
    let l = Limits {
        max_occurrences: 3,
        ..Limits::default()
    };
    assert!(matches!(
        read_index_bytes(&bytes, &l),
        Err(ScipError::LimitExceeded {
            what: "occurrences",
            ..
        })
    ));
    let l = Limits {
        max_symbols: 2,
        ..Limits::default()
    };
    assert!(matches!(
        read_index_bytes(&bytes, &l),
        Err(ScipError::LimitExceeded {
            what: "symbols",
            ..
        })
    ));
    let l = Limits {
        max_documentation_bytes: 3,
        ..Limits::default()
    };
    let idx = read_index_bytes(&bytes, &l).unwrap();
    let id = idx.find_symbol(CANCEL).unwrap();
    assert_eq!(idx.symbol(id).unwrap().documentation, "fir");
}

#[test]
fn malformed_and_truncated_input_errors_without_panic() {
    let bytes = sample_message().write_to_bytes().unwrap();
    for cut in [1, 2, 7, bytes.len() / 2, bytes.len() - 1] {
        // A truncation may by chance end on a field boundary; it must never panic.
        let _ = read_index_bytes(&bytes[..cut], &Limits::default());
    }
    assert!(matches!(
        read_index_bytes(
            &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            &Limits::default()
        ),
        Err(ScipError::Decode(_))
    ));
    // Hostile: a length-delimited field claiming a huge payload.
    assert!(read_index_bytes(&[0x12, 0xff, 0xff, 0xff, 0x7f, 0x01], &Limits::default()).is_err());
    // Empty input is a valid, empty index.
    let empty = read_index_bytes(&[], &Limits::default()).unwrap();
    assert!(empty.documents().is_empty());
}

#[test]
fn oversize_symbols_are_ignored() {
    let mut msg = sample_message();
    let huge = format!("x npm p 1 {}#", "a".repeat(5000));
    msg.documents[2]
        .occurrences
        .push(occ(&[60, 0, 3], &huge, 0));
    msg.external_symbols
        .push(info(&huge, pb::symbol_information::Kind::Class, "Huge"));
    let idx = read(&msg);
    assert!(idx.find_symbol(&huge).is_none());
    assert_eq!(idx.report().invalid_occurrences, 1);
    assert_eq!(idx.report().invalid_symbols, 1);
}

#[test]
fn reads_from_file_and_reports_io_errors() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("index.scip");
    std::fs::write(&file, sample_message().write_to_bytes().unwrap()).unwrap();
    let idx = read_index_file(&file, &Limits::default()).unwrap();
    assert_eq!(idx.documents().len(), 3);
    let tiny = Limits {
        max_bytes: 8,
        ..Limits::default()
    };
    assert!(matches!(
        read_index_file(&file, &tiny),
        Err(ScipError::TooLarge { .. })
    ));
    assert!(matches!(
        read_index_file(&dir.path().join("missing.scip"), &Limits::default()),
        Err(ScipError::Io { .. })
    ));
}

#[test]
fn position_encoding_is_kept() {
    let mut msg = sample_message();
    msg.documents[0].position_encoding =
        EnumOrUnknown::new(pb::PositionEncoding::UTF16CodeUnitOffsetFromLineStart);
    let idx = read(&msg);
    assert_eq!(
        idx.document(&path("src/service.rs"))
            .unwrap()
            .position_encoding,
        knowell_scip::PositionEncoding::Utf16
    );
}
