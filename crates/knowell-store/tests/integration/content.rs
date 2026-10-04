use knowell_core::{ContentHash, LineRange};
use knowell_store::content::*;
use knowell_store::views::{self, GenerationPin};
use knowell_store::{StoreError, hierarchy};

use crate::common::{fixture, name, path, require_db};

fn h(text: &str) -> ContentHash {
    ContentHash::of(text.as_bytes())
}

fn blob(text: &str) -> NewContent {
    NewContent {
        hash: h(text),
        size_bytes: text.len() as u64,
        language: Some("rust".into()),
        redacted_text: Some(text.to_owned()),
    }
}

fn chunk(content: &str, ordinal: u32, start: u32, end: u32) -> NewChunk {
    NewChunk {
        content_hash: h(content),
        parser_version: "ts-rust-1".into(),
        ordinal,
        lines: LineRange::new(start, end).unwrap(),
        start_byte: u64::from(start) * 10,
        end_byte: u64::from(end) * 10,
        kind: "function".into(),
        symbol_path: Some(format!("crate::f{ordinal}")),
        prepared_input_hash: h(&format!("{content}#{ordinal}")),
    }
}

#[tokio::test]
async fn bulk_content_and_chunk_upserts_are_idempotent() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let org = fx.org.id;

    // More rows than one batch, to cover batching.
    let blobs: Vec<NewContent> = (0..12_000)
        .map(|i| blob(&format!("fn f{i}() {{}}")))
        .collect();
    assert_eq!(upsert_contents(&mut c, org, &blobs).await.unwrap(), 12_000);
    assert_eq!(upsert_contents(&mut c, org, &blobs).await.unwrap(), 0);
    // Duplicates inside one call are fine for content (immutable).
    assert_eq!(
        upsert_contents(&mut c, org, &[blob("new"), blob("new")])
            .await
            .unwrap(),
        1
    );
    let stored = get_content(&mut c, org, &h("fn f7() {}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.redacted_text.as_deref(), Some("fn f7() {}"));
    assert_eq!(stored.size_bytes, 10);
    assert_eq!(stored.language.as_deref(), Some("rust"));

    let missing = missing_contents(&mut c, org, &[h("unknown"), h("fn f1() {}"), h("unknown")])
        .await
        .unwrap();
    assert_eq!(missing, vec![h("unknown")]);

    // Content never crosses tenants.
    let other = hierarchy::create_organization(&mut c, &name("other"))
        .await
        .unwrap();
    assert_eq!(
        get_content(&mut c, other.id, &h("fn f7() {}"))
            .await
            .unwrap(),
        None
    );

    let chunks = vec![chunk("new", 0, 1, 3), chunk("new", 1, 4, 9)];
    assert_eq!(upsert_chunks(&mut c, org, &chunks).await.unwrap(), 2);
    assert_eq!(
        upsert_chunks(&mut c, org, &chunks).await.unwrap(),
        0,
        "unchanged rows are not rewritten"
    );
    let mut changed = chunks.clone();
    changed[1].kind = "method".into();
    assert_eq!(upsert_chunks(&mut c, org, &changed).await.unwrap(), 1);
    let read = chunks_of(&mut c, org, &h("new"), "ts-rust-1")
        .await
        .unwrap();
    assert_eq!(
        read.iter().map(|c| c.chunk.clone()).collect::<Vec<_>>(),
        changed
    );
    assert!(
        chunks_of(&mut c, org, &h("new"), "ts-rust-2")
            .await
            .unwrap()
            .is_empty()
    );

    // A key twice in one call, and chunks of unknown content, are rejected.
    assert!(matches!(
        upsert_chunks(&mut c, org, &[chunk("new", 0, 1, 2), chunk("new", 0, 1, 2)]).await,
        Err(StoreError::InvalidInput(_))
    ));
    assert!(matches!(
        upsert_chunks(&mut c, org, &[chunk("never stored", 0, 1, 2)]).await,
        Err(StoreError::InvalidInput(_))
    ));
}

fn upsert(p: &str, content: &str) -> FileChange {
    FileChange::Upsert {
        path: path(p),
        content_hash: h(content),
        renamed_from: None,
    }
}

fn listing(files: Vec<FileVersion>) -> Vec<(String, ContentHash)> {
    files
        .into_iter()
        .map(|f| (f.path.to_string(), f.content_hash))
        .collect()
}

#[tokio::test]
async fn file_versions_track_changes_renames_and_retries() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let view = fx.view.id;
    let pin = |generation| GenerationPin { view, generation };

    let g1 = views::begin_generation(&mut c, view, None).await.unwrap();
    let s = apply_file_changes(
        &mut c,
        view,
        g1,
        &[
            upsert("src/a.rs", "a1"),
            upsert("src/b.rs", "b1"),
            upsert("README.md", "r1"),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        s,
        FileChangeSummary {
            added: 3,
            closed: 0
        }
    );
    views::activate_generation(&mut c, view, g1).await.unwrap();

    let g2 = views::begin_generation(&mut c, view, None).await.unwrap();
    let changes = [
        upsert("src/a.rs", "a2"),
        // Moved b.rs to lib/b.rs without changing it.
        FileChange::Upsert {
            path: path("lib/b.rs"),
            content_hash: h("b1"),
            renamed_from: Some(path("src/b.rs")),
        },
        FileChange::Delete {
            path: path("README.md"),
        },
        // A new file.
        upsert("src/c.rs", "c2"),
    ];
    let first = apply_file_changes(&mut c, view, g2, &changes)
        .await
        .unwrap();
    assert_eq!(
        first,
        FileChangeSummary {
            added: 3,
            closed: 3
        }
    );
    // A retry of the same changes in the same generation changes nothing.
    let retry = apply_file_changes(&mut c, view, g2, &changes)
        .await
        .unwrap();
    assert_eq!(retry, first);
    // Re-applying one path again replaces only that path's earlier attempt.
    let again = apply_file_changes(&mut c, view, g2, &[upsert("src/a.rs", "a2")])
        .await
        .unwrap();
    assert_eq!(
        again,
        FileChangeSummary {
            added: 1,
            closed: 1
        }
    );
    views::activate_generation(&mut c, view, g2).await.unwrap();

    assert_eq!(
        listing(files_at(&mut c, pin(1)).await.unwrap()),
        [
            ("README.md".to_owned(), h("r1")),
            ("src/a.rs".to_owned(), h("a1")),
            ("src/b.rs".to_owned(), h("b1")),
        ]
    );
    assert_eq!(
        listing(files_at(&mut c, pin(2)).await.unwrap()),
        [
            ("lib/b.rs".to_owned(), h("b1")),
            ("src/a.rs".to_owned(), h("a2")),
            ("src/c.rs".to_owned(), h("c2")),
        ]
    );
    // Re-sending identical content in a later generation adds nothing.
    let g3 = views::begin_generation(&mut c, view, None).await.unwrap();
    let same = apply_file_changes(&mut c, view, g3, &[upsert("src/a.rs", "a2")])
        .await
        .unwrap();
    assert_eq!(same, FileChangeSummary::default());
    apply_file_changes(&mut c, view, g3, &[upsert("lib/b.rs", "b3")])
        .await
        .unwrap();
    views::activate_generation(&mut c, view, g3).await.unwrap();

    // History follows the rename back to the old path.
    let history = file_history(&mut c, view, &path("lib/b.rs"), 10)
        .await
        .unwrap();
    let steps: Vec<(String, ContentHash, i64, Option<i64>)> = history
        .iter()
        .map(|f| (f.path.to_string(), f.content_hash, f.valid_from, f.valid_to))
        .collect();
    assert_eq!(
        steps,
        [
            ("lib/b.rs".to_owned(), h("b3"), 3, None),
            ("lib/b.rs".to_owned(), h("b1"), 2, Some(3)),
            ("src/b.rs".to_owned(), h("b1"), 1, Some(2)),
        ]
    );
    assert_eq!(history[1].renamed_from, Some(path("src/b.rs")));
    assert_eq!(
        file_history(&mut c, view, &path("lib/b.rs"), 1)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        file_history(&mut c, view, &path("nope.rs"), 10)
            .await
            .unwrap()
            .is_empty()
    );

    let a = file_at(&mut c, pin(1), &path("src/a.rs"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (a.content_hash, a.valid_from, a.valid_to),
        (h("a1"), 1, Some(2))
    );
    assert_eq!(
        file_at(&mut c, pin(2), &path("README.md")).await.unwrap(),
        None
    );

    // Malformed change sets are rejected before anything is written.
    let g4 = views::begin_generation(&mut c, view, None).await.unwrap();
    assert!(matches!(
        apply_file_changes(
            &mut c,
            view,
            g4,
            &[
                upsert("x.rs", "1"),
                FileChange::Delete { path: path("x.rs") }
            ]
        )
        .await,
        Err(StoreError::InvalidInput(_))
    ));
    assert!(matches!(
        apply_file_changes(
            &mut c,
            view,
            g4,
            &[FileChange::Upsert {
                path: path("x.rs"),
                content_hash: h("1"),
                renamed_from: Some(path("x.rs")),
            }]
        )
        .await,
        Err(StoreError::InvalidInput(_))
    ));
    // Writes to an activated generation are fenced off.
    assert!(matches!(
        apply_file_changes(&mut c, view, g3, &[upsert("late.rs", "1")]).await,
        Err(StoreError::GenerationNotBuilding { .. })
    ));
}

#[tokio::test]
async fn chunks_are_located_only_in_pinned_views_of_the_tenant() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let other = fixture(&mut c, "web").await; // another organization
    let view = fx.view.id;

    upsert_contents(&mut c, fx.org.id, &[blob("body")])
        .await
        .unwrap();
    upsert_chunks(&mut c, fx.org.id, &[chunk("body", 0, 1, 2)])
        .await
        .unwrap();
    let g1 = views::begin_generation(&mut c, view, None).await.unwrap();
    apply_file_changes(
        &mut c,
        view,
        g1,
        &[upsert("src/x.rs", "body"), upsert("src/y.rs", "body")],
    )
    .await
    .unwrap();
    views::activate_generation(&mut c, view, g1).await.unwrap();
    // The other tenant has a file with the same content.
    let og = views::begin_generation(&mut c, other.view.id, None)
        .await
        .unwrap();
    apply_file_changes(&mut c, other.view.id, og, &[upsert("z.rs", "body")])
        .await
        .unwrap();
    views::activate_generation(&mut c, other.view.id, og)
        .await
        .unwrap();

    let prepared = h("body#0");
    let pins = [
        GenerationPin {
            view,
            generation: 1,
        },
        GenerationPin {
            view: other.view.id,
            generation: 1,
        },
    ];
    let found = locate_prepared_inputs(&mut c, fx.org.id, &pins, &[prepared])
        .await
        .unwrap();
    let paths: Vec<String> = found.iter().map(|l| l.path.to_string()).collect();
    assert_eq!(paths, ["src/x.rs", "src/y.rs"]);
    assert_eq!(found[0].chunk.prepared_input_hash, prepared);
    assert_eq!(found[0].pin, pins[0]);
    // The other tenant has no chunk rows: nothing to locate.
    assert!(
        locate_prepared_inputs(&mut c, other.org.id, &pins, &[prepared])
            .await
            .unwrap()
            .is_empty()
    );
}

fn input(p: &str, content: &str, ordinal: u32, embed: bool) -> NewChunkInput {
    NewChunkInput {
        path: path(p),
        content_hash: h(content),
        ordinal,
        prepared_input_hash: h(&format!("{p}:{content}#{ordinal}")),
        embed,
    }
}

#[tokio::test]
async fn chunk_inputs_are_per_path_and_follow_their_file_versions() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let other = fixture(&mut c, "web").await; // another organization
    let view = fx.view.id;
    let org = fx.org.id;
    let pin = |generation| GenerationPin { view, generation };
    let parser = "ts-rust-1";

    // Identical content at two paths: one set of chunk rows, two inputs.
    upsert_contents(&mut c, org, &[blob("same"), blob("next")])
        .await
        .unwrap();
    upsert_chunks(
        &mut c,
        org,
        &[
            chunk("same", 0, 1, 2),
            chunk("same", 1, 3, 4),
            chunk("next", 0, 1, 1),
        ],
    )
    .await
    .unwrap();
    let g1 = views::begin_generation(&mut c, view, None).await.unwrap();
    apply_file_changes(
        &mut c,
        view,
        g1,
        &[upsert("a/x.rs", "same"), upsert("b/x.rs", "same")],
    )
    .await
    .unwrap();
    let paths = [path("a/x.rs"), path("b/x.rs")];
    let inputs = [
        input("a/x.rs", "same", 0, true),
        input("a/x.rs", "same", 1, false),
        input("b/x.rs", "same", 0, true),
        input("b/x.rs", "same", 1, true),
    ];
    assert_eq!(
        replace_chunk_inputs(&mut c, pin(g1), parser, &paths, &inputs)
            .await
            .unwrap(),
        4
    );
    // Idempotent: replacing again leaves the same rows.
    assert_eq!(
        replace_chunk_inputs(&mut c, pin(g1), parser, &paths, &inputs)
            .await
            .unwrap(),
        4
    );
    views::activate_generation(&mut c, view, g1).await.unwrap();
    let stored = chunk_inputs_at(&mut c, pin(g1), parser, None)
        .await
        .unwrap();
    assert_eq!(stored.len(), 4);
    assert!(stored.iter().all(|i| i.content_hash == h("same")));
    assert!(stored.iter().all(|i| i.file_valid_from == g1));
    assert_ne!(stored[0].prepared_input_hash, stored[2].prepared_input_hash);
    assert!(!stored[1].embed);
    let only_b = chunk_inputs_at(&mut c, pin(g1), parser, Some(&[path("b/x.rs")]))
        .await
        .unwrap();
    assert_eq!(only_b.len(), 2);
    assert!(
        chunk_inputs_at(&mut c, pin(g1), "other-parser", None)
            .await
            .unwrap()
            .is_empty()
    );

    // Each input locates its own path only, with the content's ranges.
    let pins = [pin(g1)];
    let a0 = input("a/x.rs", "same", 0, true).prepared_input_hash;
    let found = locate_chunk_inputs(&mut c, org, &pins, &[a0])
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, path("a/x.rs"));
    assert_eq!(found[0].chunk.prepared_input_hash, a0);
    assert_eq!(found[0].chunk.lines, LineRange::new(1, 2).unwrap());
    assert!(
        locate_chunk_inputs(&mut c, other.org.id, &pins, &[a0])
            .await
            .unwrap()
            .is_empty(),
        "never across tenants"
    );

    // A rename starts a new file version; its inputs are separate rows, and
    // the old path's inputs stay with the old generation.
    let g2 = views::begin_generation(&mut c, view, None).await.unwrap();
    apply_file_changes(
        &mut c,
        view,
        g2,
        &[
            FileChange::Upsert {
                path: path("c/x.rs"),
                content_hash: h("same"),
                renamed_from: Some(path("b/x.rs")),
            },
            upsert("a/x.rs", "next"),
        ],
    )
    .await
    .unwrap();
    replace_chunk_inputs(
        &mut c,
        pin(g2),
        parser,
        &[path("c/x.rs"), path("a/x.rs")],
        &[
            input("c/x.rs", "same", 0, true),
            input("a/x.rs", "next", 0, true),
        ],
    )
    .await
    .unwrap();
    let at_g2: Vec<(String, u32)> = chunk_inputs_at(&mut c, pin(g2), parser, None)
        .await
        .unwrap()
        .into_iter()
        .map(|i| (i.path.to_string(), i.ordinal))
        .collect();
    assert_eq!(at_g2, [("a/x.rs".to_owned(), 0), ("c/x.rs".to_owned(), 0)]);
    assert_eq!(
        chunk_inputs_at(&mut c, pin(g1), parser, None)
            .await
            .unwrap()
            .len(),
        4
    );
    let b0 = input("b/x.rs", "same", 0, true).prepared_input_hash;
    assert!(
        locate_chunk_inputs(&mut c, org, &[pin(g2)], &[b0])
            .await
            .unwrap()
            .is_empty(),
        "the old path's input is not located in the new generation"
    );

    // Failing the generation drops the rows of its file versions.
    views::fail_generation(&mut c, view, g2, "test")
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM chunk_input WHERE file_valid_from = $1")
            .bind(g2)
            .fetch_one(&mut *c)
            .await
            .unwrap(),
        0
    );

    // Malformed input is rejected before anything is written.
    let g3 = views::begin_generation(&mut c, view, None).await.unwrap();
    for (paths, inputs) in [
        (vec![path("a/x.rs")], vec![input("b/x.rs", "same", 0, true)]),
        (
            vec![path("a/x.rs")],
            vec![
                input("a/x.rs", "same", 0, true),
                input("a/x.rs", "same", 0, true),
            ],
        ),
        (vec![path("nope.rs")], vec![]),
        (vec![path("a/x.rs")], vec![input("a/x.rs", "next", 0, true)]),
    ] {
        assert!(
            matches!(
                replace_chunk_inputs(&mut c, pin(g3), parser, &paths, &inputs).await,
                Err(StoreError::InvalidInput(_))
            ),
            "{paths:?}"
        );
    }
    assert_eq!(
        replace_chunk_inputs(&mut c, pin(g3), parser, &[], &[])
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn legacy_content_level_inputs_are_still_located() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let view = fx.view.id;
    upsert_contents(&mut c, fx.org.id, &[blob("old")])
        .await
        .unwrap();
    upsert_chunks(&mut c, fx.org.id, &[chunk("old", 0, 1, 2)])
        .await
        .unwrap();
    let g1 = views::begin_generation(&mut c, view, None).await.unwrap();
    apply_file_changes(&mut c, view, g1, &[upsert("src/x.rs", "old")])
        .await
        .unwrap();
    views::activate_generation(&mut c, view, g1).await.unwrap();
    let pins = [GenerationPin {
        view,
        generation: g1,
    }];
    // No per-path inputs were recorded: the content-level hash is used.
    let found = locate_chunk_inputs(&mut c, fx.org.id, &pins, &[h("old#0")])
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, path("src/x.rs"));
    // Once inputs are recorded, the legacy hash no longer locates the file.
    replace_chunk_inputs(
        &mut c,
        pins[0],
        "ts-rust-1",
        &[path("src/x.rs")],
        &[input("src/x.rs", "old", 0, true)],
    )
    .await
    .unwrap();
    assert!(
        locate_chunk_inputs(&mut c, fx.org.id, &pins, &[h("old#0")])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        locate_chunk_inputs(
            &mut c,
            fx.org.id,
            &pins,
            &[input("src/x.rs", "old", 0, true).prepared_input_hash]
        )
        .await
        .unwrap()
        .len(),
        1
    );
}

