use knowell_core::{ContentHash, LineRange};
use knowell_store::analysis::{self, MAX_COVERAGE_BYTES};
use knowell_store::content::{self, FileChange, NewContent};
use knowell_store::graph::{self, NewEdge, NodeRef};
use knowell_store::symbols::{self, NewOccurrence, NewSymbol};
use knowell_store::views::{self, GenerationPin};
use knowell_store::{EvidenceType, OccurrenceRole, Resolution, StoreError};
use serde_json::json;
use sqlx::Connection;

use crate::common::{Fixture, commit, fixture, path, require_db};

async fn source(
    conn: &mut sqlx::PgConnection,
    fx: &Fixture,
    text: &str,
) -> (GenerationPin, ContentHash) {
    let hash = ContentHash::of(text.as_bytes());
    content::upsert_contents(
        conn,
        fx.org.id,
        &[NewContent {
            hash,
            size_bytes: text.len() as u64,
            language: Some("rust".into()),
            redacted_text: Some(text.into()),
        }],
    )
    .await
    .unwrap();
    let generation = views::begin_generation(conn, fx.view.id, Some(&commit(1)))
        .await
        .unwrap();
    content::apply_file_changes(
        conn,
        fx.view.id,
        generation,
        &[FileChange::Upsert {
            path: path("src/lib.rs"),
            content_hash: hash,
            renamed_from: None,
        }],
    )
    .await
    .unwrap();
    (
        GenerationPin {
            view: fx.view.id,
            generation,
        },
        hash,
    )
}

#[tokio::test]
async fn prepared_imports_are_owned_revision_bound_immutable_and_explicitly_selected() {
    let db = require_db!();
    let mut conn = db.conn().await;
    let fx = fixture(&mut conn, "analysis-import").await;
    let other = fixture(&mut conn, "analysis-foreign").await;
    let revision = commit(1);
    let artifact = ContentHash::of(b"synthetic compiler artifact");
    let payload = json!({"format_version":1,"documents":[],"source_revision":revision});
    let id = analysis::stage_scip_import(
        &mut conn, fx.org.id, fx.view.id, &revision, &artifact, &payload,
    )
    .await
    .unwrap();
    assert_eq!(
        id,
        analysis::stage_scip_import(
            &mut conn, fx.org.id, fx.view.id, &revision, &artifact, &payload
        )
        .await
        .unwrap()
    );
    assert_eq!(
        Some(payload.clone()),
        analysis::load_scip_import(&mut conn, fx.org.id, fx.view.id, id, &revision)
            .await
            .unwrap()
    );
    for (organization, view, expected) in [
        (other.org.id, fx.view.id, revision.clone()),
        (fx.org.id, other.view.id, revision.clone()),
        (fx.org.id, fx.view.id, commit(2)),
    ] {
        assert!(
            analysis::load_scip_import(&mut conn, organization, view, id, &expected)
                .await
                .unwrap()
                .is_none()
        );
    }
    assert!(matches!(
        analysis::stage_scip_import(
            &mut conn,
            other.org.id,
            fx.view.id,
            &revision,
            &artifact,
            &payload
        )
        .await,
        Err(StoreError::NotFound { .. })
    ));
    assert!(matches!(
        analysis::stage_scip_import(
            &mut conn,
            fx.org.id,
            fx.view.id,
            &revision,
            &artifact,
            &json!({"format_version":2})
        )
        .await,
        Err(StoreError::Conflict { .. })
    ));
    let another = analysis::stage_scip_import(
        &mut conn,
        fx.org.id,
        fx.view.id,
        &revision,
        &ContentHash::of(b"another artifact"),
        &payload,
    )
    .await
    .unwrap();
    assert_ne!(id, another);
    // Even a direct update cannot rewrite an artifact already selected by a job.
    assert!(
        sqlx::query("UPDATE scip_import SET payload = '{}' WHERE id = $1")
            .bind(id)
            .execute(&mut *conn)
            .await
            .is_err()
    );
    assert_eq!(
        Some(payload),
        analysis::load_scip_import(&mut conn, fx.org.id, fx.view.id, id, &revision)
            .await
            .unwrap()
    );
    for invalid in ["abc", &"A".repeat(40), &"g".repeat(40)] {
        assert!(matches!(
            analysis::load_scip_import(&mut conn, fx.org.id, fx.view.id, id, invalid).await,
            Err(StoreError::InvalidInput(_))
        ));
    }
    let revision_64 = "a".repeat(64);
    assert!(
        analysis::stage_scip_import(
            &mut conn,
            fx.org.id,
            fx.view.id,
            &revision_64,
            &artifact,
            &json!({})
        )
        .await
        .is_ok()
    );
}

