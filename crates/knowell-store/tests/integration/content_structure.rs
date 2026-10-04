use knowell_core::{ContentHash, LineRange};
use knowell_store::StoreError;
use knowell_store::content::{self, ChunkKey, ChunkStructure, NewChunk, NewContent, SourceRange};
use knowell_store::views::{self, GenerationPin};

use crate::common::{fixture, path, require_db};

#[tokio::test]
async fn retained_commit_prefixes_do_not_select_a_different_tenant_path_or_building_pin() {
    let db = require_db!();
    let mut conn = db.conn().await;
    let fx = fixture(&mut conn, "prefix-collision").await;
    let foreign = fixture(&mut conn, "foreign-prefix-collision").await;
    let input = blob("fn synthetic_source() {}\n");
    content::upsert_contents(&mut conn, fx.org.id, std::slice::from_ref(&input))
        .await
        .unwrap();
    let commit_prefix = "abcdefff1122";
    let old = format!("{commit_prefix}{}", "a".repeat(28));
    let current = format!("{commit_prefix}{}", "b".repeat(28));
    let hash_prefix = input.hash.to_string();
    let file_path = path("src/synthetic.rs");
    let first = views::begin_generation(&mut conn, fx.view.id, Some(&old))
        .await
        .unwrap();
    content::apply_file_changes(
        &mut conn,
        fx.view.id,
        first,
        &[content::FileChange::Upsert {
            path: file_path.clone(),
            content_hash: input.hash,
            renamed_from: None,
        }],
    )
    .await
    .unwrap();
    views::activate_generation(&mut conn, fx.view.id, first)
        .await
        .unwrap();
    let next = views::begin_generation(&mut conn, fx.view.id, Some(&current))
        .await
        .unwrap();
    content::apply_file_changes(
        &mut conn,
        fx.view.id,
        next,
        &[content::FileChange::Upsert {
            path: file_path.clone(),
            content_hash: input.hash,
            renamed_from: None,
        }],
    )
    .await
    .unwrap();
    assert!(
        !content::retained_source_commit_collision(
            &mut conn,
            fx.org.id,
            fx.view.id,
            &file_path,
            &hash_prefix,
            commit_prefix,
            &old
        )
        .await
        .unwrap()
    );
    views::activate_generation(&mut conn, fx.view.id, next)
        .await
        .unwrap();
    assert!(
        content::retained_source_commit_collision(
            &mut conn,
            fx.org.id,
            fx.view.id,
            &file_path,
            &hash_prefix,
            commit_prefix,
            &current
        )
        .await
        .unwrap()
    );
    assert!(
        !content::retained_source_commit_collision(
            &mut conn,
            foreign.org.id,
            fx.view.id,
            &file_path,
            &hash_prefix,
            commit_prefix,
            &current
        )
        .await
        .unwrap()
    );
    assert!(
        !content::retained_source_commit_collision(
            &mut conn,
            fx.org.id,
            foreign.view.id,
            &file_path,
            &hash_prefix,
            commit_prefix,
            &current
        )
        .await
        .unwrap()
    );
    assert!(
        !content::retained_source_commit_collision(
            &mut conn,
            fx.org.id,
            fx.view.id,
            &path("src/absent.rs"),
            &hash_prefix,
            commit_prefix,
            &current
        )
        .await
        .unwrap()
    );
    assert!(
        !content::retained_source_commit_collision(
            &mut conn,
            fx.org.id,
            fx.view.id,
            &file_path,
            &ContentHash::of(b"different synthetic body").to_string(),
            commit_prefix,
            &current
        )
        .await
        .unwrap()
    );
    for (hash, commit) in [
        ("", commit_prefix),
        (&hash_prefix, ""),
        ("not-hex", commit_prefix),
        (&hash_prefix, "abc%'"),
    ] {
        assert!(matches!(
            content::retained_source_commit_collision(
                &mut conn, fx.org.id, fx.view.id, &file_path, hash, commit, &current
            )
            .await,
            Err(StoreError::InvalidInput(_))
        ));
    }
}

fn blob(text: &str) -> NewContent {
    NewContent {
        hash: ContentHash::of(text.as_bytes()),
        size_bytes: text.len() as u64,
        language: Some("rust".into()),
        redacted_text: Some(text.into()),
    }
}

fn chunk(hash: ContentHash, ordinal: u32, start: u32, end: u32) -> NewChunk {
    NewChunk {
        content_hash: hash,
        parser_version: "synthetic-structure".into(),
        ordinal,
        lines: LineRange::new(start, end).unwrap(),
        start_byte: 0,
        end_byte: 100,
        kind: "function".into(),
        symbol_path: Some("synthetic".into()),
        prepared_input_hash: ContentHash::of(format!("synthetic input {ordinal}").as_bytes()),
    }
}