#[tokio::test]
async fn texts_are_read_in_batches_and_never_across_tenants() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let other = fixture(&mut c, "web").await;
    let blobs: Vec<NewContent> = (0..(BATCH_ROWS + 3))
        .map(|i| blob(&format!("text {i}")))
        .collect();
    upsert_contents(&mut c, fx.org.id, &blobs).await.unwrap();
    upsert_contents(
        &mut c,
        fx.org.id,
        &[NewContent {
            redacted_text: None,
            ..blob("no text")
        }],
    )
    .await
    .unwrap();
    let mut wanted: Vec<ContentHash> = blobs.iter().map(|b| b.hash).collect();
    wanted.push(h("text 0")); // duplicates are fine
    wanted.push(h("never stored"));
    wanted.push(h("no text"));
    let texts = redacted_texts(&mut c, fx.org.id, &wanted).await.unwrap();
    assert_eq!(texts.len(), BATCH_ROWS + 3);
    assert_eq!(texts.get(&h("text 7")).map(String::as_str), Some("text 7"));
    assert!(!texts.contains_key(&h("no text")));
    assert!(
        redacted_texts(&mut c, other.org.id, &wanted)
            .await
            .unwrap()
            .is_empty()
    );

    // The full-blob batch API also returns metadata for non-text content.
    wanted.reverse();
    let contents = get_contents(&mut c, fx.org.id, &wanted).await.unwrap();
    assert_eq!(contents.len(), blobs.len() + 1);
    assert!(contents.windows(2).all(|pair| pair[0].hash < pair[1].hash));
    assert!(contents.iter().all(|row| row.organization == fx.org.id));
    let text_seven = contents.iter().find(|row| row.hash == h("text 7")).unwrap();
    assert_eq!(text_seven.redacted_text.as_deref(), Some("text 7"));
    assert_eq!(text_seven.language.as_deref(), Some("rust"));
    assert_eq!(text_seven.size_bytes, 6);
    assert!(
        contents
            .iter()
            .find(|row| row.hash == h("no text"))
            .unwrap()
            .redacted_text
            .is_none()
    );
    assert!(
        get_contents(&mut c, fx.org.id, &[])
            .await
            .unwrap()
            .is_empty()
    );

    let mut chunks: Vec<_> = blobs
        .iter()
        .map(|b| NewChunk {
            content_hash: b.hash,
            parser_version: "batch-parser-1".into(),
            ordinal: 0,
            lines: LineRange::new(1, 1).unwrap(),
            start_byte: 0,
            end_byte: b.size_bytes,
            kind: "text".into(),
            symbol_path: None,
            prepared_input_hash: b.hash,
        })
        .collect();
    let mut extra = chunks[0].clone();
    extra.ordinal = 1;
    extra.prepared_input_hash = h("second batch-parser-1 input");
    chunks.push(extra);
    upsert_chunks(&mut c, fx.org.id, &chunks).await.unwrap();
    let alternate = NewChunk {
        parser_version: "batch-parser-2".into(),
        prepared_input_hash: h("batch-parser-2 input"),
        ..chunks[0].clone()
    };
    upsert_chunks(&mut c, fx.org.id, std::slice::from_ref(&alternate))
        .await
        .unwrap();
    chunks.sort_by_key(|chunk| (chunk.content_hash, chunk.ordinal));
    let read = chunks_of_many(&mut c, fx.org.id, &wanted, "batch-parser-1")
        .await
        .unwrap();
    assert_eq!(
        read.iter().map(|row| row.chunk.clone()).collect::<Vec<_>>(),
        chunks
    );
    assert!(read.iter().all(|row| row.organization == fx.org.id));
    let alternate_read = chunks_of_many(&mut c, fx.org.id, &wanted, "batch-parser-2")
        .await
        .unwrap();
    assert_eq!(
        alternate_read
            .iter()
            .map(|row| row.chunk.clone())
            .collect::<Vec<_>>(),
        vec![alternate]
    );
    assert!(
        chunks_of_many(&mut c, fx.org.id, &wanted, "unknown-parser")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        chunks_of_many(&mut c, fx.org.id, &[], "batch-parser-1")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        chunks_of_many(&mut c, other.org.id, &wanted, "batch-parser-1")
            .await
            .unwrap()
            .is_empty()
    );

    // Identical original bytes may have tenant-specific redaction and analysis.
    upsert_contents(
        &mut c,
        other.org.id,
        &[
            NewContent {
                redacted_text: Some("tenant-specific redaction".into()),
                language: Some("text".into()),
                ..blobs[0].clone()
            },
            blob("other-tenant-only"),
        ],
    )
    .await
    .unwrap();
    let other_chunk = NewChunk {
        symbol_path: Some("other::symbol".into()),
        ..chunks
            .iter()
            .find(|row| row.content_hash == h("text 0") && row.ordinal == 0)
            .unwrap()
            .clone()
    };
    upsert_chunks(&mut c, other.org.id, std::slice::from_ref(&other_chunk))
        .await
        .unwrap();
    wanted.push(h("other-tenant-only"));
    assert_eq!(
        get_contents(&mut c, fx.org.id, &wanted).await.unwrap(),
        contents
    );
    assert_eq!(
        chunks_of_many(&mut c, fx.org.id, &wanted, "batch-parser-1")
            .await
            .unwrap(),
        read
    );
    let other_read = get_contents(&mut c, other.org.id, &wanted).await.unwrap();
    assert_eq!(other_read.len(), 2);
    assert!(
        other_read
            .iter()
            .all(|row| row.organization == other.org.id)
    );
    assert_eq!(
        other_read
            .iter()
            .find(|row| row.hash == h("text 0"))
            .unwrap()
            .redacted_text
            .as_deref(),
        Some("tenant-specific redaction")
    );
    let other_chunks = chunks_of_many(&mut c, other.org.id, &wanted, "batch-parser-1")
        .await
        .unwrap();
    assert_eq!(
        other_chunks
            .iter()
            .map(|row| row.chunk.clone())
            .collect::<Vec<_>>(),
        vec![other_chunk]
    );
}