#[tokio::test]
async fn staging_participates_in_the_callers_transaction() {
    let db = require_db!();
    let mut conn = db.conn().await;
    let fx = fixture(&mut conn, "analysis-rollback").await;
    let revision = commit(1);
    let mut tx = conn.begin().await.unwrap();
    let id = analysis::stage_scip_import(
        &mut tx,
        fx.org.id,
        fx.view.id,
        &revision,
        &ContentHash::of(b"rollback artifact"),
        &json!({}),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    assert!(
        analysis::load_scip_import(&mut conn, fx.org.id, fx.view.id, id, &revision)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn coverage_is_exact_file_tenant_generation_and_building_only() {
    let db = require_db!();
    let mut conn = db.conn().await;
    let fx = fixture(&mut conn, "analysis-coverage").await;
    let other = fixture(&mut conn, "analysis-coverage-other").await;
    let (pin, hash) = source(&mut conn, &fx, "fn entry() {}\n").await;
    let file = path("src/lib.rs");
    let syntax = json!({"calls_complete":false,"unresolved":1});
    let scip = json!({"calls_complete":false,"calls_written":1});
    for (provider, details) in [("syntax", &syntax), ("scip", &scip)] {
        analysis::upsert_coverage(&mut conn, fx.org.id, pin, &file, &hash, provider, details)
            .await
            .unwrap();
    }
    let found = analysis::coverage_at(&mut conn, fx.org.id, pin, &file, &hash)
        .await
        .unwrap();
    assert_eq!(
        found
            .iter()
            .map(|item| item.provider.as_str())
            .collect::<Vec<_>>(),
        ["scip", "syntax"]
    );
    assert_eq!(found[0].details, scip);
    assert_eq!(found[1].details, syntax);
    assert!(
        analysis::coverage_at(&mut conn, other.org.id, pin, &file, &hash)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        analysis::coverage_at(
            &mut conn,
            fx.org.id,
            pin,
            &file,
            &ContentHash::of(b"different file")
        )
        .await
        .unwrap()
        .is_empty()
    );
    assert!(
        analysis::coverage_at(&mut conn, fx.org.id, pin, &path("src/absent.rs"), &hash)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        analysis::upsert_coverage(
            &mut conn,
            other.org.id,
            pin,
            &file,
            &hash,
            "syntax",
            &json!({})
        )
        .await,
        Err(StoreError::NotFound { .. })
    ));
    assert!(matches!(
        analysis::upsert_coverage(
            &mut conn,
            fx.org.id,
            pin,
            &file,
            &ContentHash::of(b"wrong"),
            "syntax",
            &json!({})
        )
        .await,
        Err(StoreError::InvalidInput(_))
    ));
    for (provider, details) in [
        ("unknown", json!({})),
        ("syntax", json!([])),
        ("syntax", json!({"large":"x".repeat(MAX_COVERAGE_BYTES)})),
    ] {
        assert!(matches!(
            analysis::upsert_coverage(&mut conn, fx.org.id, pin, &file, &hash, provider, &details)
                .await,
            Err(StoreError::InvalidInput(_))
        ));
    }
    views::activate_generation(&mut conn, pin.view, pin.generation)
        .await
        .unwrap();
    assert!(matches!(
        analysis::upsert_coverage(
            &mut conn,
            fx.org.id,
            pin,
            &file,
            &hash,
            "syntax",
            &json!({})
        )
        .await,
        Err(StoreError::GenerationNotBuilding { .. })
    ));
    assert_eq!(
        analysis::coverage_at(&mut conn, fx.org.id, pin, &file, &hash)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn coverage_does_not_inherit_and_failed_generation_coverage_is_hidden_and_pruned() {
    let db = require_db!();
    let mut conn = db.conn().await;
    let fx = fixture(&mut conn, "analysis-freshness").await;
    let file = path("src/lib.rs");
    let (first, hash) = source(&mut conn, &fx, "fn unchanged() {}\n").await;
    analysis::upsert_coverage(
        &mut conn,
        fx.org.id,
        first,
        &file,
        &hash,
        "scip",
        &json!({"compiler":"synthetic-v1"}),
    )
    .await
    .unwrap();
    views::activate_generation(&mut conn, first.view, first.generation)
        .await
        .unwrap();
    let next = GenerationPin {
        view: first.view,
        generation: views::begin_generation(&mut conn, first.view, Some(&commit(2)))
            .await
            .unwrap(),
    };
    assert!(
        analysis::coverage_at(&mut conn, fx.org.id, next, &file, &hash)
            .await
            .unwrap()
            .is_empty()
    );
    analysis::upsert_coverage(
        &mut conn,
        fx.org.id,
        next,
        &file,
        &hash,
        "syntax",
        &json!({}),
    )
    .await
    .unwrap();
    views::fail_generation(
        &mut conn,
        next.view,
        next.generation,
        "synthetic analysis cancelled",
    )
    .await
    .unwrap();
    assert!(
        analysis::coverage_at(&mut conn, fx.org.id, next, &file, &hash)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        analysis::coverage_at(&mut conn, fx.org.id, first, &file, &hash)
            .await
            .unwrap()
            .len(),
        1
    );
    let third = GenerationPin {
        view: first.view,
        generation: views::begin_generation(&mut conn, first.view, Some(&commit(3)))
            .await
            .unwrap(),
    };
    assert!(
        analysis::coverage_at(&mut conn, fx.org.id, third, &file, &hash)
            .await
            .unwrap()
            .is_empty()
    );
    views::activate_generation(&mut conn, third.view, third.generation)
        .await
        .unwrap();
    views::prune_history(&mut conn, third.view, third.generation)
        .await
        .unwrap();
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM analysis_coverage WHERE view_id = $1")
            .bind(first.view)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(remaining, 0);
}

fn edge(fx: &Fixture, origin: &str) -> NewEdge {
    NewEdge {
        from: NodeRef::File {
            project: fx.project.id,
            path: path("src/lib.rs"),
        },
        to: NodeRef::Project(fx.project.id),
        kind: "references".into(),
        evidence_type: EvidenceType::SemanticResolved,
        resolution: Resolution::Resolved,
        evidence: json!({"path":"src/lib.rs"}),
        origin: origin.into(),
    }
}

#[tokio::test]
async fn compiler_occurrences_and_edges_are_separate_retryable_and_rollback_with_generation() {
    let db = require_db!();
    let mut conn = db.conn().await;
    let fx = fixture(&mut conn, "analysis-products").await;
    let other = fixture(&mut conn, "analysis-products-foreign").await;
    let (first, hash) = source(&mut conn, &fx, "fn entry() {}\nentry();\n").await;
    let file = path("src/lib.rs");
    let symbol = symbols::upsert_symbols(
        &mut conn,
        fx.project.id,
        &[NewSymbol {
            qualified_name: "entry".into(),
            kind: "function".into(),
        }],
    )
    .await
    .unwrap()[0];
    let occurrence = NewOccurrence {
        symbol,
        path: file.clone(),
        content_hash: hash,
        lines: LineRange::new(2, 2).unwrap(),
        role: OccurrenceRole::Reference,
    };
    let syntax_occurrence = NewOccurrence {
        lines: LineRange::new(1, 1).unwrap(),
        role: OccurrenceRole::Definition,
        ..occurrence.clone()
    };
    analysis::replace_scip_occurrences(
        &mut conn,
        fx.org.id,
        first,
        "scip:src/lib.rs",
        std::slice::from_ref(&occurrence),
    )
    .await
    .unwrap();
    symbols::replace_occurrences(
        &mut conn,
        first.view,
        first.generation,
        std::slice::from_ref(&file),
        std::slice::from_ref(&syntax_occurrence),
    )
    .await
    .unwrap();
    analysis::replace_scip_occurrences(
        &mut conn,
        fx.org.id,
        first,
        "scip:src/lib.rs",
        std::slice::from_ref(&occurrence),
    )
    .await
    .unwrap();
    let found = symbols::occurrences_in_file(&mut conn, first, &file)
        .await
        .unwrap();
    assert_eq!(found.len(), 2);
    assert!(found.iter().any(|item| item.origin == "syntax"));
    assert!(found.iter().any(|item| item.origin == "scip:src/lib.rs"));
    let other_symbol = symbols::upsert_symbols(
        &mut conn,
        other.project.id,
        &[NewSymbol {
            qualified_name: "foreign".into(),
            kind: "function".into(),
        }],
    )
    .await
    .unwrap()[0];
    assert!(matches!(
        analysis::replace_scip_occurrences(
            &mut conn,
            fx.org.id,
            first,
            "scip:src/lib.rs",
            &[
                occurrence.clone(),
                NewOccurrence {
                    symbol: other_symbol,
                    ..occurrence.clone()
                }
            ]
        )
        .await,
        Err(StoreError::InvalidInput(_))
    ));
    assert_eq!(
        symbols::occurrences_in_file(&mut conn, first, &file)
            .await
            .unwrap()
            .len(),
        2
    );
    graph::replace_edges(
        &mut conn,
        first.view,
        first.generation,
        &["src/lib.rs".into(), "scip:src/lib.rs".into()],
        &[edge(&fx, "src/lib.rs"), edge(&fx, "scip:src/lib.rs")],
    )
    .await
    .unwrap();
    analysis::upsert_coverage(
        &mut conn,
        fx.org.id,
        first,
        &file,
        &hash,
        "scip",
        &json!({"references_written":1}),
    )
    .await
    .unwrap();
    views::activate_generation(&mut conn, first.view, first.generation)
        .await
        .unwrap();
    assert!(matches!(
        analysis::clear_scip_edges(&mut conn, fx.org.id, first).await,
        Err(StoreError::GenerationNotBuilding { .. })
    ));
    let next = GenerationPin {
        view: first.view,
        generation: views::begin_generation(&mut conn, first.view, Some(&commit(2)))
            .await
            .unwrap(),
    };
    assert!(matches!(
        analysis::clear_scip_edges(&mut conn, other.org.id, next).await,
        Err(StoreError::NotFound { .. })
    ));
    assert_eq!(
        analysis::clear_scip_edges(&mut conn, fx.org.id, next)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        analysis::clear_scip_edges(&mut conn, fx.org.id, next)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        symbols::occurrences_in_file(&mut conn, next, &file)
            .await
            .unwrap()
            .len(),
        1
    );
    analysis::replace_scip_occurrences(
        &mut conn,
        fx.org.id,
        next,
        "scip:src/lib.rs",
        &[occurrence],
    )
    .await
    .unwrap();
    graph::replace_edges(
        &mut conn,
        next.view,
        next.generation,
        &["scip:src/lib.rs".into()],
        &[edge(&fx, "scip:src/lib.rs")],
    )
    .await
    .unwrap();
    analysis::upsert_coverage(
        &mut conn,
        fx.org.id,
        next,
        &file,
        &hash,
        "scip",
        &json!({"references_written":1}),
    )
    .await
    .unwrap();
    analysis::upsert_coverage(
        &mut conn,
        fx.org.id,
        next,
        &file,
        &hash,
        "syntax",
        &json!({"calls_complete":false}),
    )
    .await
    .unwrap();
    let mut tx = conn.begin().await.unwrap();
    assert_eq!(
        analysis::clear_scip_edges(&mut tx, fx.org.id, next)
            .await
            .unwrap(),
        1
    );
    let during_clear = analysis::coverage_at(&mut tx, fx.org.id, next, &file, &hash)
        .await
        .unwrap();
    assert_eq!(during_clear.len(), 1);
    assert_eq!(during_clear[0].provider, "syntax");
    assert_eq!(
        analysis::coverage_at(&mut tx, fx.org.id, first, &file, &hash)
            .await
            .unwrap()
            .len(),
        1
    );
    tx.rollback().await.unwrap();
    assert_eq!(
        analysis::coverage_at(&mut conn, fx.org.id, next, &file, &hash)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        analysis::clear_scip_edges(&mut conn, fx.org.id, next)
            .await
            .unwrap(),
        1
    );
    let after_clear = analysis::coverage_at(&mut conn, fx.org.id, next, &file, &hash)
        .await
        .unwrap();
    assert_eq!(after_clear.len(), 1);
    assert_eq!(after_clear[0].provider, "syntax");
    assert_eq!(
        analysis::coverage_at(&mut conn, fx.org.id, first, &file, &hash)
            .await
            .unwrap()
            .len(),
        1
    );
    views::fail_generation(
        &mut conn,
        next.view,
        next.generation,
        "synthetic compiler analysis failed",
    )
    .await
    .unwrap();
    assert_eq!(
        symbols::occurrences_in_file(&mut conn, first, &file)
            .await
            .unwrap()
            .len(),
        2
    );
    let node = NodeRef::Project(fx.project.id);
    assert_eq!(
        graph::edges_at(&mut conn, &node, &[first])
            .await
            .unwrap()
            .len(),
        2
    );
    let third = GenerationPin {
        view: first.view,
        generation: views::begin_generation(&mut conn, first.view, Some(&commit(3)))
            .await
            .unwrap(),
    };
    analysis::clear_scip_edges(&mut conn, fx.org.id, third)
        .await
        .unwrap();
    views::activate_generation(&mut conn, third.view, third.generation)
        .await
        .unwrap();
    assert_eq!(
        graph::edges_at(&mut conn, &node, &[third])
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        symbols::occurrences_in_file(&mut conn, third, &file)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        symbols::occurrences_in_file(&mut conn, first, &file)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        analysis::coverage_at(&mut conn, fx.org.id, first, &file, &hash)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn occurrence_pages_are_bounded_deterministic_and_tenant_scoped() {
    let db = require_db!();
    let mut conn = db.conn().await;
    let fx = fixture(&mut conn, "occurrence-pages").await;
    let foreign = fixture(&mut conn, "occurrence-pages-foreign").await;
    let (pin, hash) = source(&mut conn, &fx, "fn entry() {}\nentry();\n").await;
    let (foreign_pin, foreign_hash) = source(&mut conn, &foreign, "fn foreign() {}\n").await;
    let ids = symbols::upsert_symbols(
        &mut conn,
        fx.project.id,
        &[
            NewSymbol {
                qualified_name: "entry".into(),
                kind: "function".into(),
            },
            NewSymbol {
                qualified_name: "other".into(),
                kind: "function".into(),
            },
        ],
    )
    .await
    .unwrap();
    let symbol = ids[0];
    let other = ids[1];
    let foreign_symbol = symbols::upsert_symbols(
        &mut conn,
        foreign.project.id,
        &[NewSymbol {
            qualified_name: "foreign".into(),
            kind: "function".into(),
        }],
    )
    .await
    .unwrap()[0];
    let mut occurrences = Vec::new();
    for (file, line, role) in [
        ("src/z.rs", 2, OccurrenceRole::Reference),
        ("src/a.rs", 2, OccurrenceRole::Reference),
        ("src/a.rs", 1, OccurrenceRole::Reference),
        ("src/a.rs", 1, OccurrenceRole::Definition),
        ("src/a.rs", 1, OccurrenceRole::Reference),
        ("src/ö.rs", 1, OccurrenceRole::Reference),
    ] {
        occurrences.push(NewOccurrence {
            symbol,
            path: path(file),
            content_hash: hash,
            lines: LineRange::new(line, line).unwrap(),
            role,
        });
    }
    let paths = vec![path("src/z.rs"), path("src/a.rs"), path("src/ö.rs")];
    symbols::replace_occurrences(&mut conn, pin.view, pin.generation, &paths, &occurrences)
        .await
        .unwrap();
    let foreign_occurrence = NewOccurrence {
        symbol: foreign_symbol,
        path: path("src/lib.rs"),
        content_hash: foreign_hash,
        lines: LineRange::new(1, 1).unwrap(),
        role: OccurrenceRole::Definition,
    };
    symbols::replace_occurrences(
        &mut conn,
        foreign_pin.view,
        foreign_pin.generation,
        &[path("src/lib.rs")],
        &[foreign_occurrence],
    )
    .await
    .unwrap();
    let second_view = views::create_view(
        &mut conn,
        fx.project.id,
        &"branch:secondary".parse().unwrap(),
    )
    .await
    .unwrap();
    let second_pin = GenerationPin {
        view: second_view.id,
        generation: views::begin_generation(&mut conn, second_view.id, Some(&commit(1)))
            .await
            .unwrap(),
    };
    symbols::replace_occurrences(
        &mut conn,
        second_pin.view,
        second_pin.generation,
        &[path("src/lib.rs")],
        &[NewOccurrence {
            symbol,
            path: path("src/lib.rs"),
            content_hash: hash,
            lines: LineRange::new(1, 1).unwrap(),
            role: OccurrenceRole::Reference,
        }],
    )
    .await
    .unwrap();
    views::activate_generation(&mut conn, pin.view, pin.generation)
        .await
        .unwrap();
    views::activate_generation(&mut conn, foreign_pin.view, foreign_pin.generation)
        .await
        .unwrap();
    views::activate_generation(&mut conn, second_pin.view, second_pin.generation)
        .await
        .unwrap();
    let expected = symbols::occurrences_of(&mut conn, symbol, &[pin, second_pin])
        .await
        .unwrap();
    let mut collected = Vec::new();
    let mut after = None;
    loop {
        let page = symbols::occurrences_of_page(
            &mut conn,
            fx.org.id,
            symbol,
            &[second_pin, pin, pin],
            after,
            2,
        )
        .await
        .unwrap();
        assert!(page.len() <= 2);
        if page.is_empty() {
            break;
        }
        after = page.last().map(|row| row.id);
        collected.extend(page);
        assert!(collected.len() <= expected.len());
    }
    assert_eq!(collected, expected);
    assert!(
        symbols::occurrences_of_page(&mut conn, foreign.org.id, symbol, &[pin], None, 2)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        symbols::occurrences_of_page(
            &mut conn,
            fx.org.id,
            foreign_symbol,
            &[foreign_pin],
            None,
            2
        )
        .await
        .unwrap()
        .is_empty()
    );
    let foreign_cursor = symbols::occurrences_of(&mut conn, foreign_symbol, &[foreign_pin])
        .await
        .unwrap()[0]
        .id;
    for (requested_symbol, cursor) in [
        (symbol, foreign_cursor),
        (other, expected[0].id),
        (symbol, knowell_store::OccurrenceId(uuid::Uuid::nil())),
    ] {
        assert!(matches!(
            symbols::occurrences_of_page(
                &mut conn,
                fx.org.id,
                requested_symbol,
                &[pin],
                Some(cursor),
                2
            )
            .await,
            Err(StoreError::InvalidInput(_))
        ));
    }
    for limit in [0, symbols::MAX_OCCURRENCES_PAGE_ROWS + 1] {
        assert!(matches!(
            symbols::occurrences_of_page(&mut conn, fx.org.id, symbol, &[pin], None, limit).await,
            Err(StoreError::InvalidInput(_))
        ));
    }
    assert!(
        symbols::occurrences_of_page(&mut conn, fx.org.id, symbol, &[], None, 2)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        symbols::occurrences_of_page(&mut conn, fx.org.id, symbol, &[], Some(expected[0].id), 2)
            .await,
        Err(StoreError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn occurrence_cursors_cannot_select_a_different_historical_generation() {
    let db = require_db!();
    let mut conn = db.conn().await;
    let fx = fixture(&mut conn, "occurrence-history").await;
    let (first, hash) = source(&mut conn, &fx, "fn entry() {}\nentry();\n").await;
    let file = path("src/lib.rs");
    let symbol = symbols::upsert_symbols(
        &mut conn,
        fx.project.id,
        &[NewSymbol {
            qualified_name: "entry".into(),
            kind: "function".into(),
        }],
    )
    .await
    .unwrap()[0];
    let occurrence = NewOccurrence {
        symbol,
        path: file.clone(),
        content_hash: hash,
        lines: LineRange::new(1, 1).unwrap(),
        role: OccurrenceRole::Reference,
    };
    symbols::replace_occurrences(
        &mut conn,
        first.view,
        first.generation,
        std::slice::from_ref(&file),
        std::slice::from_ref(&occurrence),
    )
    .await
    .unwrap();
    views::activate_generation(&mut conn, first.view, first.generation)
        .await
        .unwrap();
    let old_cursor = symbols::occurrences_of_page(&mut conn, fx.org.id, symbol, &[first], None, 1)
        .await
        .unwrap()[0]
        .id;
    let next = GenerationPin {
        view: first.view,
        generation: views::begin_generation(&mut conn, first.view, Some(&commit(2)))
            .await
            .unwrap(),
    };
    symbols::replace_occurrences(
        &mut conn,
        next.view,
        next.generation,
        std::slice::from_ref(&file),
        &[NewOccurrence {
            lines: LineRange::new(2, 2).unwrap(),
            ..occurrence
        }],
    )
    .await
    .unwrap();
    views::activate_generation(&mut conn, next.view, next.generation)
        .await
        .unwrap();
    assert!(matches!(
        symbols::occurrences_of_page(&mut conn, fx.org.id, symbol, &[next], Some(old_cursor), 1)
            .await,
        Err(StoreError::InvalidInput(_))
    ));
    assert!(
        symbols::occurrences_of_page(&mut conn, fx.org.id, symbol, &[first], Some(old_cursor), 1)
            .await
            .unwrap()
            .is_empty()
    );
    let new_cursor = symbols::occurrences_of_page(&mut conn, fx.org.id, symbol, &[next], None, 1)
        .await
        .unwrap()[0]
        .id;
    assert!(matches!(
        symbols::occurrences_of_page(&mut conn, fx.org.id, symbol, &[first], Some(new_cursor), 1)
            .await,
        Err(StoreError::InvalidInput(_))
    ));
    assert!(matches!(
        symbols::occurrences_of_page(&mut conn, fx.org.id, symbol, &[first, next], None, 1).await,
        Err(StoreError::InvalidInput(_))
    ));
    let failed = GenerationPin {
        view: first.view,
        generation: views::begin_generation(&mut conn, first.view, Some(&commit(3)))
            .await
            .unwrap(),
    };
    views::fail_generation(
        &mut conn,
        failed.view,
        failed.generation,
        "synthetic pagination failed",
    )
    .await
    .unwrap();
    assert!(
        symbols::occurrences_of_page(&mut conn, fx.org.id, symbol, &[failed], None, 1)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        symbols::occurrences_of_page(&mut conn, fx.org.id, symbol, &[failed], Some(new_cursor), 1)
            .await,
        Err(StoreError::InvalidInput(_))
    ));
}