#[tokio::test]
async fn structure_metadata_is_idempotent_selected_and_tenant_scoped() {
    let db = require_db!();
    let mut conn = db.conn().await;
    let fx = fixture(&mut conn, "source-structure").await;
    let other = fixture(&mut conn, "other-source-structure").await;
    let input = blob("synthetic source\nsecond line\nthird line\n");
    content::upsert_contents(&mut conn, fx.org.id, std::slice::from_ref(&input))
        .await
        .unwrap();
    let chunks = [chunk(input.hash, 0, 1, 3), chunk(input.hash, 1, 2, 3)];
    content::upsert_chunks(&mut conn, fx.org.id, &chunks)
        .await
        .unwrap();
    let declaration = SourceRange {
        lines: LineRange::new(1, 3).unwrap(),
        start_byte: 0,
        end_byte: 100,
    };
    let structures: Vec<_> = chunks
        .iter()
        .map(|chunk| ChunkStructure {
            key: ChunkKey {
                content_hash: chunk.content_hash,
                parser_version: chunk.parser_version.clone(),
                ordinal: chunk.ordinal,
            },
            parent_ordinal: (chunk.ordinal == 1).then_some(0),
            declaration: Some(declaration),
            enclosing: None,
            source_exact: chunk.ordinal == 1,
        })
        .collect();
    assert_eq!(
        content::upsert_chunk_structures(&mut conn, fx.org.id, &structures)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        content::upsert_chunk_structures(&mut conn, fx.org.id, &structures)
            .await
            .unwrap(),
        0
    );
    let keys = [
        structures[1].key.clone(),
        structures[0].key.clone(),
        structures[1].key.clone(),
    ];
    assert_eq!(
        content::chunk_structures(&mut conn, fx.org.id, &keys)
            .await
            .unwrap(),
        structures
    );
    assert_eq!(
        content::chunk_structures_of(
            &mut conn,
            fx.org.id,
            &[input.hash, input.hash],
            "synthetic-structure"
        )
        .await
        .unwrap(),
        structures
    );
    assert!(
        content::chunk_structures(&mut conn, other.org.id, &keys)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        content::chunk_structures_of(&mut conn, fx.org.id, &[input.hash], "other-parser")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        content::upsert_chunk_structures(&mut conn, other.org.id, &structures).await,
        Err(StoreError::InvalidInput(_))
    ));
    assert!(matches!(
        content::upsert_chunk_structures(
            &mut conn,
            fx.org.id,
            &[structures[0].clone(), structures[0].clone()]
        )
        .await,
        Err(StoreError::InvalidInput(_))
    ));
    let mut invalid = structures[1].clone();
    invalid.parent_ordinal = Some(1);
    assert!(matches!(
        content::upsert_chunk_structures(&mut conn, fx.org.id, &[invalid]).await,
        Err(StoreError::InvalidInput(_))
    ));
    let mut changed = structures[1].clone();
    changed.source_exact = false;
    assert_eq!(
        content::upsert_chunk_structures(&mut conn, fx.org.id, &[changed])
            .await
            .unwrap(),
        1
    );
    sqlx::query("DELETE FROM content WHERE organization_id = $1 AND hash = $2")
        .bind(fx.org.id)
        .bind(input.hash.as_bytes().as_slice())
        .execute(&mut *conn)
        .await
        .unwrap();
    assert!(
        content::chunk_structures(&mut conn, fx.org.id, &keys)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn metadata_pages_keep_source_counts_versions_and_tenant_boundaries() {
    let db = require_db!();
    let mut conn = db.conn().await;
    let fx = fixture(&mut conn, "metadata-pages").await;
    let other = fixture(&mut conn, "foreign-metadata-pages").await;
    let input = blob("é first\nsecond\nthird\n");
    content::upsert_contents(&mut conn, fx.org.id, std::slice::from_ref(&input))
        .await
        .unwrap();
    let generation = views::begin_generation(&mut conn, fx.view.id, None)
        .await
        .unwrap();
    let pin = GenerationPin {
        view: fx.view.id,
        generation,
    };
    let changes: Vec<_> = ["z.rs", "a.rs", "m.rs"]
        .into_iter()
        .map(|name| content::FileChange::Upsert {
            path: path(name),
            content_hash: input.hash,
            renamed_from: None,
        })
        .collect();
    content::apply_file_changes(&mut conn, fx.view.id, generation, &changes)
        .await
        .unwrap();
    let first = content::files_metadata_at_page(&mut conn, fx.org.id, pin, None, 2)
        .await
        .unwrap();
    assert_eq!(
        first
            .iter()
            .map(|file| file.version.path.as_str())
            .collect::<Vec<_>>(),
        ["a.rs", "m.rs"]
    );
    assert!(first.iter().all(|file| file.line_count == Some(3)
        && file.has_text
        && file.size_bytes == input.size_bytes));
    let next =
        content::files_metadata_at_page(&mut conn, fx.org.id, pin, Some(&first[1].version.path), 2)
            .await
            .unwrap();
    assert_eq!(
        next.iter()
            .map(|file| file.version.path.as_str())
            .collect::<Vec<_>>(),
        ["z.rs"]
    );
    let selected = content::files_metadata_in_paths(
        &mut conn,
        fx.org.id,
        pin,
        &[path("z.rs"), path("a.rs"), path("z.rs"), path("missing.rs")],
    )
    .await
    .unwrap();
    assert_eq!(
        selected
            .iter()
            .map(|file| file.version.path.as_str())
            .collect::<Vec<_>>(),
        ["a.rs", "z.rs"]
    );
    assert!(
        content::files_metadata_at_page(&mut conn, other.org.id, pin, None, 2)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        content::files_metadata_in_paths(&mut conn, other.org.id, pin, &[path("a.rs")])
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        content::files_metadata_at_page(&mut conn, fx.org.id, pin, None, 0).await,
        Err(StoreError::InvalidInput(_))
    ));
    sqlx::query(
        "UPDATE content SET redacted_line_count = NULL WHERE organization_id = $1 AND hash = $2",
    )
    .bind(fx.org.id)
    .bind(input.hash.as_bytes().as_slice())
    .execute(&mut *conn)
    .await
    .unwrap();
    let legacy = content::files_metadata_at_page(&mut conn, fx.org.id, pin, None, 2)
        .await
        .unwrap();
    assert!(
        legacy
            .iter()
            .all(|file| file.line_count.is_none() && file.has_text)
    );
    views::activate_generation(&mut conn, fx.view.id, generation)
        .await
        .unwrap();
    let next_generation = views::begin_generation(&mut conn, fx.view.id, None)
        .await
        .unwrap();
    content::apply_file_changes(
        &mut conn,
        fx.view.id,
        next_generation,
        &[content::FileChange::Delete { path: path("a.rs") }],
    )
    .await
    .unwrap();
    let new_pin = GenerationPin {
        view: fx.view.id,
        generation: next_generation,
    };
    assert!(
        content::files_metadata_in_paths(&mut conn, fx.org.id, new_pin, &[path("a.rs")])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        content::files_metadata_in_paths(&mut conn, fx.org.id, pin, &[path("a.rs")])
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn bounded_content_read_counts_utf8_bytes_and_distinguishes_missing() {
    let db = require_db!();
    let mut conn = db.conn().await;
    let fx = fixture(&mut conn, "bounded-body").await;
    let other = fixture(&mut conn, "foreign-bounded-body").await;
    let input = blob("é\n");
    content::upsert_contents(&mut conn, fx.org.id, std::slice::from_ref(&input))
        .await
        .unwrap();
    assert!(matches!(
        content::get_content_bounded(&mut conn, fx.org.id, &input.hash, 2).await,
        Err(StoreError::InvalidInput(_))
    ));
    let read = content::get_content_bounded(&mut conn, fx.org.id, &input.hash, 3)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.redacted_text, input.redacted_text);
    assert_eq!(
        content::get_content_bounded(&mut conn, other.org.id, &input.hash, 3)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        content::get_content_bounded(
            &mut conn,
            fx.org.id,
            &ContentHash::of(b"missing synthetic"),
            3
        )
        .await
        .unwrap(),
        None
    );
}

#[tokio::test]
async fn metadata_does_not_silently_turn_missing_content_into_an_empty_file() {
    let db = require_db!();
    let mut conn = db.conn().await;
    let fx = fixture(&mut conn, "missing-metadata-content").await;
    let generation = views::begin_generation(&mut conn, fx.view.id, None)
        .await
        .unwrap();
    let pin = GenerationPin {
        view: fx.view.id,
        generation,
    };
    content::apply_file_changes(
        &mut conn,
        fx.view.id,
        generation,
        &[content::FileChange::Upsert {
            path: path("missing.rs"),
            content_hash: ContentHash::of(b"synthetic unstored body"),
            renamed_from: None,
        }],
    )
    .await
    .unwrap();
    assert!(matches!(
        content::files_metadata_at_page(&mut conn, fx.org.id, pin, None, 10).await,
        Err(StoreError::Corrupt(_))
    ));
    assert!(matches!(
        content::files_metadata_in_paths(&mut conn, fx.org.id, pin, &[path("missing.rs")]).await,
        Err(StoreError::Corrupt(_))
    ));
}