#[tokio::test]
async fn language_catalog_counts_file_occurrences_at_the_tenant_generation() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "catalog").await;
    let other = fixture(&mut c, "other-catalog").await;
    let rust = blob("rust-body");
    let python = NewContent {
        language: Some("python".into()),
        ..blob("python-body")
    };
    let unknown = NewContent {
        language: None,
        redacted_text: None,
        ..blob("binary-body")
    };
    upsert_contents(&mut c, fx.org.id, &[rust, python, unknown])
        .await
        .unwrap();
    let g1 = views::begin_generation(&mut c, fx.view.id, None)
        .await
        .unwrap();
    apply_file_changes(
        &mut c,
        fx.view.id,
        g1,
        &[
            upsert("src/a.rs", "rust-body"),
            upsert("src/copy.rs", "rust-body"),
            upsert("scripts/main.py", "python-body"),
            upsert("asset.bin", "binary-body"),
        ],
    )
    .await
    .unwrap();
    views::activate_generation(&mut c, fx.view.id, g1)
        .await
        .unwrap();
    let pin1 = GenerationPin {
        view: fx.view.id,
        generation: g1,
    };
    let expected1 = std::collections::BTreeMap::from([
        ("rust".to_owned(), 2),
        ("python".to_owned(), 1),
        ("text".to_owned(), 1),
    ]);
    assert_eq!(
        file_languages_at(&mut c, fx.org.id, pin1).await.unwrap(),
        expected1
    );
    let g2 = views::begin_generation(&mut c, fx.view.id, None)
        .await
        .unwrap();
    apply_file_changes(
        &mut c,
        fx.view.id,
        g2,
        &[
            FileChange::Delete {
                path: path("src/copy.rs"),
            },
            FileChange::Delete {
                path: path("asset.bin"),
            },
            upsert("src/a.rs", "python-body"),
        ],
    )
    .await
    .unwrap();
    views::activate_generation(&mut c, fx.view.id, g2)
        .await
        .unwrap();
    let pin2 = GenerationPin {
        view: fx.view.id,
        generation: g2,
    };
    assert_eq!(
        file_languages_at(&mut c, fx.org.id, pin2).await.unwrap(),
        // A shared blob's Python hint cannot change the .rs occurrence's language.
        std::collections::BTreeMap::from([("python".to_owned(), 1), ("rust".to_owned(), 1)])
    );
    assert_eq!(
        file_languages_at(&mut c, fx.org.id, pin1).await.unwrap(),
        expected1
    );
    assert!(
        file_languages_at(&mut c, other.org.id, pin1)
            .await
            .unwrap()
            .is_empty()
    );

    // A foreign view with a hash also present locally must not leak its catalog.
    upsert_contents(
        &mut c,
        other.org.id,
        &[NewContent {
            language: Some("javascript".into()),
            ..blob("rust-body")
        }],
    )
    .await
    .unwrap();
    let foreign_generation = views::begin_generation(&mut c, other.view.id, None)
        .await
        .unwrap();
    apply_file_changes(
        &mut c,
        other.view.id,
        foreign_generation,
        &[upsert("foreign.js", "rust-body")],
    )
    .await
    .unwrap();
    views::activate_generation(&mut c, other.view.id, foreign_generation)
        .await
        .unwrap();
    let foreign_pin = GenerationPin {
        view: other.view.id,
        generation: foreign_generation,
    };
    assert!(
        file_languages_at(&mut c, fx.org.id, foreign_pin)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        file_languages_at(&mut c, other.org.id, foreign_pin)
            .await
            .unwrap(),
        std::collections::BTreeMap::from([("javascript".to_owned(), 1)])
    );
}

#[tokio::test]
async fn shared_body_languages_are_independent_of_blob_hint_and_insertion_order() {
    let db = require_db!();
    let mut c = db.conn().await;
    let text = "// shared language probe\n";
    for (label, first, second) in [
        ("markdown-first", "markdown", "rust"),
        ("rust-first", "rust", "markdown"),
    ] {
        let fx = fixture(&mut c, label).await;
        for language in [first, second] {
            upsert_contents(
                &mut c,
                fx.org.id,
                &[NewContent {
                    language: Some(language.into()),
                    ..blob(text)
                }],
            )
            .await
            .unwrap();
        }
        assert_eq!(
            get_content(&mut c, fx.org.id, &h(text))
                .await
                .unwrap()
                .unwrap()
                .language
                .as_deref(),
            Some(first)
        );
        let generation = views::begin_generation(&mut c, fx.view.id, None)
            .await
            .unwrap();
        let mut changes = vec![
            upsert("docs/probe.md", text),
            upsert("src/probe.rs", text),
            upsert("src/probe.ts", text),
        ];
        if first == "rust" {
            changes.reverse();
        }
        apply_file_changes(&mut c, fx.view.id, generation, &changes)
            .await
            .unwrap();
        views::activate_generation(&mut c, fx.view.id, generation)
            .await
            .unwrap();
        let pin = GenerationPin {
            view: fx.view.id,
            generation,
        };
        for (file_path, language) in [
            ("docs/probe.md", "markdown"),
            ("src/probe.rs", "rust"),
            ("src/probe.ts", "typescript"),
        ] {
            let version = file_at(&mut c, pin, &path(file_path))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(version.content_hash, h(text));
            assert_eq!(version.language.as_deref(), Some(language));
        }
        assert_eq!(
            file_languages_at(&mut c, fx.org.id, pin).await.unwrap(),
            std::collections::BTreeMap::from([
                ("markdown".to_owned(), 1),
                ("rust".to_owned(), 1),
                ("typescript".to_owned(), 1),
            ])
        );
    }
}

#[tokio::test]
async fn language_follows_same_hash_renames_and_repairs_unclassified_noops() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "renamed-language").await;
    let text = "// renamed shared language probe\n";
    upsert_contents(&mut c, fx.org.id, &[blob(text)])
        .await
        .unwrap();
    let g1 = views::begin_generation(&mut c, fx.view.id, None)
        .await
        .unwrap();
    apply_file_changes(&mut c, fx.view.id, g1, &[upsert("docs/probe.md", text)])
        .await
        .unwrap();
    views::activate_generation(&mut c, fx.view.id, g1)
        .await
        .unwrap();
    let g2 = views::begin_generation(&mut c, fx.view.id, None)
        .await
        .unwrap();
    apply_file_changes(
        &mut c,
        fx.view.id,
        g2,
        &[FileChange::Upsert {
            path: path("src/probe.rs"),
            content_hash: h(text),
            renamed_from: Some(path("docs/probe.md")),
        }],
    )
    .await
    .unwrap();
    views::activate_generation(&mut c, fx.view.id, g2)
        .await
        .unwrap();
    let pin1 = GenerationPin {
        view: fx.view.id,
        generation: g1,
    };
    assert_eq!(
        file_at(&mut c, pin1, &path("docs/probe.md"))
            .await
            .unwrap()
            .unwrap()
            .language
            .as_deref(),
        Some("markdown")
    );
    let history = file_history(&mut c, fx.view.id, &path("src/probe.rs"), 10)
        .await
        .unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].language.as_deref(), Some("rust"));
    assert_eq!(history[1].language.as_deref(), Some("markdown"));

    sqlx::query(
        "UPDATE file_version SET language = NULL, language_detection_version = 0
         WHERE view_id = $1 AND path = 'src/probe.rs' AND valid_to IS NULL",
    )
    .bind(fx.view.id)
    .execute(&mut *c)
    .await
    .unwrap();
    let g3 = views::begin_generation(&mut c, fx.view.id, None)
        .await
        .unwrap();
    for _ in 0..2 {
        let summary = apply_file_changes(&mut c, fx.view.id, g3, &[upsert("src/probe.rs", text)])
            .await
            .unwrap();
        assert_eq!(
            summary,
            FileChangeSummary {
                added: 0,
                closed: 0
            }
        );
    }
    let repaired: (i64, Option<String>, i16) = sqlx::query_as(
        "SELECT valid_from, language, language_detection_version FROM file_version
         WHERE view_id = $1 AND path = 'src/probe.rs' AND valid_to IS NULL",
    )
    .bind(fx.view.id)
    .fetch_one(&mut *c)
    .await
    .unwrap();
    assert_eq!(repaired, (g2, Some("rust".into()), 1));
}

#[tokio::test]
async fn occurrence_detection_refines_headers_and_preserves_unknown_languages() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "language-detector").await;
    let long_shebang = format!("#!/usr/bin/env {}python3\nprint(1)\n", " ".repeat(70_000));
    let unicode_header = format!(
        "{}\nnamespace outside_byte_prefix {{}}\n",
        "é".repeat(32_768)
    );
    let cases = [
        (
            "include/cpp.h",
            "namespace probe {}\n".to_owned(),
            Some("cpp"),
            true,
        ),
        (
            "include/objc.h",
            "@interface Probe\n@end\n".to_owned(),
            Some("objc"),
            true,
        ),
        (
            "include/plain.h",
            "int probe(void);\n".to_owned(),
            Some("c"),
            true,
        ),
        ("include/unicode.h", unicode_header, Some("c"), true),
        (
            "bin/probe",
            "#!/usr/bin/env python3\nprint(1)\n".to_owned(),
            Some("python"),
            true,
        ),
        ("bin/long-probe", long_shebang, Some("python"), true),
        (
            "src/override.rs",
            "#!/usr/bin/env python3\n".to_owned(),
            Some("rust"),
            true,
        ),
        (
            "docs/explicit.txt",
            "#!/usr/bin/env python3\n".to_owned(),
            Some("text"),
            true,
        ),
        (
            "data/unknown.bin",
            "no conclusive detector\n".to_owned(),
            Some("text"),
            true,
        ),
        (
            "data/unavailable.bin",
            "unavailable binary".to_owned(),
            None,
            false,
        ),
        (
            "include/unavailable.h",
            "unavailable header".to_owned(),
            None,
            false,
        ),
    ];
    let contents: Vec<_> = cases
        .iter()
        .map(|(_, text, _, available)| NewContent {
            language: Some("python".into()),
            redacted_text: available.then(|| text.clone()),
            ..blob(text)
        })
        .collect();
    upsert_contents(&mut c, fx.org.id, &contents).await.unwrap();
    let generation = views::begin_generation(&mut c, fx.view.id, None)
        .await
        .unwrap();
    let changes: Vec<_> = cases
        .iter()
        .map(|(p, text, _, _)| upsert(p, text))
        .collect();
    apply_file_changes(&mut c, fx.view.id, generation, &changes)
        .await
        .unwrap();
    let pin = GenerationPin {
        view: fx.view.id,
        generation,
    };
    for (p, _, expected, _) in &cases {
        let version = file_at(&mut c, pin, &path(p)).await.unwrap().unwrap();
        assert_eq!(version.language.as_deref(), *expected, "{p}");
    }
    let classified: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM file_version WHERE view_id = $1 AND language_detection_version = 1",
    )
    .bind(fx.view.id)
    .fetch_one(&mut *c)
    .await
    .unwrap();
    assert_eq!(classified, i64::try_from(cases.len()).unwrap());
}

#[tokio::test]
async fn header_detection_reads_only_the_occurrence_tenants_redacted_body() {
    let db = require_db!();
    let mut c = db.conn().await;
    let first = fixture(&mut c, "first-header-tenant").await;
    let second = fixture(&mut c, "second-header-tenant").await;
    let missing = fixture(&mut c, "missing-header-tenant").await;
    let hash = h("synthetic original shared header");
    for (fx, text, expected) in [
        (&first, Some("namespace first {}\n"), Some("cpp")),
        (&second, Some("@interface Second\n@end\n"), Some("objc")),
        (&missing, None, None),
    ] {
        if let Some(text) = text {
            upsert_contents(
                &mut c,
                fx.org.id,
                &[NewContent {
                    hash,
                    size_bytes: text.len() as u64,
                    language: Some("rust".into()),
                    redacted_text: Some(text.to_owned()),
                }],
            )
            .await
            .unwrap();
        }
        let generation = views::begin_generation(&mut c, fx.view.id, None)
            .await
            .unwrap();
        apply_file_changes(
            &mut c,
            fx.view.id,
            generation,
            &[FileChange::Upsert {
                path: path("include/shared.h"),
                content_hash: hash,
                renamed_from: None,
            }],
        )
        .await
        .unwrap();
        let version = file_at(
            &mut c,
            GenerationPin {
                view: fx.view.id,
                generation,
            },
            &path("include/shared.h"),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(version.language.as_deref(), expected);
    }
}

#[tokio::test]
async fn same_hash_noop_recovers_language_when_missing_tenant_source_becomes_available() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "recovered-language").await;
    let text = "#!/usr/bin/env python3\nprint(1)\n";
    let hash = h(text);
    let change = FileChange::Upsert {
        path: path("bin/recovered"),
        content_hash: hash,
        renamed_from: None,
    };
    let g1 = views::begin_generation(&mut c, fx.view.id, None)
        .await
        .unwrap();
    apply_file_changes(&mut c, fx.view.id, g1, std::slice::from_ref(&change))
        .await
        .unwrap();
    views::activate_generation(&mut c, fx.view.id, g1)
        .await
        .unwrap();
    let before: (Option<String>, i16) = sqlx::query_as(
        "SELECT language, language_detection_version FROM file_version
         WHERE view_id = $1 AND path = 'bin/recovered'",
    )
    .bind(fx.view.id)
    .fetch_one(&mut *c)
    .await
    .unwrap();
    assert_eq!(before, (None, 1));
    let g2 = views::begin_generation(&mut c, fx.view.id, None)
        .await
        .unwrap();
    upsert_contents(&mut c, fx.org.id, &[blob(text)])
        .await
        .unwrap();
    let expected_summary = FileChangeSummary {
        added: 0,
        closed: 0,
    };
    assert_eq!(
        apply_file_changes(&mut c, fx.view.id, g2, std::slice::from_ref(&change))
            .await
            .unwrap(),
        expected_summary
    );
    let after: (i64, Option<String>, i16, String) = sqlx::query_as(
        "SELECT valid_from, language, language_detection_version, xmin::text FROM file_version
         WHERE view_id = $1 AND path = 'bin/recovered'",
    )
    .bind(fx.view.id)
    .fetch_one(&mut *c)
    .await
    .unwrap();
    assert_eq!(after.0, g1);
    assert_eq!(after.1.as_deref(), Some("python"));
    assert_eq!(after.2, 1);
    assert_eq!(
        apply_file_changes(&mut c, fx.view.id, g2, std::slice::from_ref(&change))
            .await
            .unwrap(),
        expected_summary
    );
    let unchanged: (i64, Option<String>, i16, String) = sqlx::query_as(
        "SELECT valid_from, language, language_detection_version, xmin::text FROM file_version
         WHERE view_id = $1 AND path = 'bin/recovered'",
    )
    .bind(fx.view.id)
    .fetch_one(&mut *c)
    .await
    .unwrap();
    assert_eq!(unchanged, after);
}
